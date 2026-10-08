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

//! `erlang:process_info/1,2`'s per-process control items at the runtime boundary (contract A.8).
//!
//! `src/native/memory`'s unit tests pin the item vocabulary and the value shapes against the
//! process table; this suite pins the path the way a user sees it: a **real** `.beam` is
//! spawned as a process, it parks in a `receive`, and this runtime's `erlang:process_info/2`
//! answers are compared with the live system `erl` oracle running the **same** fixture the
//! **same** way.
//!
//! # The items under test
//!
//! `status`, `registered_name`, `messages`, `message_queue_len`, `links`, `monitors`,
//! `monitored_by`, `trap_exit` and `priority`. Each is answered from this runtime's own
//! process table ([`beam_process::Process`]) — never fabricated, and a pid that names no live
//! process is the atom `undefined` (see the module docs of `src/native/memory`).
//!
//! # Oracle ground truth (system `erl`, OTP 29.1.1 / `erts-17.1`)
//!
//! The fixture is the committed `tests/fixtures/mailbox.beam` (`mailbox:wait/0` parks on an
//! empty mailbox). The oracle runs `spawn(mailbox, wait, [])` and prints the nine item values
//! as a `|`-joined line:
//!
//! ```text
//! $ erl -noshell -pa <scratch> -eval '<ITEMS_PROBE>' -s init stop
//! waiting|[]|[]|0|[]|[]|[]|false|normal
//! ```
//!
//! A freshly spawned, **unregistered** process is the one scenario in which every item is
//! comparable: the oracle's `P` has `links=[]`, `trap_exit=false`, `registered_name=[]`
//! (unregistered) and `status=waiting` — exactly this runtime's own parked process. (The
//! oracle's `-eval` process is *not* comparable: it is `trap_exit=true` and linked to
//! `<0.0.0>`, while this runtime's root process is registered as `init`.)
//!
//! The `monitors`/`monitored_by` pair is compared in the one construction both runtimes can
//! build — the probing process monitors the parked one — normalized to the facts that are
//! pid-independent (`monitors => [{process,true}]`, `monitored_by => [true]`).
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_process::{PortId, ProcessState};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_nil, is_small, make_small, signed_value, Eterm};

/// The committed mailbox fixture (`erlc +deterministic mailbox.erl`; `mailbox:wait/0` parks
/// on an empty mailbox) — the scenario every item below is compared in.
const MAILBOX_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/mailbox.beam"
);

/// The nine process-control items, in the order both sides request them.
const ITEMS: [&str; 9] = [
    "status",
    "registered_name",
    "messages",
    "message_queue_len",
    "links",
    "monitors",
    "monitored_by",
    "trap_exit",
    "priority",
];

/// The recorded oracle answer for a parked `mailbox:wait/0` (re-probed and re-asserted in
/// [`the_process_control_items_match_the_live_oracle_for_a_parked_process`]).
const ORACLE_ITEMS_LINE: &str = "waiting|[]|[]|0|[]|[]|[]|false|normal";

/// `spawn(mailbox, wait, [])` + a short sleep (so it really parks), then the nine item
/// values as one `|`-joined line.
const ITEMS_PROBE: &str = "\
P = spawn(mailbox, wait, []), \
timer:sleep(50), \
Items = [status, registered_name, messages, message_queue_len, links, monitors, monitored_by, trap_exit, priority], \
Pairs = process_info(P, Items), \
io:format(\"~s~n\", [lists:join(\"|\", [io_lib:format(\"~p\", [element(2, Pair)]) || Pair <- Pairs])]), \
halt(0).";

/// `spawn(mailbox, wait, [])`, monitor it from the probing process, and print the two
/// pid-normalized facts (`monitors` of the watcher, `monitored_by` of the watched).
const MONITORS_PROBE: &str = "\
P = spawn(mailbox, wait, []), \
timer:sleep(50), \
_ = erlang:monitor(process, P), \
{monitors, M} = process_info(self(), monitors), \
{monitored_by, MB} = process_info(P, monitored_by), \
Mhat = [case E of {process, X} -> {process, X =:= P}; {port, X} -> {port, is_port(X)} end || E <- M], \
MBhat = [X =:= self() || X <- MB], \
io:format(\"~p|~p~n\", [Mhat, MBhat]), \
halt(0).";

/// The recorded oracle answer for [`MONITORS_PROBE`].
const ORACLE_MONITORS_LINE: &str = "[{process,true}]|[true]";

// ---------------------------------------------------------------------------------------
// Runtime plumbing
// ---------------------------------------------------------------------------------------

/// Run `f` against a fresh `RuntimeContext` over `rt`; the context (and its sink) are dropped
/// before `rt` is borrowed again.
fn with_context<T>(rt: &mut Runtime, f: impl FnOnce(&mut RuntimeContext<'_>) -> T) -> T {
    let mut sink = NullSink;
    let mut ctx = RuntimeContext::with_output(rt, &mut sink);
    f(&mut ctx)
}

/// Calls `erlang:process_info/2` for `pid`/`item` through the runtime's own dispatch (the
/// runtime-native floor path).
fn info(rt: &mut Runtime, pid: Eterm, item: &str) -> beam_common::Result<Eterm> {
    let item = rt.intern(item);
    with_context(rt, |ctx| {
        ctx.call_bif("erlang", "process_info", 2, &[pid, item])
    })
}

/// The `(Key, Value)` halves of a `{Item, Value}` reply.
fn pair(reply: Eterm) -> (Eterm, Eterm) {
    assert_eq!(terms::tuple_arity(reply).expect("a tuple"), 2);
    (
        terms::tuple_element(reply, 0).expect("key"),
        terms::tuple_element(reply, 1).expect("value"),
    )
}

/// The item atoms of [`ITEMS`] as an Erlang list (the `process_info/2` list form).
fn item_list(rt: &mut Runtime) -> Eterm {
    let atoms: Vec<Eterm> = ITEMS.iter().map(|name| rt.intern(name)).collect();
    with_context(rt, |ctx| {
        terms::make_list(ctx, &atoms).expect("a list of item atoms")
    })
}

/// A runtime with `mailbox.beam` loaded and a `mailbox:wait/0` process parked in its
/// `receive` — the scenario both sides compare.
fn parked_runtime() -> (Runtime, Eterm) {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(MAILBOX_BEAM).expect("mailbox.beam loads");
    let parked = rt
        .spawn_process("mailbox", "wait", vec![])
        .expect("spawn mailbox:wait/0");
    rt.run_scheduler().expect("the pass parks the receiver");
    let state = rt.processes().get(parked).expect("the receiver is alive").state();
    assert_eq!(
        state,
        ProcessState::Waiting,
        "the receiver parked in its receive"
    );
    assert!(rt.process_result(parked).is_none(), "it has not returned");
    (rt, parked.to_eterm())
}

// ---------------------------------------------------------------------------------------
// Canonical renderings (the alphabet both sides print)
// ---------------------------------------------------------------------------------------

/// The canonical `~p` rendering of one item **value** in this scenario: an atom by name, a
/// small integer in decimal, or the empty list as `[]`. The nine items answer only those
/// shapes here, so anything else fails loudly rather than comparing a made-up string.
fn canonical_value(rt: &Runtime, term: Eterm) -> String {
    if is_nil(term) {
        return "[]".to_string();
    }
    if is_small(term) {
        return signed_value(term).to_string();
    }
    if let Some(name) = rt.atom_name_of(term) {
        return name.to_string();
    }
    panic!("a process-control item answered {term:?}, not an atom, a small integer or []");
}

/// The nine item values of `pid`, in [`ITEMS`] order, as one `|`-joined line — the same
/// rendering [`ITEMS_PROBE`] prints, and it also asserts the list form's requested order.
fn items_line(rt: &mut Runtime, pid: Eterm) -> String {
    let list = item_list(rt);
    let reply = with_context(rt, |ctx| {
        ctx.call_bif("erlang", "process_info", 2, &[pid, list])
    })
    .expect("process_info/2");
    let entries = terms::list_elements(reply).expect("the list form is a list");
    assert_eq!(
        entries.len(),
        ITEMS.len(),
        "one {{Item, Value}} per requested item"
    );
    let mut values = Vec::with_capacity(ITEMS.len());
    for (entry, expected) in entries.iter().zip(ITEMS) {
        let (key, value) = pair(*entry);
        assert_eq!(
            rt.atom_name_of(key),
            Some(expected),
            "the list form echoes the requested order"
        );
        values.push(canonical_value(rt, value));
    }
    values.join("|")
}

/// The `monitors` list normalized to `[{process|<bool>} | {port,<bool>}]` — the monitored
/// pid compared by identity, a port by being a port id — so it can be compared with the
/// oracle's rendering of the same facts.
fn canonical_monitors(rt: &Runtime, value: Eterm, watched: Eterm) -> String {
    let entries = terms::list_elements(value).expect("a list");
    let parts: Vec<String> = entries
        .iter()
        .map(|entry| {
            let kind = rt
                .atom_name_of(terms::tuple_element(*entry, 0).expect("kind"))
                .expect("a monitor kind is an atom")
                .to_string();
            let target = terms::tuple_element(*entry, 1).expect("target");
            match kind.as_str() {
                "process" => format!("{{process,{}}}", target == watched),
                "port" => format!("{{port,{}}}", PortId::from_eterm(target).is_ok()),
                other => panic!("unexpected monitor kind `{other}`"),
            }
        })
        .collect();
    format!("[{}]", parts.join(","))
}

/// The `monitored_by` list normalized to `[<is the watcher>]`.
fn canonical_watchers(value: Eterm, watcher: Eterm) -> String {
    let entries = terms::list_elements(value).expect("a list");
    let parts: Vec<String> = entries
        .iter()
        .map(|pid| (*pid == watcher).to_string())
        .collect();
    format!("[{}]", parts.join(","))
}

// ---------------------------------------------------------------------------------------
// The scratch directory handed to the oracle
// ---------------------------------------------------------------------------------------

/// A scratch directory holding a copy of `mailbox.beam`, handed to the oracle as a single
/// `-pa` (never `tests/fixtures/` itself, which would put every fixture on the code path).
struct Scratch {
    dir: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the scratch dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Copies the committed fixture into a fresh scratch directory, or `None` (loudly) when the
/// scratch directory cannot be prepared.
fn scratch_with_mailbox() -> Option<Scratch> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_proc_info_{}_{n}",
        std::process::id()
    ));
    let source = match std::fs::read(MAILBOX_BEAM) {
        Ok(bytes) => bytes,
        Err(error) => {
            support::skip_loudly("process_info_items", &format!("cannot read {MAILBOX_BEAM}: {error}"));
            return None;
        }
    };
    if let Err(error) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join("mailbox.beam"), &source))
    {
        support::skip_loudly(
            "process_info_items",
            &format!("cannot prepare {}: {error}", dir.display()),
        );
        return None;
    }
    Some(Scratch { dir })
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The two failure modes of the item vocabulary (contract A.8): a first argument that is not
/// an internal pid is the oracle's `badarg`, while a valid item on a pid the table does not
/// hold is the atom `undefined` — never a fabricated value. (The unknown-item `badarg` and the
/// `{dictionary, Key}` form are pinned by `src/native/memory`'s unit tests.)
#[test]
fn a_non_pid_first_argument_is_badarg_and_an_unknown_pid_is_undefined() {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    let status = rt.intern("status");

    let error = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "process_info", 2, &[make_small(42), status])
    })
    .expect_err("a non-pid process is badarg");
    support::assert_badarg(error);

    let dead = beam_process::ProcessId::new(999_999, 0).to_eterm();
    let reply = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "process_info", 2, &[dead, status])
    })
    .expect("process_info/2");
    assert_eq!(
        rt.atom_name_of(reply),
        Some("undefined"),
        "a valid item on an unknown pid is undefined"
    );
}

/// Every process-control item matches the live oracle for a parked, unregistered process —
/// `status` (waiting), `registered_name` (`[]`), `messages` (`[]`), `message_queue_len` (0),
/// `links` (`[]`), `monitors` (`[]`), `monitored_by` (`[]`), `trap_exit` (false) and
/// `priority` (normal) — value for value, in the requested order.
#[test]
fn the_process_control_items_match_the_live_oracle_for_a_parked_process() {
    let Some(oracle) = support::require_oracle("process_info_items") else {
        return;
    };
    let Some(scratch) = scratch_with_mailbox() else {
        return;
    };
    let Some(expected) = support::oracle_eval(&oracle, Some(scratch.dir.as_path()), ITEMS_PROBE) else {
        eprintln!("SKIP the live-oracle comparison: the erl probe could not answer");
        return;
    };
    assert_eq!(
        expected, ORACLE_ITEMS_LINE,
        "the recorded oracle answer for a parked mailbox:wait/0"
    );

    let (mut rt, parked) = parked_runtime();
    assert_eq!(
        items_line(&mut rt, parked),
        expected,
        "every process-control item must match the live oracle for the same fixture"
    );
}

/// `monitors` and `monitored_by` match the live oracle for the one construction both runtimes
/// can build: the probing process monitors the parked one, so the watcher's `monitors` holds
/// `{process, Parked}` and the watched process's `monitored_by` holds the watcher.
#[test]
fn monitors_and_monitored_by_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("process_info_items") else {
        return;
    };
    let Some(scratch) = scratch_with_mailbox() else {
        return;
    };
    let Some(expected) = support::oracle_eval(&oracle, Some(scratch.dir.as_path()), MONITORS_PROBE) else {
        eprintln!("SKIP the live-oracle comparison: the erl probe could not answer");
        return;
    };
    assert_eq!(
        expected, ORACLE_MONITORS_LINE,
        "the recorded oracle answer for the monitor construction"
    );

    let (mut rt, parked) = parked_runtime();
    let process_atom = rt.intern("process");
    let reference = with_context(&mut rt, |ctx| {
        ctx.call_bif("erlang", "monitor", 2, &[process_atom, parked])
    })
    .expect("erlang:monitor(process, Parked)");
    // The oracle answers a reference term for the monitor; this runtime's is a real one.
    assert!(
        beam_term::is_boxed_term(reference),
        "erlang:monitor/2 answers a reference"
    );

    let self_pid = rt.self_pid().to_eterm();
    let monitors = pair(info(&mut rt, self_pid, "monitors").expect("process_info/2 monitors")).1;
    let monitored_by =
        pair(info(&mut rt, parked, "monitored_by").expect("process_info/2 monitored_by")).1;

    assert_eq!(
        format!(
            "{}|{}",
            canonical_monitors(&rt, monitors, parked),
            canonical_watchers(monitored_by, self_pid)
        ),
        expected,
        "the two monitor families must match the live oracle"
    );
}
