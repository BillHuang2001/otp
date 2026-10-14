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

//! The **`prim_tty` host suite** (contract A.7, the `prim_*` family): the built-in NIF
//! library ERTS serves from its **static NIF table** (`is_static_nif_module`, not a `dlopen`),
//! pinned end to end against the **live system `erl` oracle**.
//!
//! # What this suite pins
//!
//! | Surface | Shape |
//! |---|---|
//! | `prim_tty:load/0` | the built-in-library resolution (`ok`, where a `dlopen` of a non-existent `prim_tty.so` used to raise `{'EXIT',{case_clause,{error,{load_failed,…}}}}`) |
//! | `sizeof_wchar/0`, `isatty/1` | the NIF answers both runtimes serve identically |
//! | `tty_create/1` | the opaque handle: minted, never a small int, and accepted back by a second served call |
//! | `wcwidth/1`, `isprint/1`, `tty_select/2` | typed `unsupported` capability gaps (exit **3**), never a fabricated width and never catchable |
//! | `tinfo/0`, `tigetnum/1`, `tigetflag/1`, `tigetstr/1`, `tputs/2` | the **documented** no-`HAVE_TERMCAP` deviation (see below) |
//!
//! # Two drivers
//!
//! * The **CLI** (`env!("CARGO_BIN_EXE_beam_rt")`, `-noshell -eval EXPR`) produces the raw
//!   stdout every oracle comparison is made against, byte for byte, and the exit codes the
//!   typed-gap cases key off. Every probe is bounded through `support::run_bounded_with_env`.
//! * A real [`Runtime`] via its `ExecutionContext` seam (`call_bif`) for the one assertion a
//!   `~p` string cannot make: that `tty_create/1`'s handle is **opaque** (not a small
//!   integer, not the atom `undefined`) and that a second served call accepts the *same*
//!   handle back — the resource-lookup round trip `enif_get_resource` performs.
//!
//! # The documented no-`HAVE_TERMCAP` deviation
//!
//! This runtime models an ERTS build **without** `HAVE_TERMCAP`: `setupterm_nif/0`,
//! `tinfo_nif/0`, `tigetnum_nif/1`, `tigetflag_nif/1`, `tigetstr_nif/1` and `tputs_nif/2`
//! answer the C's own `make_enotsup` arm (`{error,enotsup}`) — which is what
//! `erts/emulator/nifs/common/prim_tty_nif.c` compiles to when `HAVE_TERMCAP` is undefined.
//! ncurses terminfo state has no faithful safe-std equivalent, and the arm is
//! **non-raising**, which the boot path requires (`prim_tty:init/1` does
//! `setupterm() =:= ok`). The oracle on this host *is* built with termcap, so it answers a
//! real value here; this suite therefore asserts the runtime's own honest `{error,enotsup}`
//! and, when the oracle is present, only that **the oracle's answer is not** that — it never
//! claims parity for the deviation.
//!
//! # Toolchain
//!
//! The oracle is resolved from the **filesystem** (never by running `erl` merely to probe
//! existence, `support/mod.rs`). Every **CLI** probe additionally needs the *installed OTP*,
//! because it boots the runtime and boot resolves the system code path; `support::require_oracle`
//! is the gate for that and prints the **loud skip** banner once. Nothing here is ever a panic
//! for a missing toolchain, and nothing here is ever a silent pass — the one in-process test
//! ([`tty_create_mints_an_opaque_handle_that_round_trips`]) runs with no toolchain at all,
//! because the `prim_tty` NIF stubs are served from the native floor.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use beam_bifs::NullSink;
use beam_engine::{terms, ExecutionContext};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_small, Eterm};

mod support;

use support::assert_badarg;

// ---------------------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------------------

/// The suite name every loud skip names.
const BANNER: &str = "prim_tty";

/// The live `erl` oracle, resolved **once** (so a missing toolchain is a single loud skip
/// rather than one banner per comparison).
fn oracle() -> Option<PathBuf> {
    static ORACLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    ORACLE
        .get_or_init(|| support::require_oracle(BANNER))
        .clone()
}

/// Gates a CLI probe on the **installed OTP**.
///
/// Every `-eval` probe boots the runtime, and boot resolves the *system code path* — a filesystem
/// probe that falls back to running `erl`. With neither the installation nor the tool `erl`, boot
/// fails (`cannot locate the system code path`) and the probe would report a **boot error**, which
/// is not a comparison at all. `require_oracle` is this crate's gate for exactly that (the shape
/// `tests/system_info_oracle.rs` and the `boot.rs`/`position.rs` suites use): it prints the loud
/// skip banner once, and the test then reports as skipped instead of failing.
///
/// The one in-process test ([`tty_create_mints_an_opaque_handle_that_round_trips`]) does **not**
/// gate on this — the `prim_tty` NIF stubs are served from the native floor, so it runs with no
/// toolchain at all.
fn installation_present() -> bool {
    oracle().is_some()
}

// ---------------------------------------------------------------------------------------
// The two drivers: the CLI probe and the in-process BIF call
// ---------------------------------------------------------------------------------------

/// One finished `beam_rt` invocation: its exit code and captured output.
struct Cli {
    /// The process exit code (`-1` when killed by a signal, which no probe here does).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// Runs the built `beam_rt` binary with `args`, bounded (`support::run_bounded_with_env`), so a
/// probe that blocks cannot hang the suite. `ERL_CRASH_DUMP_SECONDS=0` keeps a failing boot
/// from littering the working directory.
fn beam_rt(args: &[&str]) -> Cli {
    let output = support::run_bounded_with_env(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        args,
        &[("ERL_CRASH_DUMP_SECONDS", Some("0"))],
    )
    .expect("the built beam_rt binary runs to completion within the probe bound");
    Cli {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// This runtime's trimmed stdout for the `-eval` `program`, which must succeed (exit 0).
fn ours(program: &str) -> String {
    let cli = beam_rt(&["-noshell", "-eval", program]);
    assert_eq!(
        cli.code, 0,
        "`{program}` must succeed; stderr:\n{}",
        cli.err
    );
    cli.out
}

/// The oracle's trimmed stdout for the same `-eval` `program`, or `None` — loudly — without a
/// working toolchain or when the probe cannot answer.
fn theirs(program: &str) -> Option<String> {
    let oracle = oracle()?;
    match support::oracle_eval(&oracle, None, program) {
        Some(answer) => Some(answer),
        None => {
            eprintln!("warning: the oracle could not answer `{program}`");
            None
        }
    }
}

/// Runs `module:function(args…)` through the runtime's `ExecutionContext` seam, recording the
/// caller's error (the in-process route every CLI probe also reaches).
fn call(
    runtime: &mut Runtime,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> beam_common::Result<Eterm> {
    let mut sink = NullSink;
    let mut context = RuntimeContext::with_output(runtime, &mut sink);
    context.call_bif(module, function, args.len() as u32, args)
}

// ---------------------------------------------------------------------------------------
// 1. The served answers both runtimes give identically
// ---------------------------------------------------------------------------------------

/// One probe both runtimes are asked **identically**: the `~p` rendering of the call, then
/// `halt(0)`.
struct AgreeingProbe {
    /// The label a failure names.
    name: &'static str,
    /// The `-eval` program, run unchanged under both runtimes.
    program: &'static str,
    /// The oracle's own answer (confirmed against the system `erl`, OTP 29.1.1 / erts-17.1),
    /// so the runtime's answer is checked even with no toolchain present.
    golden: &'static str,
}

/// The `prim_tty` surface both runtimes serve identically.
///
/// `load/0` is the `erlang:load_nif/2` entry the built-in-library resolution answers `ok` for
/// (the module is served from the emulator's static NIF table, so no `prim_tty.so` is ever
/// opened). `sizeof_wchar/0` and `isatty/1` are the two NIFs whose answers do not depend on
/// terminfo; the probes run with stdin at `/dev/null` and stdout/stderr piped, so every
/// standard stream is a non-terminal on both sides.
const AGREEING: &[AgreeingProbe] = &[
    AgreeingProbe {
        name: "load/0",
        program: "io:format(\"~p~n\",[catch prim_tty:load()]),halt(0).",
        golden: "ok",
    },
    AgreeingProbe {
        name: "sizeof_wchar/0",
        program: "io:format(\"~p~n\",[catch prim_tty:sizeof_wchar()]),halt(0).",
        golden: "4",
    },
    AgreeingProbe {
        name: "isatty(stdin)",
        program: "io:format(\"~p~n\",[catch prim_tty:isatty(stdin)]),halt(0).",
        golden: "false",
    },
    AgreeingProbe {
        name: "isatty(stdout)",
        program: "io:format(\"~p~n\",[catch prim_tty:isatty(stdout)]),halt(0).",
        golden: "false",
    },
    AgreeingProbe {
        name: "isatty(stderr)",
        program: "io:format(\"~p~n\",[catch prim_tty:isatty(stderr)]),halt(0).",
        golden: "false",
    },
];

/// `prim_tty:load/0` answers `ok`, and `sizeof_wchar/0` / `isatty/1` answer exactly as the
/// live oracle does — an uncaught `ok`, `4` and three `false`s, compared as raw stdout.
///
/// The `load/0` row is the regression pin for the built-in-library resolution: before it, this
/// runtime `dlopen`ed a non-existent `prim_tty.so` and `load/0` raised
/// `{'EXIT',{case_clause,{error,{load_failed,"…prim_tty.so: cannot open shared object file…"}}}}`,
/// which took the whole kernel supervision tree down on the boot path.
#[test]
fn the_served_prim_tty_answers_match_the_oracle() {
    if !installation_present() {
        return;
    }
    for probe in AGREEING {
        let runtime = ours(probe.program);
        assert_eq!(
            runtime, probe.golden,
            "{}: this runtime's answer",
            probe.name
        );
        if let Some(oracle_answer) = theirs(probe.program) {
            assert_eq!(
                oracle_answer, probe.golden,
                "{}: the oracle's answer",
                probe.name
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// 2. The opaque `tty_create/1` handle
// ---------------------------------------------------------------------------------------

/// `tty_create/1` mints an **opaque** handle and every served call accepts the same handle
/// back.
///
/// The oracle's `prim_tty` module does not export `tty_create/1` (it is a `-nifs` local stub
/// reached from `init/1`; see [`the_oracle_does_not_export_tty_create`]), so there is no
/// oracle answer to compare at the CLI. The contract is therefore pinned structurally through
/// the `ExecutionContext` seam: the success tag is `ok`, the handle is **not** a small
/// integer and **not** the atom `undefined`, and the very same term round-trips through
/// `tty_encoding/1` / `tty_window_size/1` (both of which do the C's `enif_get_resource`
/// lookup, and answer `badarg` for anything that is not a live handle).
#[test]
fn tty_create_mints_an_opaque_handle_that_round_trips() {
    let mut runtime = Runtime::with_defaults().expect("the default runtime");
    let stdout = runtime.intern("stdout");

    let created = call(&mut runtime, "prim_tty", "tty_create", &[stdout]).expect("a served answer");
    assert_eq!(
        terms::tuple_arity(created).expect("a `{ok, Handle}` tuple"),
        2
    );
    let tag = terms::tuple_element(created, 0).expect("the tag");
    let handle = terms::tuple_element(created, 1).expect("the handle");
    assert_eq!(
        runtime.atom_name_of(tag),
        Some("ok"),
        "`tty_create/1`'s success tag"
    );
    assert!(
        !is_small(handle),
        "the handle must be opaque — an opaque resource, never a small integer"
    );
    assert_ne!(
        runtime.atom_name_of(handle),
        Some("undefined"),
        "the handle must never be a fabricated `undefined`"
    );

    // The resource lookup round-trips: a served call accepts the SAME handle back.
    let encoding =
        call(&mut runtime, "prim_tty", "tty_encoding", &[handle]).expect("a served answer");
    assert_eq!(
        runtime.atom_name_of(encoding),
        Some("utf8"),
        "`tty_encoding/1` on the minted handle"
    );
    let size =
        call(&mut runtime, "prim_tty", "tty_window_size", &[handle]).expect("a served answer");
    assert_eq!(
        terms::tuple_arity(size).expect("an `{error, Reason}` tuple"),
        2,
        "`tty_window_size/1` on the minted handle (a non-terminal ofd ⇒ `{{error,enotsup}}`)"
    );
    let reason = terms::tuple_element(size, 1).expect("the reason");
    assert_eq!(
        runtime.atom_name_of(reason),
        Some("enotsup"),
        "the C's `enotsup` arm for a non-terminal ofd"
    );

    // A term that is not a live handle is `badarg` — the C's `enif_get_resource` miss.
    let bogus = runtime.intern("not_a_handle");
    let error = call(&mut runtime, "prim_tty", "tty_encoding", &[bogus])
        .expect_err("a term that is not a live handle must be rejected");
    assert_badarg(error);
}

/// The oracle's `prim_tty` module does not export `tty_create/1`, which is why the handle is
/// pinned structurally (above) instead of compared: the oracle's own answer is `undef`.
///
/// This runtime serves `tty_create/1` from the floor (`floor_shadows("prim_tty", "tty_create", 1)`
/// is true — it is one of `NIF_KEYS`), so the CLI really does answer a `{ok, Handle}` tuple.
#[test]
fn the_oracle_does_not_export_tty_create() {
    if !installation_present() {
        return;
    }
    const PROGRAM: &str = "io:format(\"~p~n\",[catch prim_tty:tty_create(stdout)]),halt(0).";

    let runtime = ours(PROGRAM);
    assert!(
        runtime.starts_with("{ok,"),
        "this runtime serves `tty_create/1` (a `{{ok, Handle}}` tuple), got {runtime}"
    );
    assert!(
        !runtime.contains("undefined"),
        "the handle is never a fabricated `undefined`, got {runtime}"
    );

    let Some(oracle_answer) = theirs(PROGRAM) else {
        return;
    };
    assert!(
        oracle_answer.contains("undef"),
        "the oracle's `prim_tty:tty_create/1` is a non-exported `-nifs` local stub, so its \
         answer is `undef`; got {oracle_answer}"
    );
}

// ---------------------------------------------------------------------------------------
// 3. The typed capability gaps
// ---------------------------------------------------------------------------------------

/// One NIF stub this runtime does **not** serve, and the `-eval` program that reaches it.
struct Gap {
    /// The label a failure names.
    name: &'static str,
    /// The `-eval` program.
    program: &'static str,
}

/// The unserved stubs, each a typed `unsupported` gap rather than a fabricated answer.
///
/// `wcwidth/1` / `isprint/1` / `wcswidth/1` need libc's locale/Unicode width tables, which have
/// no faithful safe-std equivalent (`libc::wcwidth` is `unsafe` and the crate is
/// `#![deny(unsafe_code)]`). The oracle's own module does not export `wcwidth/1` or `isprint/1`
/// either, so there is nothing to compare — the assertion is that the runtime refuses them
/// **loudly** instead of inventing a width. (`tty_select/2` **is** served: it arms a one-shot
/// read select over the port layer's poller and answers `ok`.)
const GAPS: &[Gap] = &[
    Gap {
        name: "wcwidth/1",
        program: "prim_tty:wcwidth(65).",
    },
    Gap {
        name: "isprint/1",
        program: "prim_tty:isprint(65).",
    },
];

/// Each unserved stub exits **3** with the single `beam_rt: unsupported: …` line on stderr and
/// nothing on stdout — the Milestone-1 capability-gap signal (`src/cli/CONTEXT.md`), never a
/// wrong answer.
#[test]
fn the_unserved_stubs_are_typed_unsupported_gaps() {
    if !installation_present() {
        return;
    }
    for gap in GAPS {
        let cli = beam_rt(&["-noshell", "-eval", gap.program]);
        assert_eq!(
            cli.code, 3,
            "`{}` must be the typed capability gap (exit 3), got exit {} (stderr: {})",
            gap.name, cli.code, cli.err
        );
        assert!(
            cli.out.is_empty(),
            "`{}` must fabricate nothing on stdout, got {:?}",
            gap.name,
            cli.out
        );
        let first = cli.err.lines().next().unwrap_or_default();
        assert!(
            first.starts_with("beam_rt: unsupported: "),
            "`{}`: {first:?}",
            gap.name
        );
        assert!(
            first.contains("prim_tty"),
            "`{}` names the gap's module: {first:?}",
            gap.name
        );
        assert!(
            !cli.err.contains("USAGE"),
            "`{}` is a capability gap, not a usage error: {}",
            gap.name,
            cli.err
        );
    }
}

/// A capability gap is **uncatchable**: `try`/`catch` cannot swallow it, so a probe cannot
/// turn the missing width into a value and a differential case cannot mistake it for a pass.
#[test]
fn a_prim_tty_gap_cannot_be_caught() {
    if !installation_present() {
        return;
    }
    let cli = beam_rt(&[
        "-noshell",
        "-eval",
        "io:format(\"~p~n\",[catch prim_tty:wcwidth(65)]),halt(0).",
    ]);
    assert_eq!(
        cli.code, 3,
        "`catch prim_tty:wcwidth/1` must still be the uncatchable gap; stderr: {}",
        cli.err
    );
    assert!(
        cli.out.is_empty(),
        "the `io:format` after the gap must never run, got {:?}",
        cli.out
    );
}

// ---------------------------------------------------------------------------------------
// 4. The documented no-HAVE_TERMCAP deviation
// ---------------------------------------------------------------------------------------

/// The runtime's own honest no-termcap answer (the C's `make_enotsup` arm).
const NO_TERMCAP: &str = "{error,enotsup}";

/// One termcap-family function: the program run under this runtime, and the (possibly
/// different) program run under the oracle.
struct TermcapDeviation {
    /// The label a failure names.
    name: &'static str,
    /// The `~p` probe run under **this** runtime's CLI.
    ours: &'static str,
    /// The probe run under the **oracle**. Usually the same `~p` program; `tinfo/0` is the
    /// exception, because the oracle's answer is a ~30 KB map that would drown the log — an
    /// `is_map/1` shape test makes the same point.
    theirs: &'static str,
}

/// The `tinfo/0` / `tigetnum/1` / `tigetflag/1` / `tigetstr/1` / `tputs/2` family.
///
/// These are the **exported** `lib/kernel/src/prim_tty.erl` wrappers (`tinfo/0`, `tigetnum/1`,
/// … all of which delegate to the `*_nif` stubs the floor serves). The runtime answers
/// `{error,enotsup}` for every one of them, because it models a no-`HAVE_TERMCAP` ERTS build —
/// see the module docs above. The oracle on this host has termcap, so it answers a real value.
const TERMCAP_DEVIATIONS: &[TermcapDeviation] = &[
    TermcapDeviation {
        name: "tinfo/0",
        ours: "io:format(\"~p~n\",[catch prim_tty:tinfo()]),halt(0).",
        theirs: "io:format(\"~p~n\",[is_map(prim_tty:tinfo())]),halt(0).",
    },
    TermcapDeviation {
        name: "tigetnum/1",
        ours: "io:format(\"~p~n\",[catch prim_tty:tigetnum(\"co\")]),halt(0).",
        theirs: "io:format(\"~p~n\",[catch prim_tty:tigetnum(\"co\")]),halt(0).",
    },
    TermcapDeviation {
        name: "tigetflag/1",
        ours: "io:format(\"~p~n\",[catch prim_tty:tigetflag(\"am\")]),halt(0).",
        theirs: "io:format(\"~p~n\",[catch prim_tty:tigetflag(\"am\")]),halt(0).",
    },
    TermcapDeviation {
        name: "tigetstr/1",
        ours: "io:format(\"~p~n\",[catch prim_tty:tigetstr(\"clear\")]),halt(0).",
        theirs: "io:format(\"~p~n\",[catch prim_tty:tigetstr(\"clear\")]),halt(0).",
    },
    TermcapDeviation {
        name: "tputs/2",
        ours: "io:format(\"~p~n\",[catch prim_tty:tputs(\"clear\", [])]),halt(0).",
        theirs: "io:format(\"~p~n\",[catch prim_tty:tputs(\"clear\", [])]),halt(0).",
    },
];

/// This runtime's termcap family answers the honest `{error,enotsup}` — and the oracle's does
/// **not**, which is exactly the recorded deviation. Parity is deliberately **not** claimed
/// here: a terminfo implementation is out of scope, and the boot path only needs
/// `setupterm() =:= ok`.
#[test]
fn the_termcap_family_is_the_documented_no_termcap_deviation() {
    if !installation_present() {
        return;
    }
    for deviation in TERMCAP_DEVIATIONS {
        let runtime = ours(deviation.ours);
        assert_eq!(
            runtime, NO_TERMCAP,
            "{}: this runtime models a no-`HAVE_TERMCAP` build",
            deviation.name
        );
        if let Some(oracle_answer) = theirs(deviation.theirs) {
            assert_ne!(
                oracle_answer, NO_TERMCAP,
                "{}: this oracle IS built with termcap, so it must not answer the runtime's \
                 no-termcap value — if it does, the documented deviation has been mis-recorded",
                deviation.name
            );
        }
    }
}
