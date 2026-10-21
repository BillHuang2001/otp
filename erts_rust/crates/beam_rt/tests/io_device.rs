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

//! The `io` **device floor** — `io:put_chars/1,2`, `io:nl/0,1`, `io:fwrite/1,2,3`,
//! `io:write/1,2` and `io:format/3` — end to end at the **executable** boundary and
//! cross-checked against the live system `erl` oracle.
//!
//! # Why these keys are served natively
//!
//! Each one is an `io.erl` wrapper over `o_request/2` (`lib/stdlib/src/io.erl:160-207`),
//! which resolves the request's **device** and posts `{io_request, self(), Mref, Request}` to
//! it, blocking in a `receive` for `{io_reply, Mref, Reply}`. The default device is
//! `group_leader()`, which here is a process that runs no io server, so that round trip never
//! completes — and an **explicit `self()`** device is `execute_request/3`'s first clause, the
//! oracle's `error:calling_self`. The floor serves the keys directly (`src/native/io.rs`), so
//! the request never leaves the runtime and the device question is answered natively; the
//! rendering still goes through the **registered** `io_lib:format/2`.
//!
//! `io:format/3` is `(Device, Format, Args)` — OTP has **no** options-taking `io:format/3`
//! (`io.erl:1350`), so `io:format("~p~n", [[1,2]], [])` passes the format string as the
//! device and is the bare `function_clause` on both runtimes. The `{chars_limit, _}` options
//! belong to `io_lib:format/3`, which is real `io_lib` code and is cross-checked separately.
//!
//! # What is compared
//!
//! Every probe is run under the built `beam_rt` binary ([`CARGO_BIN_EXE_beam_rt`]) and, where
//! the toolchain allows, under the system `erl`, and the **exit code** and the **stdout
//! bytes** must be equal (`differential`). The oracle is the expected value, so a divergence
//! shows up as a mismatch instead of being hardcoded away. Every probe additionally pins the
//! **exact** bytes, so the suite still discriminates without a toolchain. The
//! `standard_error` probes compare the **stderr** bytes as well
//! (`differential_with_stderr`) — those bytes must land on fd 2, not stdout.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).
//!
//! # The deliberate non-parity
//!
//! One probe is **not** oracle-equal by design, and is pinned as such rather than dropped:
//! a **live non-stdio device** (a registered pid other than the caller's group leader or the
//! `standard_error` device) needs the `{io_request, …}`/`{io_reply, …}` round trip, which a
//! BIF may not perform — this runtime answers the typed `unsupported` (exit 3) instead of
//! blocking, so `a_live_non_stdio_device_is_a_typed_gap` asserts only *this* runtime's side.
//! Every device-taking arm — `io:put_chars/2` included — shares that one model.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and its
// toolchain discovery; the unused remainder is expected here — the same shape `tests/io_scan.rs`
// uses.
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::Output;

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and its **raw** stdout/stderr bytes.
///
/// The bytes are kept un-trimmed on purpose: `io:nl/0` prints a lone newline and `io:write/1`
/// prints with **no** trailing newline, so a trimmed string would collapse the two and hide a
/// regression.
struct Run {
    code: i32,
    out: Vec<u8>,
    err: Vec<u8>,
}

impl Run {
    /// The finished probe described by one collected child `output`.
    fn from_output(output: Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: output.stdout,
            err: output.stderr,
        }
    }

    /// stdout as a lossy string, for diagnostics.
    fn out_str(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    /// stderr as a lossy string, for diagnostics.
    fn err_str(&self) -> String {
        String::from_utf8_lossy(&self.err).into_owned()
    }
}

/// Runs `program args` under the shared bounded runner (`/dev/null` stdin, killed at
/// `support::PROBE_TIMEOUT`), failing loudly when the probe could not be run at all.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!(
            "`{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Run::from_output(output)
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// `erl -noshell -eval <expr> -s init stop` through the oracle. The node is bounded: the
/// probe always gets a terminator, and the shared runner kills it at the bound.
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    run(oracle, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

// ---------------------------------------------------------------------------------------
// The differential helpers
// ---------------------------------------------------------------------------------------

/// Runs `expr` under the built binary **and** under the live oracle, asserting that the
/// **exit code** and the **stdout bytes** are equal. Returns this runtime's run so the caller
/// can pin the exact bytes.
///
/// A missing `erl` is a loud skip (`support::require_oracle`) — the caller's pinned assertion
/// still discriminates, so the probe is never a silent pass.
///
/// stderr is deliberately **not** compared here: this runtime's CLI prefixes a runtime error
/// with `beam_rt: ` where the oracle starts a crash report, so the two stderrs of a failing
/// probe can never be byte-equal. The `standard_error` device probes use
/// [`differential_with_stderr`] instead.
fn differential(what: &str, expr: &str) -> Run {
    let ours = beam_rt_eval(expr);
    let Some(oracle) = support::require_oracle("io_device") else {
        return ours;
    };
    let theirs = oracle_eval(&oracle, expr);
    assert_eq!(
        ours.code,
        theirs.code,
        "{what}: exit codes differ (beam_rt {} vs oracle {}) — beam_rt stderr: {}",
        ours.code,
        theirs.code,
        ours.err_str()
    );
    assert_eq!(
        ours.out, theirs.out,
        "{what}: stdout bytes differ — beam_rt {:?} vs oracle {:?}",
        ours.out_str(),
        theirs.out_str()
    );
    ours
}

/// As [`differential`], but the **stderr** bytes must be equal too — the shape the
/// `standard_error` device needs, so a probe can never pass by quietly writing the stderr
/// payload to stdout.
fn differential_with_stderr(what: &str, expr: &str) -> Run {
    let ours = beam_rt_eval(expr);
    let Some(oracle) = support::require_oracle("io_device") else {
        return ours;
    };
    let theirs = oracle_eval(&oracle, expr);
    assert_eq!(
        ours.code,
        theirs.code,
        "{what}: exit codes differ (beam_rt {} vs oracle {}) — beam_rt stderr: {}",
        ours.code,
        theirs.code,
        ours.err_str()
    );
    assert_eq!(
        ours.out, theirs.out,
        "{what}: stdout bytes differ — beam_rt {:?} vs oracle {:?}",
        ours.out_str(),
        theirs.out_str()
    );
    assert_eq!(
        ours.err, theirs.err,
        "{what}: stderr bytes differ — beam_rt {:?} vs oracle {:?}",
        ours.err_str(),
        theirs.err_str()
    );
    ours
}

/// Asserts this runtime's probe succeeded.
fn assert_ok(what: &str, run: &Run) {
    assert_eq!(
        run.code, 0,
        "{what}: beam_rt exited {} — {}",
        run.code,
        run.err_str()
    );
}

/// The probe of a **successful** `io:*` call whose payload lands on **stdout**: runs the
/// differential, asserts exit 0 and compares stdout to the pinned `expected` bytes exactly.
fn pinned_stdout(what: &str, expr: &str, expected: &[u8]) -> Run {
    let run = differential(what, expr);
    assert_ok(what, &run);
    assert_eq!(
        run.out,
        expected,
        "{what}: stdout bytes — got {:?}",
        run.out_str()
    );
    run
}

/// The probe of a **successful** `io:*` call whose payload lands on **stderr**: the
/// differential (stderr included), exit 0, and nothing on stdout.
fn pinned_stderr(what: &str, expr: &str, expected: &[u8]) {
    let run = differential_with_stderr(what, expr);
    assert_ok(what, &run);
    assert!(
        run.out.is_empty(),
        "{what}: nothing may reach stdout — got {:?}",
        run.out_str()
    );
    assert_eq!(
        run.err,
        expected,
        "{what}: stderr bytes — got {:?}",
        run.err_str()
    );
}

// ---------------------------------------------------------------------------------------
// `io:nl/0,1`
// ---------------------------------------------------------------------------------------

/// `io:nl/0` writes exactly one newline to the caller's default device (the group leader
/// this runtime stands in for) and answers `ok`. The payload is a lone `\n`, so the pinned
/// comparison is on the raw bytes, not a trimmed string.
#[test]
fn io_nl_0_writes_exactly_one_newline() {
    let run = pinned_stdout("io:nl/0", "io:nl(), halt(0).", b"\n");
    assert!(run.err.is_empty(), "io:nl/0 writes nothing to stderr");
}

/// `io:nl/1`'s device argument is honoured for every device this runtime serves: the
/// `standard_io` rewrite, the `user` terminal name, and the caller's **group-leader pid** (a
/// distinct process here — the caller's *own* pid is `error:calling_self` instead, see
/// [`an_explicit_self_device_is_the_oracles_calling_self`]) all select the output stream.
#[test]
fn io_nl_1_honours_the_device() {
    for (device, expr) in [
        ("standard_io", "io:nl(standard_io), halt(0)."),
        ("user", "io:nl(user), halt(0)."),
        ("group_leader()", "io:nl(group_leader()), halt(0)."),
    ] {
        pinned_stdout(&format!("io:nl({device})"), expr, b"\n");
    }
}

// ---------------------------------------------------------------------------------------
// `io:fwrite/1,2,3`
// ---------------------------------------------------------------------------------------

/// `io:fwrite/1` is `io:fwrite(standard_io, Format, [])` (`io.erl:794-804`), so the format
/// renders with an empty argument list on the default device.
#[test]
fn io_fwrite_1_renders_with_an_empty_argument_list() {
    pinned_stdout("io:fwrite/1", "io:fwrite(\"f1~n\"), halt(0).", b"f1\n");
}

/// `io:fwrite/2` renders the format with its argument list through the registered
/// `io_lib:format/2` — the same implementation `io.erl`'s own server calls.
#[test]
fn io_fwrite_2_renders_the_format() {
    pinned_stdout("io:fwrite/2", "io:fwrite(\"f2 ~p~n\",[42]), halt(0).", b"f2 42\n");
}

/// `io:fwrite/3`'s first argument is the **device**, and `standard_io` selects the output
/// stream (`request/3` rewrites it to `group_leader()`).
#[test]
fn io_fwrite_3_honours_the_device() {
    pinned_stdout(
        "io:fwrite(standard_io, ...)",
        "io:fwrite(standard_io,\"f3 ~s~n\",[\"x\"]), halt(0).",
        b"f3 x\n",
    );
    pinned_stdout(
        "io:fwrite(user, ...)",
        "io:fwrite(user,\"f4~n\",[]), halt(0).",
        b"f4\n",
    );
}

// ---------------------------------------------------------------------------------------
// `io:write/1,2`
// ---------------------------------------------------------------------------------------

/// `io:write/1` writes the `~w` representation and — unlike `io:fwrite/2` — appends **no**
/// newline, so the pinned bytes are the term's rendering alone.
#[test]
fn io_write_1_prints_no_trailing_newline() {
    pinned_stdout("io:write(hello)", "io:write(hello), halt(0).", b"hello");
    // `~w` (which `io:write/1` is) does not quote a printable atom with a space the way `~p`
    // does — the rendering is the oracle's own.
    pinned_stdout(
        "io:write('hello world')",
        "io:write('hello world'), halt(0).",
        b"'hello world'",
    );
    pinned_stdout(
        "io:write([97,98,99])",
        "io:write([97,98,99]), halt(0).",
        b"[97,98,99]",
    );
}

/// `io:write/2` takes the device **first**, and `standard_io`/`user` select the output
/// stream.
#[test]
fn io_write_2_honours_the_device() {
    pinned_stdout(
        "io:write(standard_io, {a,1})",
        "io:write(standard_io, {a,1}), halt(0).",
        b"{a,1}",
    );
    pinned_stdout(
        "io:write(user, hi)",
        "io:write(user, hi), halt(0).",
        b"hi",
    );
}

// ---------------------------------------------------------------------------------------
// `io:format/3`
// ---------------------------------------------------------------------------------------

/// `io:format/3` is the same `{format, Format, Args}` request as `io:fwrite/3`, and its
/// explicit device is honoured for the streams this runtime serves.
#[test]
fn io_format_3_honours_the_device() {
    pinned_stdout(
        "io:format(standard_io, ...)",
        "io:format(standard_io,\"g3 ~p~n\",[[1,2]]), halt(0).",
        b"g3 [1,2]\n",
    );
    pinned_stdout(
        "io:format(user, ...)",
        "io:format(user,\"g4~n\",[]), halt(0).",
        b"g4\n",
    );
}

// ---------------------------------------------------------------------------------------
// `standard_error`
// ---------------------------------------------------------------------------------------

/// `standard_error` is the registered stderr device, so every device-taking wrapper that
/// honours its device must put its bytes on **fd 2** — never on stdout. The `io:nl/1` and
/// `io:write/2` forms cover the newline and the un-newlined payload.
#[test]
fn the_standard_error_device_writes_to_stderr() {
    pinned_stderr(
        "io:fwrite(standard_error, ...)",
        "io:fwrite(standard_error,\"e2~n\",[]), halt(0).",
        b"e2\n",
    );
    pinned_stderr(
        "io:format(standard_error, ...)",
        "io:format(standard_error,\"e3~n\",[]), halt(0).",
        b"e3\n",
    );
    pinned_stderr(
        "io:write(standard_error, hi)",
        "io:write(standard_error, hi), halt(0).",
        b"hi",
    );
    pinned_stderr(
        "io:nl(standard_error)",
        "io:nl(standard_error), halt(0).",
        b"\n",
    );
}

// ---------------------------------------------------------------------------------------
// The bad-device answers (oracle-equal)
// ---------------------------------------------------------------------------------------

/// A device atom naming **no registered process** is `request/3`'s `{ErrorTag, arguments}`,
/// which `o_request/2` turns into `error:badarg` — the oracle's own reason, never a silent
/// redirect to stdout. Caught so the probe prints the reason and both runtimes exit 0.
#[test]
fn a_non_live_device_atom_is_the_oracles_badarg() {
    for (what, expr) in [
        (
            "io:nl(foo)",
            "R = try io:nl(foo) catch error:E -> E end, io:format(\"~p~n\",[R]), halt(0).",
        ),
        (
            "io:write(no_such_io_device, hi)",
            "R = try io:write(no_such_io_device, hi) catch error:E -> E end, \
             io:format(\"~p~n\",[R]), halt(0).",
        ),
        (
            "io:format(no_such_io_device, ...)",
            "R = try io:format(no_such_io_device,\"x~n\",[]) catch error:E -> E end, \
             io:format(\"~p~n\",[R]), halt(0).",
        ),
        (
            "io:put_chars(no_such_io_device, ...)",
            "R = try io:put_chars(no_such_io_device,\"x\") catch error:E -> E end, \
             io:format(\"~p~n\",[R]), halt(0).",
        ),
    ] {
        pinned_stdout(what, expr, b"badarg\n");
    }
}

/// A device that is neither an atom nor a pid leaves `o_request/2` with no matching clause,
/// so the oracle raises the **bare** `function_clause` — and so does this floor.
#[test]
fn a_non_device_argument_is_function_clause() {
    pinned_stdout(
        "io:nl(1)",
        "R = try io:nl(1) catch error:E -> E end, io:format(\"~p~n\",[R]), halt(0).",
        b"function_clause\n",
    );
    pinned_stdout(
        "io:write(1, hi)",
        "R = try io:write(1, hi) catch error:E -> E end, io:format(\"~p~n\",[R]), halt(0).",
        b"function_clause\n",
    );
    pinned_stdout(
        "io:put_chars(1, ...)",
        "R = try io:put_chars(1, \"x\") catch error:E -> E end, io:format(\"~p~n\",[R]), halt(0).",
        b"function_clause\n",
    );
}

/// An **uncaught** bad device terminates both runtimes with the same exit code and the same
/// (empty) stdout — the exit-status parity, not just the caught reason.
#[test]
fn an_uncaught_bad_device_exits_like_the_oracle() {
    let run = differential("io:nl(foo) uncaught", "io:nl(foo), halt(0).");
    assert_eq!(run.code, 1, "a runtime error is CLI exit 1");
    assert!(
        run.out.is_empty(),
        "an uncaught badarg prints nothing to stdout — got {:?}",
        run.out_str()
    );
}

// ---------------------------------------------------------------------------------------
// The one deliberate non-parity: a live non-stdio device
// ---------------------------------------------------------------------------------------

/// A **live** io device this runtime cannot serve from a BIF (a registered pid that is
/// neither the caller's group leader nor the `standard_error` device) is the typed
/// `unsupported` — exit 3, one `unsupported:` line on stderr, nothing on stdout.
///
/// The oracle is deliberately **not** consulted: it would post an `{io_request, …}` to that
/// process and block for ever in the `receive` (the spawned device never replies), so the
/// oracle side of this probe is not expressible as a terminating comparison.
#[test]
fn a_live_non_stdio_device_is_a_typed_gap() {
    for (what, function) in [
        ("io:nl/1", "io:nl(io_device_probe)"),
        ("io:write/2", "io:write(io_device_probe, hi)"),
        ("io:put_chars/2", "io:put_chars(io_device_probe, \"pp\")"),
        ("io:fwrite/3", "io:fwrite(io_device_probe, \"pf~n\", [])"),
        ("io:format/3", "io:format(io_device_probe, \"pg~n\", [])"),
    ] {
        let expr = format!(
            "P = spawn(fun() -> receive _ -> ok end end), register(io_device_probe, P), \
             {function}, halt(0)."
        );
        let run = beam_rt_eval(&expr);
        assert_eq!(run.code, 3, "{what}: a capability gap is CLI exit 3");
        assert!(
            run.out.is_empty(),
            "{what}: a capability gap prints nothing to stdout — got {:?}",
            run.out_str()
        );
        let err = run.err_str();
        assert!(
            err.contains("unsupported") && err.contains("io-device"),
            "{what}: the typed gap must name the live io-device process — stderr: {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// No regressions in the pre-existing `io` floor surface
// ---------------------------------------------------------------------------------------

/// `io:put_chars/1` (the default device) and `io:put_chars/2` on a device this runtime
/// serves write their bytes to the output stream.
#[test]
fn io_put_chars_1_and_2_answer_ok_on_the_output_stream() {
    pinned_stdout("io:put_chars/1", "io:put_chars(\"pc\"), halt(0).", b"pc");
    pinned_stdout(
        "io:put_chars(standard_io, ...)",
        "io:put_chars(standard_io,\"pc2\"), halt(0).",
        b"pc2",
    );
    pinned_stdout(
        "io:put_chars(user, ...)",
        "io:put_chars(user,\"pu\"), halt(0).",
        b"pu",
    );
    pinned_stdout(
        "io:put_chars(group_leader(), ...)",
        "io:put_chars(group_leader(),\"pg\"), halt(0).",
        b"pg",
    );
}

/// `io:put_chars/2`'s device is **honoured**, exactly like the other device-taking arms:
/// the registered `standard_error` device puts its bytes on **fd 2**, never on stdout.
#[test]
fn io_put_chars_2_honours_the_standard_error_device() {
    pinned_stderr(
        "io:put_chars(standard_error, ...)",
        "io:put_chars(standard_error,\"pe\"), halt(0).",
        b"pe",
    );
}

/// `io:format/1,2` are **registered `beam_bifs` BIFs** rather than floor keys, and the floor
/// must not shadow or change them: they render through the same printer and answer `ok`.
#[test]
fn io_format_1_and_2_are_unchanged_by_the_floor() {
    pinned_stdout("io:format/1", "io:format(\"gf1~n\"), halt(0).", b"gf1\n");
    pinned_stdout(
        "io:format/2",
        "io:format(\"gf2 ~p~n\",[7]), halt(0).",
        b"gf2 7\n",
    );
}

// ---------------------------------------------------------------------------------------
// The arity truth: `io:format/3` is `(Device, Format, Args)`
// ---------------------------------------------------------------------------------------

/// `io:format/3` is `(Device, Format, Args)` and OTP has **no** options-taking
/// `io:format/3` (`io.erl:1350`), so the `io:format(Format, Args, Options)`-shaped call
/// passes the format string as the **device** and raises the bare `function_clause` — on the
/// oracle exactly as here.
///
/// This is pinned on purpose: reading the third argument as an options list would turn a
/// genuine oracle agreement into a divergence. The options-taking form is `io_lib:format/3`
/// (real `io_lib` code, never this floor), which answers `[[91,"...",93]]` for
/// `io_lib:format("~p",[[1,2]],[{chars_limit,3}])` on both runtimes.
#[test]
fn the_empty_options_shaped_format_3_is_the_arity_truth() {
    pinned_stdout(
        "io:format(\"~p~n\", [[1,2]], []) caught",
        "R = try io:format(\"~p~n\",[[1,2]],[]) catch error:E -> E end, \
         io:format(\"~p~n\",[R]), halt(0).",
        b"function_clause\n",
    );
    // Uncaught: both runtimes exit 1 and print nothing on stdout.
    let run = differential(
        "io:format(\"~p~n\", [[1,2]], []) uncaught",
        "io:format(\"~p~n\",[[1,2]],[]), halt(0).",
    );
    assert_eq!(run.code, 1, "a runtime error is CLI exit 1");
    assert!(
        run.out.is_empty(),
        "an uncaught function_clause prints nothing to stdout — got {:?}",
        run.out_str()
    );

    // The device-form siblings really render: the `{Device, Format, Args}` reading is the
    // oracle's, so no `function_clause` and no silent `ok` without the bytes.
    pinned_stdout(
        "io:format(standard_io, \"~p~n\", [[1,2]])",
        "io:format(standard_io,\"~p~n\",[[1,2]]), halt(0).",
        b"[1,2]\n",
    );
    pinned_stdout(
        "io:format(user, ...)",
        "io:format(user,\"~p~n\",[[1,2]]), halt(0).",
        b"[1,2]\n",
    );
    pinned_stdout(
        "io:format(group_leader(), ...)",
        "io:format(group_leader(),\"~p~n\",[[1,2]]), halt(0).",
        b"[1,2]\n",
    );
    // `io_lib:format/3` is the options-taking API, and it is byte-identical to the oracle's.
    pinned_stdout(
        "io_lib:format(\"~p\",[[1,2]],[{chars_limit,3}])",
        "io:format(\"~p~n\",[io_lib:format(\"~p\",[[1,2]],[{chars_limit,3}])]), halt(0).",
        b"[[91,\"...\",93]]\n",
    );
}

// ---------------------------------------------------------------------------------------
// The explicit `self()` device: `error:calling_self` (oracle-equal)
// ---------------------------------------------------------------------------------------

/// An **explicit `self()`** device is `execute_request/3`'s first clause (`io.erl:1665`), so
/// the oracle raises `error:calling_self` (`conv_reason/1`, `io.erl:784`) — and so does this
/// floor, on **every** device-taking entry point. The payload is never written, and the
/// reason is never a bare `function_clause` or an `ok`.
#[test]
fn an_explicit_self_device_is_the_oracles_calling_self() {
    for (what, call) in [
        ("io:format/3", "io:format(self(), \"x~n\", [])"),
        ("io:fwrite/3", "io:fwrite(self(), \"x~n\", [])"),
        ("io:put_chars/2", "io:put_chars(self(), \"x\")"),
        ("io:nl/1", "io:nl(self())"),
        ("io:write/2", "io:write(self(), hi)"),
    ] {
        pinned_stdout(
            what,
            &format!("R = try {call} catch error:E -> E end, io:format(\"~p~n\",[R]), halt(0)."),
            b"calling_self\n",
        );
    }
    // Uncaught: both runtimes exit 1 with nothing on stdout — the exit-status parity, not
    // just the caught reason.
    let run = differential("io:nl(self()) uncaught", "io:nl(self()), halt(0).");
    assert_eq!(run.code, 1, "a runtime error is CLI exit 1");
    assert!(
        run.out.is_empty(),
        "an uncaught calling_self prints nothing to stdout — got {:?}",
        run.out_str()
    );
}

/// The whole device matrix, cross-checked against the live oracle: the caught reason and the
/// printed text (the payload plus the `~p` rendering of the reason) must be the oracle's own.
///
/// `standard_error` sends its payload to **fd 2**, so its arm lives in
/// [`the_standard_error_device_keeps_its_payload_on_fd_2`] where the stderr bytes are
/// compared too.
#[test]
fn the_device_matrix_answers_like_the_oracle() {
    for (device, expected) in [
        // Not an atom and not a pid: no `o_request/2` clause matches.
        ("[]", "function_clause\n"),
        ("1", "function_clause\n"),
        // The streams this runtime serves.
        ("standard_io", "x\nok\n"),
        ("user", "x\nok\n"),
        ("group_leader()", "x\nok\n"),
        // The caller itself: `execute_request/3`'s first clause.
        ("self()", "calling_self\n"),
        // An atom naming no registered process.
        ("no_such_io_device", "badarg\n"),
    ] {
        let expr = format!(
            "R = try io:format({device}, \"x~n\", []) catch error:E -> E end, \
             io:format(\"~p~n\",[R]), halt(0)."
        );
        pinned_stdout(
            &format!("io:format({device}, ...)"),
            &expr,
            expected.as_bytes(),
        );
    }
}

/// `standard_error` selects **fd 2** for its payload while the caught reason (`ok`) still
/// reaches stdout, and both streams match the oracle byte for byte.
#[test]
fn the_standard_error_device_keeps_its_payload_on_fd_2() {
    let expr = "R = try io:format(standard_error, \"x~n\", []) catch error:E -> E end, \
                io:format(\"~p~n\",[R]), halt(0).";
    let run = differential_with_stderr("io:format(standard_error, ...)", expr);
    assert_ok("io:format(standard_error, ...)", &run);
    assert_eq!(run.out, b"ok\n", "the caught reason is `ok`");
    assert_eq!(run.err, b"x\n", "the payload lands on stderr");
}
