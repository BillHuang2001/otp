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

/// The two `semantics/*` corpus probes this pass closed — a binary comprehension and
/// `bit_size/1` of a **non-byte-aligned** bitstring. Both were previously `unsupported`
/// (the `-eval` grammar had no binary generator and could not enter a non-byte-aligned
/// segment); they now AGREE with the oracle. Pinned here — invoking the **same** probe
/// wrapper and argv the corpus builds — so the fix cannot silently regress, even where
/// the oracle is unavailable.
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
/// The three `port_info/2` goldens that differ from erts are this crate's **documented**
/// contract (`crate::port`): `connected` is a boolean where erts reports the controlling
/// pid, and `links`/`monitors` are genuinely empty because nothing links or monitors a
/// port yet.
#[test]
fn open_port_spawns_a_real_os_process() {
    let cases: &[(&str, &str)] = &[
        // The corpus's own A.4 probe — the gate case.
        ("is_port(open_port({spawn,\"true\"},[]))", "ok true"),
        // A real OS process: the port answers with a real integer `os_pid`.
        (
            "is_integer(erlang:port_info(open_port({spawn,\"true\"},[]), os_pid))",
            "ok true",
        ),
        // The `port_info/1` item list is well formed …
        (
            "is_list(erlang:port_info(open_port({spawn,\"true\"},[])))",
            "ok true",
        ),
        // … nothing links or monitors such a port …
        ("erlang:port_info(open_port({spawn,\"true\"},[]), links)", "ok []"),
        // … `list` is the default stream type and the port is connected …
        ("erlang:port_info(open_port({spawn,\"true\"},[]), stream)", "ok list"),
        (
            "erlang:port_info(open_port({spawn,\"true\"},[]), connected)",
            "ok true",
        ),
        // … and `port_close/1` kills and reaps the child, answering `ok`.
        ("erlang:port_close(open_port({spawn,\"true\"},[]))", "ok ok"),
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

/// A `binary`-stream read wider than the on-heap binary limit (64 bytes) is a typed
/// *capability gap*: the CLI exits 3 with the honest `unsupported:` line, never a wrong
/// result and never a disagreement with the oracle (which would print the binary).
#[test]
fn an_oversized_port_stream_binary_is_unsupported_not_wrong() {
    let output = Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args([
            "-noshell",
            "-eval",
            "try (begin P = open_port({spawn,\"head -c 100 /dev/zero\"}, [binary]), \
             receive {P,{data,D}} -> D after 500 -> no_data end end) of V -> \
             io:format(\"ok ~w~n\", [V]), halt(0) catch C:R -> \
             io:format(\"exception ~w ~w~n\", [C, R]), halt(1) end.",
        ])
        .output()
        .expect("spawning beam_rt failed");
    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("beam_rt: unsupported: "),
        "stderr: {stderr:?}"
    );
    assert!(!stderr.contains("Usage:"), "stderr: {stderr:?}");
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
