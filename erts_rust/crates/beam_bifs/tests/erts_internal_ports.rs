// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The `erts_internal` port quartet — the **C spellings** of the port operations —
//! driven through [`BifRegistry::call`] over a mock [`PortHook`], with the whole
//! matrix re-derived against the live oracle.
//!
//! `bif.tab` declares only `erts_internal:port_close/1`, `port_info/1,2` and
//! `open_port/2`; `erlang:port_close/1`, `erlang:port_info/1,2` and
//! `erlang:open_port/2` are `erlang.erl` **wrappers** over them
//! (`erts/preloaded/src/erlang.erl:8050`, `:12170`, `:12322`, `:12376`) that consume the
//! C's value-shaped answers and re-raise them:
//!
//! ```text
//! port_close(Port)     -> case erts_internal:port_close(Port)  of true -> true;  Error -> error_with_info(…)
//! port_info(Port)      -> case erts_internal:port_info(Port)   of badarg -> badarg_with_info(…); Res -> Res
//! port_info(Port,Item) -> case erts_internal:port_info(P,Item) of badtype|badarg -> badarg…; Res -> Res
//! open_port(Name,Opts) -> case erts_internal:open_port(…),  Ref when is_reference → receive {Ref,Res} → Res
//! ```
//!
//! So the `erts_internal:` spellings **return status atoms** (`true`/`badarg`/
//! `badtype`/`undefined`) where the `erlang:` spellings raise `error:badarg`, and
//! `port_info/1` says `badarg` for a non-port where `port_info/2` says `badtype`.
//! Both facts are pinned below.
//!
//! # The rows
//!
//! Every value is pinned against `erl -noshell` (OTP 29.1.1 / erts-17.1), and
//! [`the_pinned_matrix_agrees_with_the_live_oracle`] re-derives the whole matrix live
//! — including the recorded **capability-gap** rows, whose oracle answer it re-checks
//! while the Rust side answers the typed `unsupported_bif`. It skips **loudly** (and
//! still passes) when no usable `erl` is on `$PATH`, and kills a wedged oracle rather
//! than hanging the suite (the `tests/erl_ddll.rs` convention).
//!
//! # The mock hook reproduces the *runtime's* port table
//!
//! `MockPorts` models what `beam_rt`'s `RuntimePorts` does: a live port answers its
//! pair list, a port the table no longer holds answers an **empty** list (which the
//! `erts_internal:` spellings render as `undefined`, exactly as the C's `!prt` arm
//! does), and closing a port removes it (`badarg` on a second close). The pair order is
//! `RuntimePorts::info`'s own — the seven `/1` roster items, the `/2`-only extras, then
//! `registered_name` **last** — so the roster split (never the hook) owns the answer's
//! order.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::{default_registry, needs_port_hook, BifContext, BifRegistry, PortHook};
use beam_codec::term::{cons_head, cons_tail};
use beam_common::{BeamError, ExitReason, Result};
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, AtomTable, Eterm, NIL};

/// The number of the live, **unregistered** port the mock table holds.
const PLAIN: u64 = 7;
/// The number of the live port registered as `myp`.
const NAMED: u64 = 8;
/// The name the [`NAMED`] port is registered under.
const PORT_NAME: &str = "myp";
/// The value of the `name` item — the command line the port was opened with.
const COMMAND: &str = "sleep 5";

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Seeds the atoms the matrix and the status answers need — the runtime's own boot
/// seed holds every one of them.
const SEEDED_ATOMS: [&str; 33] = [
    "true",
    "false",
    "ok",
    "undefined",
    "badarg",
    "badtype",
    "name",
    "links",
    "id",
    "connected",
    "input",
    "output",
    "os_pid",
    "monitors",
    "queue_size",
    "memory",
    "monitored_by",
    "locking",
    "parallelism",
    "registered_name",
    "myp",
    "no_such_name",
    "no_such_item",
    "spawn",
    "cat",
    "sleep 5",
    "gone",
    "error",
    "badarith",
    "badfun",
    "badmatch",
    "normal",
    "killed",
];

/// The crate under test plus the heap and atom table it is driven over.
struct Fixture {
    atoms: AtomTable,
    heap: Heap,
    reg: BifRegistry,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(SEEDED_ATOMS);
        Fixture {
            atoms,
            heap: Heap::new(1 << 16).expect("test heap"),
            reg: default_registry(),
        }
    }

    /// Runs `f` over a fresh [`BifContext`] on this fixture's heap (no seam installed).
    fn term<R>(&mut self, f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let Fixture { atoms, heap, .. } = self;
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// `M:F/A(Args)` with **no** host seam installed.
    fn call(&mut self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        let Fixture { atoms, heap, reg } = self;
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
        reg.call(module, function, arity, &mut ctx, args)
    }

    /// `M:F/A(Args)` with `hook` installed as the [`PortHook`].
    fn call_hooked(
        &mut self,
        hook: &mut dyn PortHook,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let Fixture { atoms, heap, reg } = self;
        let mut ctx =
            BifContext::new(heap, atoms, ProcessId::new(1, 0)).with_port_hook(hook);
        reg.call(module, function, arity, &mut ctx, args)
    }

    /// The pair list a **live** port reports, in `beam_rt`'s `RuntimePorts::info`
    /// emission order: the seven `/1` roster items, the six `/2`-only extras, then
    /// `registered_name` last when the port is registered.
    fn pairs(&mut self, registered: Option<&str>) -> Vec<(Eterm, Eterm)> {
        self.term(|ctx| {
            let zero = ctx.int(0).expect("fits");
            let mut pairs = vec![
                (ctx.atom("name").expect("seeded"), chars(ctx, COMMAND)),
                (ctx.atom("links").expect("seeded"), NIL),
                (ctx.atom("id").expect("seeded"), ctx.int(PLAIN as i64).expect("fits")),
                (ctx.atom("connected").expect("seeded"), NIL),
                (ctx.atom("input").expect("seeded"), zero),
                (ctx.atom("output").expect("seeded"), zero),
                (ctx.atom("os_pid").expect("seeded"), ctx.int(12).expect("fits")),
                // The `/2`-only extras `port_info/1` must drop.
                (ctx.atom("monitors").expect("seeded"), NIL),
                (ctx.atom("queue_size").expect("seeded"), zero),
                (ctx.atom("memory").expect("seeded"), ctx.int(48).expect("fits")),
                (ctx.atom("monitored_by").expect("seeded"), NIL),
                (ctx.atom("locking").expect("seeded"), ctx.atom("false").expect("seeded")),
                (
                    ctx.atom("parallelism").expect("seeded"),
                    ctx.atom("false").expect("seeded"),
                ),
            ];
            if let Some(name) = registered {
                let key = ctx.atom("registered_name").expect("seeded");
                let value = ctx.atom(name).expect("seeded");
                pairs.push((key, value));
            }
            pairs
        })
    }
}

/// The Erlang string `s` — a proper list of byte-sized small integers (the shape a
/// charlist item value and `io_lib:format/2`'s format argument take).
fn chars(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
    let elems: Vec<Eterm> = s
        .bytes()
        .map(|byte| ctx.int(i64::from(byte)).expect("a byte fits"))
        .collect();
    ctx.list(&elems).expect("fits")
}

// ---------------------------------------------------------------------------
// The mock port table
// ---------------------------------------------------------------------------

/// A [`PortHook`] test double modeling the runtime's port table: a live port answers
/// its pair list, a port the table does not hold answers **empty**, and a close
/// removes it.
struct MockPorts {
    /// The pair list per live port number, in `RuntimePorts::info`'s order.
    pairs: HashMap<u64, Vec<(Eterm, Eterm)>>,
    /// Every `open_port/2` this hook saw (it hands out port `99`).
    opened: Vec<(Eterm, Eterm)>,
    /// Every port this hook actually closed.
    closed: Vec<PortId>,
}

impl MockPorts {
    fn new(pairs: HashMap<u64, Vec<(Eterm, Eterm)>>) -> MockPorts {
        MockPorts {
            pairs,
            opened: Vec::new(),
            closed: Vec::new(),
        }
    }

    /// The standard table: [`PLAIN`] live and unregistered, [`NAMED`] live and
    /// registered as [`PORT_NAME`].
    fn standard(fx: &mut Fixture) -> MockPorts {
        let plain = fx.pairs(None);
        let named = fx.pairs(Some(PORT_NAME));
        MockPorts::new(HashMap::from([(PLAIN, plain), (NAMED, named)]))
    }
}

impl PortHook for MockPorts {
    fn open_port(&mut self, name: Eterm, options: Eterm) -> Result<PortId> {
        self.opened.push((name, options));
        Ok(PortId::new(99))
    }

    fn close_port(&mut self, port: PortId) -> Result<()> {
        if self.pairs.remove(&port.number()).is_some() {
            self.closed.push(port);
            Ok(())
        } else {
            // The C's `ERTS_PORT_OP_BADARG` arm: the runtime does not hold the port.
            Err(BeamError::BadArg)
        }
    }

    fn port_info(&mut self, port: PortId) -> Result<Vec<(Eterm, Eterm)>> {
        Ok(self.pairs.get(&port.number()).cloned().unwrap_or_default())
    }
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// The name of `term` when it is an atom.
fn atom_name<'a>(atoms: &'a AtomTable, term: Eterm) -> Option<&'a str> {
    if beam_term::is_atom(term) {
        atoms.name(atom_val(term))
    } else {
        None
    }
}

/// Asserts `answer` is the atom `name` **as a value** — the C's `BIF_RET(retval)`
/// status atom, never a raise.
fn assert_status(fx: &Fixture, answer: &Result<Eterm>, name: &str) {
    match answer {
        Ok(term) => assert_eq!(
            atom_name(&fx.atoms, *term),
            Some(name),
            "expected the atom `{name}` as a value"
        ),
        other => panic!("expected the atom `{name}` as a value, got a raise: {other:?}"),
    }
}

/// Asserts the call is the typed `unsupported_bif` capability gap.
fn assert_gap(answer: &Result<Eterm>) {
    match answer {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, beam_common::ExceptionClass::Error);
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"));
        }
        other => panic!("expected the typed unsupported_bif gap, got {other:?}"),
    }
}

/// Asserts `answer` raised the plain `badarg` (the `erlang:` spellings' wrapper
/// behaviour).
fn assert_badarg(answer: &Result<Eterm>) {
    assert!(
        matches!(answer, Err(BeamError::BadArg)),
        "expected error:badarg, got {answer:?}"
    );
}

/// Structural term equality (`==` on a boxed `Eterm` compares by address).
fn assert_eq_term(fx: &Fixture, got: Eterm, expected: Eterm, what: &str) {
    assert!(
        beam_term::compare::eq(got, expected, &fx.atoms),
        "{what}: term mismatch"
    );
}

/// Builds an expected term on a throwaway heap and compares it with `got` — two heaps
/// may be alive at once and `compare::eq` walks both.
fn assert_renders_as(fx: &Fixture, got: Eterm, what: &str, build: impl FnOnce(&mut BifContext<'_>) -> Eterm) {
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let mut ctx = BifContext::new(&mut heap, &fx.atoms, ProcessId::new(1, 0));
    let expected = build(&mut ctx);
    assert_eq_term(fx, got, expected, what);
}

/// The proper list `term` as its elements.
fn elems(term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut rest = term;
    while rest != NIL {
        assert!(beam_term::is_list(rest), "expected a proper list");
        out.push(cons_head(rest).expect("a cons head"));
        rest = cons_tail(rest).expect("a cons tail");
    }
    out
}

/// The item keys of a `{Item, Value}` pair list, as `[name,links,…]` — the text
/// `io:format("~w", [[element(1, X) || X <- L]])` prints, so a live-oracle probe can
/// compare the *order* without depending on environment-specific values.
fn item_keys(fx: &Fixture, term: Eterm) -> String {
    let mut names = Vec::new();
    for pair in elems(term) {
        let key = beam_codec::term::tuple_element(pair, 0).expect("a 2-tuple");
        names.push(atom_name(&fx.atoms, key).expect("an atom item").to_string());
    }
    format!("[{}]", names.join(","))
}

/// The item keys of a `{Item, Value}` pair list as the list of atoms `~w` prints
/// unquoted — `[name,links,…]` — so a key-order probe compares the same way `erl`'s
/// `io:format("~w~n", [[element(1, X) || X <- L]])` does (`~w` prints a charlist as
/// integers, so the keys must stay atoms, never a rendered string).
fn keys_term(fx: &mut Fixture, term: Eterm) -> Eterm {
    let keys: Vec<Eterm> = elems(term)
        .iter()
        .map(|pair| beam_codec::term::tuple_element(*pair, 0).expect("a 2-tuple"))
        .collect();
    fx.term(|ctx| ctx.list(&keys).expect("fits"))
}
/// The `~w` text of a term — rendered by the crate's own `io_lib:format/2`, i.e. the
/// text `erl`'s `io:format("~w~n", …)` prints.
fn w(fx: &mut Fixture, term: Eterm) -> String {
    let Fixture { atoms, heap, reg } = fx;
    let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
    let format = chars(&mut ctx, "~w");
    let args = ctx.list(&[term]).expect("fits");
    let text = reg
        .call("io_lib", "format", 2, &mut ctx, &[format, args])
        .expect("io_lib:format/2");
    deep_charlist(text)
}

/// Flattens a (possibly nested) charlist into a `String`.
fn deep_charlist(term: Eterm) -> String {
    let mut out = String::new();
    let mut stack = vec![term];
    while let Some(term) = stack.pop() {
        if term == NIL {
            continue;
        }
        if beam_term::is_list(term) {
            stack.push(cons_tail(term).expect("a cons tail"));
            stack.push(cons_head(term).expect("a cons head"));
            continue;
        }
        let byte = u8::try_from(beam_term::signed_value(term)).expect("a charlist byte");
        out.push(char::from(byte));
    }
    out
}

/// The answer as text: a value renders through the crate's `~w`, a `badarg` raise as
/// the oracle's `{error,badarg}`, and a typed capability gap as `{gap,Atom}` — a text
/// the oracle never produces, so a gap row can never masquerade as an agreement.
fn answer_text(fx: &mut Fixture, answer: Result<Eterm>) -> String {
    match answer {
        Ok(term) => w(fx, term),
        Err(BeamError::BadArg) => "{error,badarg}".to_string(),
        Err(BeamError::Raise(raise)) => match raise.reason.as_atom() {
            Some("unsupported_bif") => "{gap,unsupported_bif}".to_string(),
            Some(atom) => format!("{{error,{atom}}}"),
            None => "{error,structured}".to_string(),
        },
        Err(other) => format!("{{error,{other:?}}}"),
    }
}

// ---------------------------------------------------------------------------
// The pinned matrix
// ---------------------------------------------------------------------------

/// One matrix row: a label, the `erl -noshell -eval` statement that prints the
/// oracle's `~w` answer, this runtime's answer text, and the oracle's own text.
///
/// `oracle == rust` for every **agreeing** row; the rows where it differs are the
/// recorded capability gaps (the atom-name forms and `open_port/2`), whose oracle
/// answer is pinned here rather than hidden.
struct Probe {
    label: &'static str,
    erl: String,
    rust: String,
    oracle: String,
}

/// Runs one agreeing row: a fresh fixture, the standard live-port table, and `call`'s
/// answer, whose text must be `expected`.
fn row(
    label: &'static str,
    erl: &str,
    expected: &str,
    call: impl FnOnce(&mut Fixture, &mut MockPorts) -> Result<Eterm>,
) -> Probe {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let answer = call(&mut fx, &mut ports);
    let rust = answer_text(&mut fx, answer);
    assert_eq!(rust, expected, "crate side of `{label}`");
    Probe {
        label,
        erl: erl.to_string(),
        rust: rust.clone(),
        oracle: rust,
    }
}

/// Runs one recorded **capability gap**: the Rust side must be the typed
/// `unsupported_bif` and `oracle` is what `erl` answers for the same probe.
fn gap_row(
    label: &'static str,
    erl: &str,
    oracle: &str,
    call: impl FnOnce(&mut Fixture, &mut MockPorts) -> Result<Eterm>,
) -> Probe {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let answer = call(&mut fx, &mut ports);
    assert_gap(&answer);
    let rust = answer_text(&mut fx, answer);
    Probe {
        label,
        erl: erl.to_string(),
        rust,
        oracle: oracle.to_string(),
    }
}

/// The whole matrix. Each `erl` statement is self-contained (it opens what it needs),
/// so the live oracle can run all of them in one VM.
fn matrix() -> Vec<Probe> {
    let mut probes = Vec::new();

    // ---- erts_internal:port_close/1 — status atoms as values ----
    probes.push(row(
        "port_close/1 of a live port is `true`",
        "P = open_port({spawn,\"sleep 5\"},[]), io:format(\"~w~n\",[erts_internal:port_close(P)])",
        "true",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let answer = fx.call_hooked(ports, "erts_internal", "port_close", 1, &[plain]);
            assert_eq!(ports.closed, vec![PortId::new(PLAIN)], "the hook closed it");
            answer
        },
    ));
    probes.push(row(
        "port_close/1 twice is `badarg` the second time",
        "P = open_port({spawn,\"sleep 5\"},[]), true = erts_internal:port_close(P), \
         io:format(\"~w~n\",[erts_internal:port_close(P)])",
        "badarg",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let first = fx.call_hooked(ports, "erts_internal", "port_close", 1, &[plain]);
            assert_status(fx, &first, "true");
            fx.call_hooked(ports, "erts_internal", "port_close", 1, &[plain])
        },
    ));
    probes.push(row(
        "port_close/1 of an integer is the `badarg` *value*",
        "io:format(\"~w~n\",[erts_internal:port_close(1)])",
        "badarg",
        |fx, ports| {
            let one = fx.term(|ctx| ctx.int(1).expect("fits"));
            let answer = fx.call_hooked(ports, "erts_internal", "port_close", 1, &[one]);
            assert!(ports.closed.is_empty(), "the seam was never consulted");
            answer
        },
    ));
    probes.push(row(
        "port_close/1 of a pid is the `badarg` value",
        "io:format(\"~w~n\",[erts_internal:port_close(self())])",
        "badarg",
        |fx, ports| {
            let pid = ProcessId::new(1, 0).to_eterm();
            fx.call_hooked(ports, "erts_internal", "port_close", 1, &[pid])
        },
    ));
    probes.push(row(
        "port_close/1 of a list is the `badarg` value",
        "io:format(\"~w~n\",[erts_internal:port_close([])])",
        "badarg",
        |fx, ports| fx.call_hooked(ports, "erts_internal", "port_close", 1, &[NIL]),
    ));

    // ---- erts_internal:port_info/1 ----
    probes.push(row(
        "port_info/1 of a live unregistered port is the C's 7-item roster",
        "P = open_port({spawn,\"sleep 5\"},[]), io:format(\"~w~n\",\
         [[element(1,X) || X <- erts_internal:port_info(P)]])",
        "[name,links,id,connected,input,output,os_pid]",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let answer = fx.call_hooked(ports, "erts_internal", "port_info", 1, &[plain])
                .expect("port_info/1");
            let keys = item_keys(fx, answer);
            assert_eq!(keys, "[name,links,id,connected,input,output,os_pid]");
            Ok(keys_term(fx, answer))
        },
    ));
    probes.push(row(
        "port_info/1 of a registered port leads with `registered_name`",
        "P = open_port({spawn,\"sleep 5\"},[]), register(myp,P), io:format(\"~w~n\",\
         [[element(1,X) || X <- erts_internal:port_info(P)]])",
        "[registered_name,name,links,id,connected,input,output,os_pid]",
        |fx, ports| {
            let named = PortId::new(NAMED).to_eterm();
            let answer = fx.call_hooked(ports, "erts_internal", "port_info", 1, &[named])
                .expect("port_info/1");
            let keys = item_keys(fx, answer);
            assert_eq!(
                keys,
                "[registered_name,name,links,id,connected,input,output,os_pid]"
            );
            Ok(keys_term(fx, answer))
        },
    ));
    probes.push(row(
        "port_info/1 of a non-port is `badarg`",
        "io:format(\"~w~n\",[erts_internal:port_info(1)])",
        "badarg",
        |fx, ports| {
            let one = fx.term(|ctx| ctx.int(1).expect("fits"));
            fx.call_hooked(ports, "erts_internal", "port_info", 1, &[one])
        },
    ));
    probes.push(row(
        "port_info/1 of a port the runtime dropped is `undefined`",
        "P = open_port({spawn,\"sleep 5\"},[]), true = erts_internal:port_close(P), \
         io:format(\"~w~n\",[erts_internal:port_info(P)])",
        "undefined",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let closed = fx.call_hooked(ports, "erts_internal", "port_close", 1, &[plain]);
            assert_status(fx, &closed, "true");
            fx.call_hooked(ports, "erts_internal", "port_info", 1, &[plain])
        },
    ));

    // ---- erts_internal:port_info/2 ----
    probes.push(row(
        "port_info/2 returns the `{Item, Value}` pair",
        // The value is the command line the port was opened with — compared by its
        // byte size so the row's text is environment-independent and quotes-free (the
        // crate's `~w` prints a charlist as integers, so a quoting expectation could
        // not be shared between the two sides).
        "P = open_port({spawn,\"sleep 5\"},[]), {name,C} = \
         erts_internal:port_info(P,name), \
         io:format(\"~w~n\",[{name,byte_size(C)}])",
        "{name,7}",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));
            let answer = fx
                .call_hooked(ports, "erts_internal", "port_info", 2, &[plain, name])
                .expect("port_info/2");
            let key = beam_codec::term::tuple_element(answer, 0).expect("a 2-tuple");
            let value = beam_codec::term::tuple_element(answer, 1).expect("a 2-tuple");
            assert_eq!(atom_name(&fx.atoms, key), Some("name"));
            let command = fx.term(|ctx| chars(ctx, COMMAND));
            assert_eq_term(fx, value, command, "the `name` item's value");
            let seven = fx.term(|ctx| ctx.int(7).expect("fits"));
            Ok(fx.term(|ctx| ctx.tuple(&[name, seven]).expect("fits")))
        },
    ));
    probes.push(row(
        "port_info/2 reaches a /2-only item (`memory`)",
        "P = open_port({spawn,\"sleep 5\"},[]), io:format(\"~w~n\",\
         [[element(1,erts_internal:port_info(P,memory))]])",
        "[memory]",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let memory = fx.term(|ctx| ctx.atom("memory").expect("seeded"));
            let answer = fx
                .call_hooked(ports, "erts_internal", "port_info", 2, &[plain, memory])
                .expect("port_info/2");
            let key = beam_codec::term::tuple_element(answer, 0).expect("a 2-tuple");
            assert_eq!(atom_name(&fx.atoms, key), Some("memory"));
            Ok(fx.term(|ctx| ctx.list(&[key]).expect("fits")))
        },
    ));
    probes.push(row(
        "port_info/2 of `registered_name` on an unregistered port is the bare `[]`",
        "P = open_port({spawn,\"sleep 5\"},[]), io:format(\"~w~n\",\
         [erts_internal:port_info(P,registered_name)])",
        "[]",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let item = fx.term(|ctx| ctx.atom("registered_name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[plain, item])
        },
    ));
    probes.push(row(
        "port_info/2 of `registered_name` on a registered port is the pair",
        "P = open_port({spawn,\"sleep 5\"},[]), register(myp,P), io:format(\"~w~n\",\
         [erts_internal:port_info(P,registered_name)])",
        "{registered_name,myp}",
        |fx, ports| {
            let named = PortId::new(NAMED).to_eterm();
            let item = fx.term(|ctx| ctx.atom("registered_name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[named, item])
        },
    ));
    probes.push(row(
        "port_info/2 of an unknown item is the `badarg` value",
        "P = open_port({spawn,\"sleep 5\"},[]), io:format(\"~w~n\",\
         [erts_internal:port_info(P,no_such_item)])",
        "badarg",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let item = fx.term(|ctx| ctx.atom("no_such_item").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[plain, item])
        },
    ));
    probes.push(row(
        "port_info/2 of a non-atom item is the `badarg` value",
        "P = open_port({spawn,\"sleep 5\"},[]), io:format(\"~w~n\",\
         [erts_internal:port_info(P,1)])",
        "badarg",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let item = fx.term(|ctx| ctx.int(1).expect("fits"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[plain, item])
        },
    ));
    probes.push(row(
        "port_info/2 of a non-port is `badtype` (where /1 says `badarg`)",
        "io:format(\"~w~n\",[erts_internal:port_info(1,name)])",
        "badtype",
        |fx, ports| {
            let one = fx.term(|ctx| ctx.int(1).expect("fits"));
            let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[one, name])
        },
    ));
    probes.push(row(
        "port_info/2 of a pid is `badtype`",
        "io:format(\"~w~n\",[erts_internal:port_info(self(),name)])",
        "badtype",
        |fx, ports| {
            let pid = ProcessId::new(1, 0).to_eterm();
            let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[pid, name])
        },
    ));
    probes.push(row(
        "port_info/2 of a dropped port is `undefined` for any item",
        "P = open_port({spawn,\"sleep 5\"},[]), true = erts_internal:port_close(P), \
         io:format(\"~w~n\",[erts_internal:port_info(P,registered_name)])",
        "undefined",
        |fx, ports| {
            let plain = PortId::new(PLAIN).to_eterm();
            let closed = fx.call_hooked(ports, "erts_internal", "port_close", 1, &[plain]);
            assert_status(fx, &closed, "true");
            let item = fx.term(|ctx| ctx.atom("registered_name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[plain, item])
        },
    ));

    // ---- The recorded capability gaps ----
    probes.push(gap_row(
        "port_close/1 of an unregistered name (the C answers `badarg`)",
        "io:format(\"~w~n\",[erts_internal:port_close(no_such_name)])",
        "badarg",
        |fx, ports| {
            let name = fx.term(|ctx| ctx.atom("no_such_name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_close", 1, &[name])
        },
    ));
    probes.push(gap_row(
        "port_close/1 of a registered name (the C answers `true`)",
        "P = open_port({spawn,\"sleep 5\"},[]), register(myp,P), \
         io:format(\"~w~n\",[erts_internal:port_close(myp)])",
        "true",
        |fx, ports| {
            let name = fx.term(|ctx| ctx.atom(PORT_NAME).expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_close", 1, &[name])
        },
    ));
    probes.push(gap_row(
        "port_info/1 of an unregistered name (the C answers `undefined`)",
        "io:format(\"~w~n\",[erts_internal:port_info(no_such_name)])",
        "undefined",
        |fx, ports| {
            let name = fx.term(|ctx| ctx.atom("no_such_name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 1, &[name])
        },
    ));
    probes.push(gap_row(
        "port_info/2 of an unregistered name (the C answers `undefined`)",
        "io:format(\"~w~n\",[erts_internal:port_info(no_such_name,name)])",
        "undefined",
        |fx, ports| {
            let name = fx.term(|ctx| ctx.atom("no_such_name").expect("seeded"));
            let item = fx.term(|ctx| ctx.atom("name").expect("seeded"));
            fx.call_hooked(ports, "erts_internal", "port_info", 2, &[name, item])
        },
    ));
    probes.push(gap_row(
        "open_port/2 (the C answers a `#Ref<…>` init ack)",
        "io:format(\"~w~n\",[is_reference(erts_internal:open_port({spawn,\"sleep 5\"},[]))])",
        "true",
        |fx, ports| {
            let spawn = fx.term(|ctx| ctx.atom("spawn").expect("seeded"));
            let command = fx.term(|ctx| chars(ctx, COMMAND));
            let name = fx.term(|ctx| ctx.tuple(&[spawn, command]).expect("fits"));
            let answer = fx.call_hooked(ports, "erts_internal", "open_port", 2, &[name, NIL]);
            assert!(
                ports.opened.is_empty(),
                "a synchronously opened port would be a wrong result — the seam is never reached"
            );
            answer
        },
    ));

    probes
}

// ---------------------------------------------------------------------------
// Structural pins (these hold with or without an `erl` on `$PATH`)
// ---------------------------------------------------------------------------

/// `erts_internal:port_info/1` is the C's `ERTS_PORT_INFO_1_ITEMS` roster in the C's
/// order, with `registered_name` **first** when the port holds a name — and it drops
/// every `/2`-only extra the hook reports (`monitors`, `queue_size`, `memory`,
/// `monitored_by`, `locking`, `parallelism`).
#[test]
fn port_info_1_is_the_c_roster_in_the_c_order() {
    for (port, registered) in [(PLAIN, None), (NAMED, Some(PORT_NAME))] {
        let mut fx = Fixture::new();
        let expected_pairs = fx.pairs(registered);
        let expected: Vec<(Eterm, Eterm)> = expected_pairs
            .into_iter()
            .filter(|(key, _)| {
                let name = atom_name(&fx.atoms, *key).expect("an atom item");
                name == "registered_name" || PORT_INFO_ITEMS.contains(&name)
            })
            .collect();
        // Reorder to the C's roster: `registered_name` leads, then the seven.
        let mut ordered: Vec<(Eterm, Eterm)> = Vec::new();
        if let Some(pair) = expected
            .iter()
            .find(|(key, _)| atom_name(&fx.atoms, *key) == Some("registered_name"))
        {
            ordered.push(*pair);
        }
        for item in PORT_INFO_ITEMS {
            let pair = expected
                .iter()
                .find(|(key, _)| atom_name(&fx.atoms, *key) == Some(item))
                .expect("the item is reported");
            ordered.push(*pair);
        }

        let mut ports = MockPorts::standard(&mut fx);
        let answer = fx
            .call_hooked(
                &mut ports,
                "erts_internal",
                "port_info",
                1,
                &[PortId::new(port).to_eterm()],
            )
            .expect("port_info/1");
        assert_renders_as(&fx, answer, "the /1 roster", |ctx| {
            let tuples: Vec<Eterm> = ordered
                .iter()
                .map(|(key, value)| ctx.tuple(&[*key, *value]).expect("fits"))
                .collect();
            ctx.list(&tuples).expect("fits")
        });
        assert_eq!(
            item_keys(&fx, answer),
            if registered.is_some() {
                "[registered_name,name,links,id,connected,input,output,os_pid]"
            } else {
                "[name,links,id,connected,input,output,os_pid]"
            }
        );
    }
}

/// The seven `/1` item keys, in the C's order (`ERTS_PORT_INFO_1_ITEMS`,
/// `erts/emulator/beam/erl_port.h:100-108`) — restated here so the test states the
/// roster it checks rather than reading it back from the code.
const PORT_INFO_ITEMS: [&str; 7] = [
    "name",
    "links",
    "id",
    "connected",
    "input",
    "output",
    "os_pid",
];

/// `erts_internal:port_info/2` searches **every** pair — the `/2`-only extras
/// included — and answers the bare list `[]` for `registered_name` on an unregistered
/// port (the one item whose "absent" answer is not a pair).
#[test]
fn port_info_2_searches_every_pair_and_has_one_bare_list_answer() {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);

    // A `/2`-only item is reachable and keeps the hook's value.
    let plain = PortId::new(PLAIN).to_eterm();
    let memory = fx.term(|ctx| ctx.atom("memory").expect("seeded"));
    let pair = fx
        .call_hooked(&mut ports, "erts_internal", "port_info", 2, &[plain, memory])
        .expect("port_info/2 memory");
    assert_eq!(
        beam_codec::term::tuple_arity(pair).expect("a tuple"),
        2,
        "a /2 answer is the 2-tuple {{Item, Value}}"
    );
    assert_renders_as(&fx, pair, "the memory pair", |ctx| {
        let key = ctx.atom("memory").expect("seeded");
        let value = ctx.int(48).expect("fits");
        ctx.tuple(&[key, value]).expect("fits")
    });

    // `registered_name` on an unregistered port is the bare empty list.
    let item = fx.term(|ctx| ctx.atom("registered_name").expect("seeded"));
    let bare = fx
        .call_hooked(&mut ports, "erts_internal", "port_info", 2, &[plain, item])
        .expect("port_info/2 registered_name");
    assert_eq!(bare, NIL, "`registered_name` on an unregistered port is `[]`");

    // On a registered port it is the pair.
    let named = PortId::new(NAMED).to_eterm();
    let pair = fx
        .call_hooked(&mut ports, "erts_internal", "port_info", 2, &[named, item])
        .expect("port_info/2 registered_name");
    assert_renders_as(&fx, pair, "the registered_name pair", |ctx| {
        let key = ctx.atom("registered_name").expect("seeded");
        let value = ctx.atom(PORT_NAME).expect("seeded");
        ctx.tuple(&[key, value]).expect("fits")
    });
}

/// The `badarg`/`badtype` asymmetry between the two `port_info` arities, and the
/// value-shaped `badarg` of `port_close/1` — the exact split the `erlang.erl` wrappers
/// rely on.
#[test]
fn the_failure_atoms_are_values_and_slash_2_says_badtype() {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let not_a_port = fx.term(|ctx| ctx.int(1).expect("fits"));
    let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));

    let close = fx.call_hooked(&mut ports, "erts_internal", "port_close", 1, &[not_a_port]);
    assert_status(&fx, &close, "badarg");

    let info_1 = fx.call_hooked(&mut ports, "erts_internal", "port_info", 1, &[not_a_port]);
    assert_status(&fx, &info_1, "badarg");

    let info_2 = fx.call_hooked(
        &mut ports,
        "erts_internal",
        "port_info",
        2,
        &[not_a_port, name],
    );
    assert_status(&fx, &info_2, "badtype");

    // Whatever else it is, it is never a raise: a raise would make the `erlang:`
    // wrapper's `error_with_info/2` fire for a shape the C answers as a value.
    for answer in [close, info_1, info_2] {
        assert!(answer.is_ok(), "the C returns the status atom, it does not raise");
    }
    assert!(ports.closed.is_empty(), "no port was closed by a bad argument");
}

/// A port the runtime no longer holds answers `undefined` for **every** `/2` item —
/// `registered_name` included, which is what distinguishes it from a *live*
/// unregistered port's bare `[]`.
#[test]
fn a_dropped_port_is_undefined_for_both_arities() {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let plain = PortId::new(PLAIN).to_eterm();

    let closed = fx.call_hooked(&mut ports, "erts_internal", "port_close", 1, &[plain]);
    assert_status(&fx, &closed, "true");

    let info_1 = fx.call_hooked(&mut ports, "erts_internal", "port_info", 1, &[plain]);
    assert_status(&fx, &info_1, "undefined");
    for item in ["name", "registered_name", "memory"] {
        let item = fx.term(|ctx| ctx.atom(item).expect("seeded"));
        let answer = fx.call_hooked(&mut ports, "erts_internal", "port_info", 2, &[plain, item]);
        assert_status(&fx, &answer, "undefined");
    }
}

/// The atom forms are the **typed gap**: the C resolves a registered name through
/// `erts_whereis_name_to_id`, and the [`PortHook`] seam exposes no name→port lookup,
/// so a fabricated `badarg` (the *wrong* answer for a registered name) is refused.
#[test]
fn the_atom_name_forms_are_the_typed_name_resolution_gap() {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let unknown = fx.term(|ctx| ctx.atom("no_such_name").expect("seeded"));
    let registered = fx.term(|ctx| ctx.atom(PORT_NAME).expect("seeded"));
    let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));

    for arg in [unknown, registered] {
        let answer = fx.call_hooked(&mut ports, "erts_internal", "port_close", 1, &[arg]);
        assert_gap(&answer);
        let answer = fx.call_hooked(&mut ports, "erts_internal", "port_info", 1, &[arg]);
        assert_gap(&answer);
        let answer = fx.call_hooked(&mut ports, "erts_internal", "port_info", 2, &[arg, name]);
        assert_gap(&answer);
    }
    assert!(ports.closed.is_empty(), "an unresolved name closes nothing");
}

/// With **no** `PortHook` installed the argument-shape decisions still run — they are
/// in-crate and never reach the seam — so `badarg`/`badtype` are returned as values,
/// the atom-name forms stay the typed gap, and only a genuine *port* argument becomes
/// the typed `unsupported_bif`.
#[test]
fn without_a_port_hook_the_argument_shape_decisions_still_run() {
    let mut fx = Fixture::new();
    let port = PortId::new(PLAIN).to_eterm();
    let one = fx.term(|ctx| ctx.int(1).expect("fits"));
    let pid = ProcessId::new(1, 0).to_eterm();
    let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));
    let atom = fx.term(|ctx| ctx.atom("no_such_name").expect("seeded"));

    // A real port argument needs the seam: the typed gap.
    for (function, arity, args) in [
        ("port_close", 1u32, vec![port]),
        ("port_info", 1, vec![port]),
        ("port_info", 2, vec![port, name]),
        ("open_port", 2, vec![name, NIL]),
    ] {
        let answer = fx.call("erts_internal", function, arity, &args);
        assert_gap(&answer);
    }

    // The in-crate shape decisions answer before the seam is ever consulted.
    let close = fx.call("erts_internal", "port_close", 1, &[one]);
    assert_status(&fx, &close, "badarg");
    let close = fx.call("erts_internal", "port_close", 1, &[pid]);
    assert_status(&fx, &close, "badarg");
    let info_1 = fx.call("erts_internal", "port_info", 1, &[one]);
    assert_status(&fx, &info_1, "badarg");
    let info_2 = fx.call("erts_internal", "port_info", 2, &[one, name]);
    assert_status(&fx, &info_2, "badtype");

    // The name forms are the gap with or without a hook.
    for (function, arity, args) in [
        ("port_close", 1u32, vec![atom]),
        ("port_info", 1, vec![atom]),
        ("port_info", 2, vec![atom, name]),
    ] {
        let answer = fx.call("erts_internal", function, arity, &args);
        assert_gap(&answer);
    }
}

/// `erts_internal:open_port/2` is the documented typed gap, while `erlang:open_port/2`
/// (the wrapper spelling this crate implements over the same seam) still opens a port.
///
/// The C answers a **`#Ref<…>`** for a driver carrying `ERL_DRV_FLAG_USE_INIT_ACK` —
/// the `spawn` driver, which backs every `{spawn, Cmd}` — and delivers `{Ref, Port}`
/// later; the seam opens synchronously, so returning a port would be a wrong result and
/// returning a reference nobody answers would hang the `erlang.erl` wrapper.
#[test]
fn open_port_is_the_documented_ack_ref_gap() {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let spawn = fx.term(|ctx| ctx.atom("spawn").expect("seeded"));
    let command = fx.term(|ctx| chars(ctx, COMMAND));
    let name = fx.term(|ctx| ctx.tuple(&[spawn, command]).expect("fits"));

    let c_spelling = fx.call_hooked(&mut ports, "erts_internal", "open_port", 2, &[name, NIL]);
    assert_gap(&c_spelling);
    assert!(
        ports.opened.is_empty(),
        "the C spelling must not open a port the caller cannot receive an ack for"
    );

    // The `erlang:` spelling (this crate's `erlang:open_port/2`) does open one.
    let wrapper = fx
        .call_hooked(&mut ports, "erlang", "open_port", 2, &[name, NIL])
        .expect("erlang:open_port/2");
    assert_eq!(
        PortId::from_eterm(wrapper).expect("an internal port"),
        PortId::new(99)
    );
    assert_eq!(ports.opened.len(), 1, "exactly one open reached the hook");
}

/// The `erlang:` spellings are **unchanged** by the `erts_internal:` registrations:
/// `port_close/1` returns `true` and raises on a second close, `port_info/1` is exactly
/// the seven `/1` items (`registered_name` belongs to the `erts_internal:` spelling and
/// to the runtime's floor), `port_info/2` is the `{Item, Value}` tuple, and a non-port
/// raises `badarg` from all three.
#[test]
fn the_erlang_spellings_are_unchanged() {
    let mut fx = Fixture::new();
    let mut ports = MockPorts::standard(&mut fx);
    let plain = PortId::new(PLAIN).to_eterm();
    let not_a_port = fx.term(|ctx| ctx.int(1).expect("fits"));
    let name = fx.term(|ctx| ctx.atom("name").expect("seeded"));

    let closed = fx.call_hooked(&mut ports, "erlang", "port_close", 1, &[plain]);
    match closed {
        Ok(term) => assert_eq!(atom_name(&fx.atoms, term), Some("true")),
        other => panic!("erlang:port_close/1 must return `true`, got {other:?}"),
    }
    let again = fx.call_hooked(&mut ports, "erlang", "port_close", 1, &[plain]);
    assert_badarg(&again);

    // `/1` keeps the seven roster items — and only those (no `registered_name`, no
    // `/2`-only extra) even for a registered port.
    let named = PortId::new(NAMED).to_eterm();
    let info_1 = fx
        .call_hooked(&mut ports, "erlang", "port_info", 1, &[named])
        .expect("erlang:port_info/1");
    assert_eq!(
        item_keys(&fx, info_1),
        "[name,links,id,connected,input,output,os_pid]"
    );

    // `/2` is the tuple, including a `/2`-only item.
    let memory = fx.term(|ctx| ctx.atom("memory").expect("seeded"));
    let info_2 = fx
        .call_hooked(&mut ports, "erlang", "port_info", 2, &[named, memory])
        .expect("erlang:port_info/2");
    assert_renders_as(&fx, info_2, "the memory pair", |ctx| {
        let key = ctx.atom("memory").expect("seeded");
        let value = ctx.int(48).expect("fits");
        ctx.tuple(&[key, value]).expect("fits")
    });
    let unknown = fx.term(|ctx| ctx.atom("no_such_item").expect("seeded"));
    let missing = fx.call_hooked(&mut ports, "erlang", "port_info", 2, &[named, unknown]);
    assert_badarg(&missing);

    // A non-port raises `badarg` from every `erlang:` spelling.
    for (function, arity, args) in [
        ("port_close", 1u32, vec![not_a_port]),
        ("port_info", 1, vec![not_a_port]),
        ("port_info", 2, vec![not_a_port, name]),
    ] {
        let answer = fx.call_hooked(&mut ports, "erlang", function, arity, &args);
        assert_badarg(&answer);
    }
}

/// The four keys are registered and gated in **both** spellings through the one
/// `PortHook` gate (`needs_port_hook`), so a host cannot build the bridge for one
/// spelling and miss the other.
#[test]
fn the_quartet_is_registered_and_gated() {
    let reg = default_registry();
    for (module, function, arity) in [
        ("erts_internal", "port_close", 1),
        ("erts_internal", "port_info", 1),
        ("erts_internal", "port_info", 2),
        ("erts_internal", "open_port", 2),
        ("erlang", "port_close", 1),
        ("erlang", "port_info", 1),
        ("erlang", "port_info", 2),
        ("erlang", "open_port", 2),
    ] {
        assert!(reg.contains(module, function, arity), "{module}:{function}/{arity}");
        assert!(
            needs_port_hook(module, function, arity),
            "{module}:{function}/{arity} reads the PortHook seam"
        );
    }
}

// ---------------------------------------------------------------------------
// The live-oracle cross-check
// ---------------------------------------------------------------------------

/// The wall-clock budget for the oracle.
const ERL_TIMEOUT: Duration = Duration::from_secs(120);

/// Re-derives the whole matrix under `erl -noshell -eval …` and compares it row for
/// row: for an agreeing row the oracle must print this runtime's own text, and for a
/// recorded gap it must print the pinned oracle text (while the Rust side answers the
/// typed `unsupported_bif`).
#[test]
fn the_pinned_matrix_agrees_with_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!("[beam_bifs erts_internal_ports] SKIP: no usable `erl` on $PATH");
        eprintln!("(set $ERTS_ORACLE or install Erlang/OTP — this is NOT a pass).");
        return;
    };

    let probes = matrix();
    let program = format!(
        "{},\nhalt(0).",
        probes
            .iter()
            .map(|probe| probe.erl.as_str())
            .collect::<Vec<_>>()
            .join(",\n")
    );
    let Some(stdout) = erl_stdout(&oracle, &program) else {
        eprintln!("[beam_bifs erts_internal_ports] SKIP: the oracle did not answer");
        return;
    };

    let lines: Vec<&str> = stdout.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        probes.len(),
        "the oracle printed {} lines for {} probes\n--- stdout ---\n{stdout}\n\
         --- program ---\n{program}",
        lines.len(),
        probes.len()
    );
    for (index, probe) in probes.iter().enumerate() {
        assert_eq!(
            lines[index], probe.oracle,
            "probe `{}` disagrees with the oracle\n--- program ---\n{program}",
            probe.label
        );
        if probe.oracle == probe.rust {
            // An agreeing row: the crate's answer and the oracle's are the same text.
            assert_eq!(lines[index], probe.rust, "probe `{}`", probe.label);
        } else {
            assert_eq!(
                probe.rust, "{gap,unsupported_bif}",
                "a row whose oracle text differs must be a recorded capability gap: `{}`",
                probe.label
            );
        }
    }
    eprintln!(
        "[beam_bifs erts_internal_ports] oracle cross-check RAN against '{}': {} probes agreed.",
        oracle.display(),
        probes.len()
    );
}

/// `$ERTS_ORACLE`, else the first executable `erl` on `$PATH`.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(program) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(program);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs erts_internal_ports] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs one `erl -noshell -eval <program>` under a bounded wall-clock budget and
/// returns its stdout, or `None` (after a loud skip message) when it cannot answer.
///
/// The child's stdout and stderr are drained on worker threads (so a full pipe can
/// never wedge it) while the main thread waits with a deadline; a child that overruns
/// [`ERL_TIMEOUT`] is **killed** and reaped, so a hung `erl` can never hang the suite.
fn erl_stdout(oracle: &Path, program: &str) -> Option<String> {
    let spawned = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "[beam_bifs erts_internal_ports] SKIP: could not spawn the oracle '{}': {error}",
                oracle.display()
            );
            return None;
        }
    };

    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + ERL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    eprintln!(
                        "[beam_bifs erts_internal_ports] SKIP: the oracle did not answer \
                         within {ERL_TIMEOUT:?} and was killed."
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                eprintln!(
                    "[beam_bifs erts_internal_ports] SKIP: waiting for the oracle failed: {error}"
                );
                return None;
            }
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        eprintln!(
            "[beam_bifs erts_internal_ports] SKIP: the oracle exited {status}\n\
             --- stderr ---\n{stderr}"
        );
        return None;
    }
    Some(stdout)
}

fn read_pipe(mut pipe: impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}
