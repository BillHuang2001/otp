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

//! The `-eval` evaluator end-to-end: **the differential corpus's `Eval` probes**.
//!
//! `tools/differential` compares the Rust runtime against the system `erl` by handing
//! both the *same* self-terminating `-eval` probe and diffing stdout + exit status. The
//! harness is the compatibility gate, but it needs the oracle (`erl`) at run time. This
//! test **pins the golden values** the corpus asserts, so a regression is caught even
//! where the oracle is unavailable.
//!
//! Every probe is invoked at the CLI boundary (`env!("CARGO_BIN_EXE_beam_rt")`) with
//! exactly the argv the differential builds (`-noshell [-<flag> val…] -eval <probe>`) —
//! this exercises the whole parser → evaluator → CLI path, including the `~w` printer and
//! the exit codes, which a library-level test could not observe.
//!
//! The wrapper is reproduced verbatim from `corpus::probe_expr`
//! (`tools/differential/src/corpus/mod.rs`): an uncaught error under `erl -noshell -eval`
//! prints *nothing*, so the expression is wrapped in `try … catch` to make a failure
//! observable (`ok <repr>` / exit 0, or `exception <class> <reason>` / exit 1).
use std::process::Command;

mod support;
/// The `Probe::Value` wrapper: print the value of the expression with `~w`.
const VALUE_PROBE: &str = concat!(
    "try ({}) of V -> io:format(\"ok ~w~n\", [V]), halt(0) catch C:R -> ",
    "io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
);

/// The `Probe::TermToBinary` wrapper: print the canonical ETF bytes of the value.
const TERM_TO_BINARY_PROBE: &str = concat!(
    "try ({}) of V -> io:format(\"ok ~w~n\", [term_to_binary(V)]), halt(0) catch C:R -> ",
    "io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end."
);

/// Which of the two corpus probes wraps the expression.
#[derive(Clone, Copy, Debug)]
enum Kind {
    /// `Probe::Value` — `io:format("ok ~w~n", [V])`.
    Value,
    /// `Probe::TermToBinary` — `io:format("ok ~w~n", [term_to_binary(V)])`.
    TermToBinary,
}

/// Wrap `expr` exactly as [`Kind`] prescribes.
fn wrap(expr: &str, kind: Kind) -> String {
    let template = match kind {
        Kind::Value => VALUE_PROBE,
        Kind::TermToBinary => TERM_TO_BINARY_PROBE,
    };
    // The template holds a single `{}` slot; substitute it once (the expression itself
    // may contain braces, so only the first occurrence is replaced).
    template.replacen("{}", expr, 1)
}

/// Run `beam_rt -noshell <extra…> -eval <wrapped>` and return `(stdout, exit, stderr)`.
fn probe(expr: &str, extra: &[&str], kind: Kind) -> (String, i32, String) {
    let wrapped = wrap(expr, kind);
    let mut args: Vec<&str> = Vec::with_capacity(3 + extra.len());
    args.push("-noshell");
    args.extend_from_slice(extra);
    args.push("-eval");
    args.push(wrapped.as_str());

    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(&args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt for {expr:?} failed: {error}"));

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit = output
        .status
        .code()
        .expect("beam_rt always exits normally (never killed by a signal)");
    (stdout, exit, stderr)
}

/// One differential `Eval` case: the expression, the probe, the extra argv (the A.6
/// custom-flag case) and the golden stdout (without the trailing `\n`) + exit code.
struct Golden {
    /// The corpus case id (`<group>/<name>`) — reported on failure.
    id: &'static str,
    /// The plain Erlang expression.
    expr: &'static str,
    /// Which probe wraps it.
    kind: Kind,
    /// Extra argv inserted before `-eval`.
    extra: &'static [&'static str],
    /// The exact first line of stdout.
    stdout: &'static str,
    /// The process exit code.
    exit: i32,
}

/// The **17** `Eval` cases of the built-in corpus, with the goldens the corpus asserts.
/// Kept in corpus order (terms → etf → bifs → system_info → flags) so a diff against
/// `tools/differential -- suite` lines up one-to-one.
const GOLDENS: &[Golden] = &[
    // --- A.1 language semantics -------------------------------------------------
    Golden {
        id: "terms/compare_int_lists",
        expr: "[1,2,3] < [1,2,4]",
        kind: Kind::TermToBinary,
        extra: &[],
        stdout: "ok <<131,119,4,116,114,117,101>>",
        exit: 0,
    },
    Golden {
        id: "terms/eq_vs_exact_eq",
        expr: "{1 == 1.0, 1 =:= 1.0}",
        kind: Kind::TermToBinary,
        extra: &[],
        stdout: "ok <<131,104,2,119,4,116,114,117,101,119,5,102,97,108,115,101>>",
        exit: 0,
    },
    Golden {
        id: "terms/atom_order",
        expr: "hello < world",
        kind: Kind::TermToBinary,
        extra: &[],
        stdout: "ok <<131,119,4,116,114,117,101>>",
        exit: 0,
    },
    Golden {
        id: "errors/error_boom",
        expr: "erlang:error(boom)",
        kind: Kind::Value,
        extra: &[],
        stdout: "exception error boom",
        exit: 1,
    },
    Golden {
        id: "errors/badarith",
        expr: "1/0",
        kind: Kind::Value,
        extra: &[],
        stdout: "exception error badarith",
        exit: 1,
    },
    // --- A.2 canonical ETF bytes ------------------------------------------------
    Golden {
        id: "etf/term_to_binary_tuple",
        expr: "{ok,42}",
        kind: Kind::TermToBinary,
        extra: &[],
        stdout: "ok <<131,104,2,119,2,111,107,97,42>>",
        exit: 0,
    },
    Golden {
        id: "etf/term_to_binary_atom",
        expr: "hello",
        kind: Kind::TermToBinary,
        extra: &[],
        stdout: "ok <<131,119,5,104,101,108,108,111>>",
        exit: 0,
    },
    Golden {
        id: "etf/term_to_binary_map",
        expr: "#{a => 1}",
        kind: Kind::TermToBinary,
        extra: &[],
        stdout: "ok <<131,116,0,0,0,1,119,1,97,97,1>>",
        exit: 0,
    },
    Golden {
        id: "etf/binary_to_term_roundtrip",
        expr: "binary_to_term(term_to_binary({ok,[1,2,3],<<\"x\">>}))",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok {ok,[1,2,3],<<120>>}",
        exit: 0,
    },
    // --- A.7 BIF / stdlib surface -----------------------------------------------
    Golden {
        id: "bifs/length",
        expr: "length([a,b,c])",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok 3",
        exit: 0,
    },
    Golden {
        id: "bifs/integer_to_binary",
        expr: "integer_to_binary(255)",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok <<50,53,53>>",
        exit: 0,
    },
    Golden {
        id: "bifs/lists_reverse",
        expr: "lists:reverse([1,2,3])",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok [3,2,1]",
        exit: 0,
    },
    // --- A.8 introspection ------------------------------------------------------
    Golden {
        id: "system_info/version",
        expr: "list_to_binary(erlang:system_info(version))",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok <<49,55,46,49>>",
        exit: 0,
    },
    Golden {
        id: "system_info/process_count_is_integer",
        expr: "is_integer(erlang:system_info(process_count))",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok true",
        exit: 0,
    },
    Golden {
        id: "system_info/otp_release_is_string",
        expr: "is_list(erlang:system_info(otp_release))",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok true",
        exit: 0,
    },
    // --- A.6 CLI flags ----------------------------------------------------------
    Golden {
        id: "flags/noshell_argument",
        expr: "init:get_argument(noshell)",
        kind: Kind::Value,
        extra: &[],
        stdout: "ok {ok,[[]]}",
        exit: 0,
    },
    Golden {
        id: "flags/custom_argument",
        expr: "init:get_argument(differential_test_flag)",
        kind: Kind::Value,
        extra: &["-differential_test_flag", "hello"],
        stdout: "ok {ok,[[[104,101,108,108,111]]]}",
        exit: 0,
    },
];

#[test]
fn the_differential_eval_corpus_matches() {
    assert_eq!(GOLDENS.len(), 17, "the corpus has 17 Eval cases");
    let mut failures: Vec<String> = Vec::new();

    for golden in GOLDENS {
        let (stdout, exit, stderr) = probe(golden.expr, golden.extra, golden.kind);
        // `io:format("…~n", …)` emits exactly one trailing newline.
        let Some(first_line) = stdout.strip_suffix('\n') else {
            failures.push(format!(
                "{}: stdout {:?} does not end in a newline",
                golden.id, stdout
            ));
            continue;
        };
        if first_line != golden.stdout {
            failures.push(format!(
                "{}: stdout {:?} != golden {:?}",
                golden.id, first_line, golden.stdout
            ));
        }
        if exit != golden.exit {
            failures.push(format!(
                "{}: exit {} != golden {}",
                golden.id, exit, golden.exit
            ));
        }
        if !stderr.is_empty() {
            failures.push(format!("{}: unexpected stderr {stderr:?}", golden.id));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} differential Eval probes diverged:\n{}",
        failures.len(),
        GOLDENS.len(),
        failures.join("\n")
    );
}

/// Expressions the corpus does not carry, but which the evaluator must render exactly
/// like the oracle: `case`, a comprehension, a `fun` application, an element BIF and
/// unary arithmetic. All goldens were verified against the system `erl` (OTP 29 /
/// erts-17.1).
#[test]
fn evaluates_control_flow_like_the_oracle() {
    let cases: &[(&str, &str)] = &[
        ("case 2 of 1 -> a; 2 -> b end", "ok b"),
        ("lists:sum([X*X || X <- [1,2,3]])", "ok 14"),
        ("(fun(X) -> X + 1 end)(41)", "ok 42"),
        ("[{K,V} || {K,V} <- [{a,1},{b,2}]]", "ok [{a,1},{b,2}]"),
        ("element(2, {a,b,c})", "ok b"),
        ("abs(-7)", "ok 7"),
    ];

    for (expr, expected) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout, format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, 0, "{expr}: exit");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }
}

/// Structured/uncaught raise **reasons** (D6) at the CLI boundary: the *reason* a probe
/// carries must be the real term the producer built (`{badkey,K}` / `{badmap,X}` /
/// `{bad_generator,X}`), never a reshaped atom — and a map literal's key order must be the
/// oracle's canonical order, not insertion order.
///
/// Every golden is the system oracle's own line (`erl -noshell`, OTP 29 / erts-17.1),
/// captured through the **same** probe wrapper and argv the corpus builds:
///
/// ```text
/// try (maps:get(nokey,#{})) of V -> … catch C:R -> io:format("exception ~w ~w~n",[C,R]) …
///   ⇒ exception error {badkey,nokey}   (exit 1)
/// ```
///
/// `tests/structured_errors.rs` asserts the same terms **structurally** through
/// [`Runtime::eval`](beam_rt::Runtime::eval) (including that a `try … catch` really *catches*
/// them); this pins the printer + exit-code boundary the differential gate reads.
#[test]
fn structured_raise_reasons_and_map_key_order_match_the_oracle() {
    let cases: &[(&str, &str, i32)] = &[
        ("maps:get(nokey,#{})", "exception error {badkey,nokey}", 1),
        (
            "map_get(a, not_a_map)",
            "exception error {badmap,not_a_map}",
            1,
        ),
        (
            "[Y || Y <- not_a_list]",
            "exception error {bad_generator,not_a_list}",
            1,
        ),
        (
            "<<Y || <<Y>> <= not_a_binary>>",
            "exception error {bad_generator,not_a_binary}",
            1,
        ),
        ("maps:keys(#{3=>a,1=>b,2=>c})", "ok [1,2,3]", 0),
    ];

    for (expr, expected, expected_exit) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, *expected_exit, "{expr}: exit (stderr {stderr:?})");
    }
}
/// `erlang:raise/3` at the CLI boundary: the evaluator does **not** serve it, so the
/// registered `beam_bifs` BIF validates the class *and* the stacktrace argument.
///
/// The distinction that makes it observable is returned-value vs raise: the C ends every
/// invalid-shape path in `return am_badarg`, so an invalid class (`foo`, `1`) or an invalid
/// stacktrace (`42`, `x`, `{}`) is the atom `badarg` **returned** by the call — the
/// corpus probe's `of V` branch prints `ok badarg` and exits `0` — while a valid call
/// raises. Serving `raise/3` locally used to *raise* `badarg` (a caught
/// `exception error badarg`, exit 1) and to drop the stacktrace.
///
/// Every golden is the system oracle's own line (`erl -noshell`, OTP 29 / erts-17.1),
/// captured through the **same** probe wrapper and argv the corpus builds:
///
/// ```text
/// try (erlang:raise(error, foo, [42])) of V -> io:format("ok ~w~n",[V]), halt(0)
///   catch C:R -> io:format("exception ~w ~w~n",[C,R]), halt(1) end
///   ⇒ ok badarg            (exit 0)   — the BIF *returns* badarg
/// try (erlang:raise(error, foo, [{bar,baz,0}])) … ⇒ exception error foo (exit 1)
/// ```
#[test]
fn raise_3_matches_the_oracle_at_the_cli_boundary() {
    let cases: &[(&str, &str, i32)] = &[
        // A valid trace: `class:reason` is raised (the frames are echoed, not printed).
        (
            "erlang:raise(error, foo, [{bar,baz,0}])",
            "exception error foo",
            1,
        ),
        ("erlang:raise(error, foo, [])", "exception error foo", 1),
        ("erlang:raise(error, {a,b}, [])", "exception error {a,b}", 1),
        (
            "erlang:raise(exit, {shutdown,1}, [])",
            "exception exit {shutdown,1}",
            1,
        ),
        ("erlang:raise(throw, x, [])", "exception throw x", 1),
        (
            "erlang:raise(error, foo, [{m,f,1},{n,g,2}])",
            "exception error foo",
            1,
        ),
        // An invalid class or stacktrace shape is the *returned* atom `badarg`.
        ("erlang:raise(foo, foo, [])", "ok badarg", 0),
        ("erlang:raise(1, foo, [])", "ok badarg", 0),
        ("erlang:raise(error, foo, [42])", "ok badarg", 0),
        ("erlang:raise(error, foo, x)", "ok badarg", 0),
        ("erlang:raise(error, foo, [{}])", "ok badarg", 0),
        ("erlang:raise(error, foo, [{m,f}])", "ok badarg", 0),
    ];
    for (expr, expected, expected_exit) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, *expected_exit, "{expr}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }
}

/// The two `semantics/*` corpus probes this suite pins — a binary comprehension and
/// `bit_size/1` of a **non-byte-aligned** bitstring — AGREE with the oracle. Pinned here —
/// invoking the **same** probe wrapper and argv the corpus builds — so the capability cannot
/// silently regress, even where the oracle is unavailable.
#[test]
fn the_binary_segment_probes_agree_with_the_oracle() {
    let cases: &[(&str, &str)] = &[
        ("<< <<(X+1)>> || <<X>> <= <<1,2,3>> >>", "ok <<2,3,4>>"),
        ("bit_size(<<1:1>>)", "ok 1"),
    ];

    for (expr, expected) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, 0, "{expr}: exit");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }
}

/// The five `boot/*` corpus probes this suite pins: an `erlang:spawn/1` (or
/// `spawn_link/1`) process sends its parent a message, which the parent's `receive` picks up
/// from the mailbox. Each **AGREES** with the oracle.
///
/// Each expression is the corpus's own text (`tools/differential/src/corpus/boot.rs`) and is
/// run through the **same** probe wrapper and argv the corpus builds, so the capability can
/// never silently regress even where the oracle `erl` is unavailable. The `receive … after
/// 1000 -> timeout` fallback in each probe makes a lost or dropped message an observable
/// `ok timeout` divergence rather than a hang — and every golden below is the oracle's own
/// `ok <value>` line.
#[test]
fn the_boot_spawn_probes_agree_with_the_oracle() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "boot/receive_message_from_spawned_process",
            "begin Self = self(), spawn(fun() -> Self ! hello end), \
             receive M -> M after 1000 -> timeout end end",
            "ok hello",
        ),
        (
            "boot/receive_computed_value_from_spawned_process",
            "begin Self = self(), spawn(fun() -> Self ! 21 * 2 end), \
             receive M -> M after 1000 -> timeout end end",
            "ok 42",
        ),
        (
            "boot/receive_pattern_match_from_spawned_process",
            "begin Self = self(), spawn(fun() -> Self ! {self(), ok} end), \
             receive {From, _} -> From =/= Self after 1000 -> timeout end end",
            "ok true",
        ),
        (
            "boot/receive_ping_pong_between_processes",
            "begin Self = self(), P = spawn(fun() -> \
             receive {ping, From} -> From ! pong end end), \
             P ! {ping, Self}, receive M -> M after 1000 -> timeout end end",
            "ok pong",
        ),
        (
            "boot/spawn_link_notifies_caller",
            "begin Self = self(), spawn_link(fun() -> Self ! linked end), \
             receive M -> M after 1000 -> timeout end end",
            "ok linked",
        ),
    ];

    for (id, expr, expected) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{id}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, 0, "{id}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{id}: stderr {stderr:?}");
    }
}

/// The honest boundary underneath the `spawn` capability: a **bare** `receive` (no `after`)
/// in `-eval` has no host timer to block on, and with nothing runnable to deliver a message it
/// is a typed *capability gap* — exit 3 with exactly one `beam_rt: unsupported: …` line, never
/// a hang and never a wrong `ok`.
///
/// The `receive … after 1000` fallback the `spawn` probes above rely on is what keeps them
/// observable; this pins the complementary case. The gap is uncatchable (it travels as an
/// uncatchable control signal), so the probe's own `try … catch` cannot turn it into an
/// apparent `exception`, and stdout stays empty.
#[test]
fn a_bare_receive_with_no_sender_is_unsupported_not_a_hang() {
    let (stdout, exit, stderr) = probe("receive X -> X end", &[], Kind::Value);
    assert_eq!(exit, 3, "stdout {stdout:?} stderr {stderr:?}");
    assert!(stdout.is_empty(), "stdout {stdout:?}");
    assert!(
        stderr.starts_with("beam_rt: unsupported: "),
        "stderr: {stderr:?}"
    );
    assert!(!stderr.contains("Usage:"), "stderr: {stderr:?}");
}

/// The A.4 driver probe the differential corpus carries
/// (`driver/port_backed_by_spawn_driver`): `erlang:open_port/2` **really** starts an OS
/// process through the `spawn` driver and yields a real internal port.
///
/// Pinned at the CLI boundary with the corpus's own argv and probe wrapper, so the
/// **wiring** cannot silently regress: the shared `PortRegistry` on
/// [`Runtime`](beam_rt::Runtime), the boot-seeded `port_info` atom vocabulary
/// ([`PORT_INFO_ATOMS`](beam_rt::PORT_INFO_ATOMS), interned by
/// [`seed_core_atoms`](beam_rt::boot::seed_core_atoms)) and the `PortHook` that
/// `RuntimeContext::call_registered` installs are all on this path. The `src/port.rs`
/// unit tests cover the provider in isolation; this covers it end to end.
///
/// The item values that depend on the *dynamic* owner pid or port number are probed by
/// **shape** (`is_*`/`element/2`) rather than by a literal golden: `links` is a list, `id`
/// an integer, `connected` a pid — the oracle's own answer shapes. `stream` is not an item
/// at all, so `erlang:port_info(P, stream)` raises `error:badarg` (the oracle's answer).
#[test]
fn open_port_spawns_a_real_os_process() {
    let cases: &[(&str, &str, i32)] = &[
        // The corpus's own A.4 probe — the gate case.
        ("is_port(open_port({spawn,\"true\"},[]))", "ok true", 0),
        // A real OS process: the port answers with a real integer `os_pid` (the `/2` arity
        // returns the `{Item, Value}` pair, so the value is `element/2`).
        (
            "is_integer(element(2, erlang:port_info(open_port({spawn,\"true\"},[]), os_pid)))",
            "ok true",
            0,
        ),
        // The `port_info/1` item list is well formed …
        (
            "is_list(erlang:port_info(open_port({spawn,\"true\"},[])))",
            "ok true",
            0,
        ),
        // … `links` is a list (the owner as a one-element list), `id` a bare integer and
        // `connected` the owner pid …
        (
            "is_list(element(2, erlang:port_info(open_port({spawn,\"true\"},[]), links)))",
            "ok true",
            0,
        ),
        (
            "is_integer(element(2, erlang:port_info(open_port({spawn,\"true\"},[]), id)))",
            "ok true",
            0,
        ),
        (
            "is_pid(element(2, erlang:port_info(open_port({spawn,\"true\"},[]), connected)))",
            "ok true",
            0,
        ),
        // … while `stream` is **not** an item (the oracle raises `badarg`) …
        (
            "erlang:port_info(open_port({spawn,\"true\"},[]), stream)",
            "exception error badarg",
            1,
        ),
        // … and `port_close/1` kills and reaps the child, answering `true` (the oracle's
        // value: `erts_internal:port_close/1` returns the atom `true`, not `ok`).
        ("erlang:port_close(open_port({spawn,\"true\"},[]))", "ok true", 0),
    ];

    for (expr, expected, expected_exit) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, *expected_exit, "{expr}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }
}

/// The A.4 **port stream read** probe the differential corpus carries
/// (`driver/spawn_driver_port_stream_read`): the `spawn` driver's stdout is polled and its
/// bytes are delivered as a `{Port, {data, Data}}` message.
///
/// This pins the corpus golden exactly (`ok <<104,105,10>>` for `echo hi` on a `binary`
/// stream), plus the boundary behaviours the same path owns: the `list` default delivers a
/// char list, the bound port variable in a `receive` clause matches the message's port
/// term, an `after` still fires the fallback when the child produces no output, and a
/// `binary` stream wider than the on-heap limit is an honest *capability gap* (exit 3),
/// never a disagreement with the oracle.
#[test]
fn a_spawn_driver_port_stream_is_read_back_as_a_data_message() {
    let cases: &[(&str, &str)] = &[
        // The corpus's own A.4 probe — the gate case (`echo hi` ⇒ `"hi\n"`).
        (
            "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
             receive {P,{data,D}} -> D after 500 -> no_data end end",
            "ok <<104,105,10>>",
        ),
        // The `list` default delivers a char list, not a binary.
        (
            "begin P = open_port({spawn,\"echo hi\"}, []), \
             receive {P,{data,D}} -> D after 500 -> no_data end end",
            "ok [104,105,10]",
        ),
        // The message is exactly `{Port, {data, Data}}`; the clause's bound `P` matches the
        // message's port term. A shape check the printer *can* render (it cannot print a
        // bare port), verified against the system `erl`.
        (
            "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
             receive {P,{data,D}} -> {is_port(P), is_binary(D), D} \
             after 500 -> no_data end end",
            "ok {true,true,<<104,105,10>>}",
        ),
        // A child with no output still fires the `after` fallback (never a hang).
        (
            "begin P = open_port({spawn,\"true\"}, [binary]), \
             receive {P,{data,D}} -> D after 200 -> no_data end end",
            "ok no_data",
        ),
    ];

    for (expr, expected) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, 0, "{expr}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }

    // The message shape asserted structurally: `{Port, {data, <<"hi\n">>}}`.
    let (stdout, exit, stderr) = probe(
        "begin P = open_port({spawn,\"echo hi\"}, [binary]), \
         receive M -> \
         {is_tuple(M), tuple_size(M), element(1, M) =:= P, \
          is_tuple(element(2, M)), element(1, element(2, M)), element(2, element(2, M))} \
         after 500 -> no_data end end",
        &[],
        Kind::Value,
    );
    assert_eq!(exit, 0, "shape probe: exit (stderr {stderr:?})");
    assert_eq!(
        stdout,
        "ok {true,2,true,true,data,<<104,105,10>>}\n",
        "shape probe: stdout (stderr {stderr:?})"
    );
}

/// The A.4 **`port_command/2,3` write** probe the differential corpus carries
/// (`driver/port_command_write_read`): `port_command/2` writes to the spawn child's stdin
/// and its echo reads back through the same poll path as any other port output.
///
/// The corpus expression is pinned exactly (`cat` echoes `"hello"` ⇒
/// `ok [104,101,108,108,111]`), together with the data forms the write accepts (a binary,
/// a nested iolist, arbitrary bytes above 127), the `/3` arity's options, and the two
/// `badarg` shapes the oracle raises (a non-port argument, and a port the runtime no longer
/// holds).
#[test]
fn port_command_writes_to_the_spawn_child_and_its_echo_reads_back() {
    let cases: &[(&str, &str, i32)] = &[
        // The corpus's own A.4 probe — the gate case.
        (
            "begin P = open_port({spawn,\"cat\"}, []), port_command(P, \"hello\"), \
             receive {P,{data,D}} -> D after 1000 -> no_data end end",
            "ok [104,101,108,108,111]",
            0,
        ),
        // A `binary`-stream port echoes the bytes as a binary.
        (
            "begin P = open_port({spawn,\"cat\"}, [binary]), port_command(P, \"hello\"), \
             receive {P,{data,D}} -> D after 1000 -> no_data end end",
            "ok <<104,101,108,108,111>>",
            0,
        ),
        // A binary and a nested iolist are iodata; a byte above 127 goes through untouched
        // (the stream is bytes, not text).
        (
            "begin P = open_port({spawn,\"cat\"}, []), port_command(P, <<255,0,1>>), \
             port_command(P, [<<\"ab\">>, [99]]), \
             receive {P,{data,D}} -> D after 1000 -> no_data end end",
            "ok [255,0,1,97,98,99]",
            0,
        ),
        // `port_command/3` takes an option list (`[]` here) and answers `true`.
        (
            "begin P = open_port({spawn,\"cat\"}, []), Res = port_command(P, \"hello\", []), \
             receive {P,{data,D}} -> {Res,D} after 1000 -> {Res,no_data} end end",
            "ok {true,[104,101,108,108,111]}",
            0,
        ),
        // A non-port argument is `badarg` (the oracle's own class and reason). The variable
        // is deliberately not named `R`: the probe's `catch C:R` would make it unsafe.
        (
            "port_command(5, \"x\")",
            "exception error badarg",
            1,
        ),
        // A port the runtime no longer holds is `badarg` too — never a silent success.
        (
            "begin P = open_port({spawn,\"cat\"},[]), erlang:port_close(P), \
             port_command(P, \"x\") end",
            "exception error badarg",
            1,
        ),
    ];

    for (expr, expected, expected_exit) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, *expected_exit, "{expr}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }
}

/// The two A.4 **port lifecycle** divergences the native floor and the poller close: a
/// `port_info/1,2` on a port the runtime no longer holds answers the atom `undefined` (the
/// oracle's value — *not* `badarg` and not an empty list), and a port opened with the
/// `exit_status` option reports its reaped child's status as `{Port, {exit_status, Status}}`.
///
/// Every golden here is the system oracle's own answer, including the two boundaries: a
/// port opened *without* `exit_status` never gets an exit message (the `after` fallback
/// fires), and a normally-exiting child reports status `0`.
#[test]
fn port_info_after_close_and_exit_status_follow_the_oracle() {
    let cases: &[(&str, &str)] = &[
        // Closed port: `undefined` for both arities (verified against OTP 29/erts-17.1).
        (
            "begin P = open_port({spawn,\"true\"},[]), erlang:port_close(P), \
             erlang:port_info(P) end",
            "ok undefined",
        ),
        (
            "begin P = open_port({spawn,\"true\"},[]), erlang:port_close(P), \
             erlang:port_info(P, name) end",
            "ok undefined",
        ),
        // A live port keeps its item list (the native floor must not shadow it).
        (
            "is_list(erlang:port_info(open_port({spawn,\"true\"},[])))",
            "ok true",
        ),
        // The `exit_status` option: the reaped child's status is delivered.
        (
            "begin P = open_port({spawn,\"sh -c \\\"exit 3\\\"\"},[exit_status]), \
             receive {P,{exit_status,S}} -> S after 2000 -> no_data end end",
            "ok 3",
        ),
        (
            "begin P = open_port({spawn,\"true\"},[exit_status]), \
             receive {P,{exit_status,S}} -> S after 2000 -> no_data end end",
            "ok 0",
        ),
        // Without the option there is no exit message at all — the fallback fires.
        (
            "begin P = open_port({spawn,\"true\"},[]), \
             receive {P,{exit_status,S}} -> S after 300 -> no_data end end",
            "ok no_data",
        ),
    ];

    for (expr, expected) in cases {
        let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
        assert_eq!(
            stdout,
            format!("{expected}\n"),
            "{expr}: stdout (stderr {stderr:?})"
        );
        assert_eq!(exit, 0, "{expr}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");
    }
}

/// A `binary`-stream read wider than the on-heap binary limit (64 bytes) is **delivered at
/// full width** — the oracle's own behaviour — never a typed capability gap.
///
/// `open_port({spawn,"head -c 100 /dev/zero"}, [binary])` writes its 100 bytes in one
/// `write(2)`, and the read path hands over up to `READ_CHUNK` (64 KiB) of one `read(2)`, so
/// the first `{Port, {data, D}}` clause binds the whole 100-byte `binary()`. The value is
/// printed with `~w`, so stdout is exactly `ok <<0,0,…,0>>` — one hundred `0` elements — and
/// the process exits 0 with an empty stderr. The oracle comparison at the end cross-checks
/// that whole line against the system `erl` (a loud skip when no `erl` is available).
#[test]
fn an_oversized_port_stream_binary_is_delivered_at_full_width() {
    let expr = concat!(
        r#"begin P = open_port({spawn,"head -c 100 /dev/zero"}, [binary]), "#,
        "receive {P,{data,D}} -> D after 500 -> no_data end end"
    );
    let (stdout, exit, stderr) = probe(expr, &[], Kind::Value);
    assert_eq!(exit, 0, "stdout {stdout:?} / stderr {stderr:?}");
    assert!(stderr.is_empty(), "stderr {stderr:?}");
    // `~w` renders a binary as `<<b1,b2,…>>`, so the line is `ok <<0,0,…,0>>`: the byte list
    // between the delimiters must be the *whole* 100-byte read, every element a zero.
    let rendered = stdout
        .strip_prefix("ok <<")
        .and_then(|rest| rest.strip_suffix(">>\n"))
        .unwrap_or_else(|| panic!("expected a `~w`-printed binary, got {stdout:?}"));
    let elements: Vec<&str> = rendered.split(',').collect();
    assert_eq!(
        elements.len(),
        100,
        "the 100-byte read is delivered as one binary: {stdout:?}"
    );
    assert!(
        elements.iter().all(|element| *element == "0"),
        "every byte of /dev/zero is a NUL: {stdout:?}"
    );

    // Oracle cross-check: the system `erl` prints the very same line. A missing oracle is a
    // loud skip, never a silent pass.
    if let Some(oracle) = support::require_oracle("eval") {
        match support::oracle_eval(&oracle, None, &wrap(expr, Kind::Value)) {
            Some(oracle_stdout) => assert_eq!(
                stdout.trim_end(),
                oracle_stdout,
                "the runtime delivers the same binary the oracle does"
            ),
            None => eprintln!("SKIP the live-oracle comparison: the erl probe could not answer"),
        }
    }
}
/// A `-eval` expression outside the grammar is a *capability gap* — exit 3 with exactly/// one `beam_rt: unsupported: …` line. `tests/cli.rs` asserts the shape; here it is
/// re-asserted through the probe helper so the corpus cannot regress into a silent 0.
#[test]
fn an_unparseable_probe_is_unsupported_not_wrong() {
    // `(` cannot be parsed: the evaluator reports `unsupported`, never a wrong result.
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(["-noshell", "-eval", "try (1 +) of V -> V catch C:R -> R end."])
        .output()
        .expect("spawning beam_rt failed");
    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("beam_rt: unsupported: "), "stderr: {stderr:?}");
    assert!(!stderr.contains("Usage:"), "stderr: {stderr:?}");
}

/// The corpus's `wire/new_fun_ext_encode` case stays the single honest **capability gap**.
///
/// `term_to_binary/1` of an `erl_eval` template closure (a `-eval` lambda) has no
/// faithfully derivable `NEW_FUN_EXT` image — the oracle's is a closure over
/// `erl_eval.erl`'s own arity-specialized `expr/6` lambda, which this runtime cannot
/// reproduce — so the CLI exits 3 with exactly one `beam_rt: unsupported: … local_fun`
/// line rather than emitting a *wrong* image. The `LocalFunIdentity` seam is scoped to a
/// **compiled** module's local fun (see `src/context/fun_identity.rs`'s oracle test), and
/// an `-eval` context has no calling module, so the hook is a clean miss here.
#[test]
fn an_eval_lambda_has_no_new_fun_ext_image_and_stays_unsupported() {
    let (stdout, exit, stderr) = probe("term_to_binary(fun() -> ok end)", &[], Kind::Value);
    assert!(stdout.is_empty(), "stdout: {stdout:?}");
    assert_eq!(exit, 3, "stdout: {stdout:?} stderr: {stderr:?}");
    assert_eq!(
        stderr, "beam_rt: unsupported: local_fun\n",
        "the -eval lambda must stay the typed local_fun gap, never an image"
    );
}

/// Runs the raw `io:format("~p~n",[<expr>]),halt(0).` probe at the CLI boundary and
/// returns `(trimmed_stdout, exit_code, stderr)`.
///
/// This is the *unwrapped* form the differential harness uses for an `-eval` case that
/// prints its own result (the corpus's `try … catch` wrapper is for an uncaught error);
/// it exercises the whole parser → evaluator → `~p` printer → CLI path.
fn raw_probe(expr: &str) -> (String, i32, String) {
    let probe_expr = format!(r#"io:format("~p~n",[{expr}]),halt(0)."#);
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(["-noshell", "-eval", probe_expr.as_str()])
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt for {expr:?} failed: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit = output
        .status
        .code()
        .expect("beam_rt always exits normally (never killed by a signal)");
    (stdout, exit, stderr)
}

/// `catch Expr` and `try … catch C:R:S -> … end` render the **supplied** `erlang:raise/3`
/// stacktrace, matching the system oracle (contract A.1, exception semantics).
///
/// The oracle's rule (`lib/stdlib/src/erl_eval.erl`) folds `catch` **by class**: a `throw`
/// yields the reason, an `exit` yields `{'EXIT', Reason}` (never a trace, even when one was
/// supplied), and an `error` yields `{'EXIT', {Reason, Stacktrace}}` **always** — including a
/// non-atom reason, which keeps the trace. A `try` clause's 3rd pattern binds the same
/// cooked `[{M,F,A,Location}]` list. Each golden below is the oracle's *own* output
/// (OTP 29 / erts-17.1) and is cross-checked against it live (a loud skip when no `erl` is
/// available, never a silent pass).
#[test]
fn a_caught_raise_renders_its_supplied_stacktrace() {
    let cases: &[(&str, &str)] = &[
        // `error` keeps the supplied trace.
        (
            "catch erlang:raise(error,r,[{m,f,0}])",
            "{'EXIT',{r,[{m,f,0,[]}]}}",
        ),
        // A located frame keeps its `file`/`line`.
        (
            r#"catch erlang:raise(error,r,[{m,f,1,[{file,"m.erl"},{line,7}]}])"#,
            r#"{'EXIT',{r,[{m,f,1,[{file,"m.erl"},{line,7}]}]}}"#,
        ),
        // An empty supplied trace is still a 2-tuple.
        ("catch erlang:raise(error,r,[])", "{'EXIT',{r,[]}}"),
        // `exit` drops the trace even when one was supplied.
        (
            "catch erlang:raise(exit,{shutdown,1},[{m,f,0}])",
            "{'EXIT',{shutdown,1}}",
        ),
        // The rule is class-based, not reason-shape-based: a non-atom `error` reason keeps it.
        (
            "catch erlang:raise(error,{badmatch,1},[{m,f,0}])",
            "{'EXIT',{{badmatch,1},[{m,f,0,[]}]}}",
        ),
        // A `try` clause's stacktrace pattern binds the same cooked list.
        (
            "try erlang:raise(error,r,[{m,f,0}]) catch C:R:S -> {C,R,S} end",
            "{error,r,[{m,f,0,[]}]}",
        ),
    ];

    let oracle = support::require_oracle("eval caught-raise stacktrace");
    for (expr, golden) in cases {
        let (stdout, exit, stderr) = raw_probe(expr);
        assert_eq!(stdout, *golden, "{expr}: stdout (stderr {stderr:?})");
        assert_eq!(exit, 0, "{expr}: exit (stderr {stderr:?})");
        assert!(stderr.is_empty(), "{expr}: stderr {stderr:?}");

        // Live oracle cross-check: the system `erl` prints the very same line. A missing
        // oracle (or a probe that cannot answer) is a loud skip, never a silent pass.
        if let Some(oracle) = &oracle {
            let probe_expr = format!(r#"io:format("~p~n",[{expr}]),halt(0)."#);
            match support::oracle_eval(oracle, None, &probe_expr) {
                Some(oracle_stdout) => assert_eq!(
                    stdout, oracle_stdout,
                    "{expr}: the runtime renders the trace the oracle does"
                ),
                None => eprintln!(
                    "SKIP the live-oracle comparison for {expr:?}: the erl probe could not answer"
                ),
            }
        }
    }
}
