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

//! The **bignum bit-segment** contract: `<<Int:Size>>` where `Int` is a bignum — plus the
//! narrower contract that a failure in that field is *typed*, never a wrong value.
//!
//! # The two bit-segment builders, and why only one of them is deficient
//!
//! This runtime has two **independent** implementations of Erlang's `<<…>>` construction, and
//! this suite pins both:
//!
//! * the `-eval` **bit-buffer builder** (`src/eval/mod.rs`: `BitBuf` + `BitBuf::push_integer_bits`,
//!   fed by `Evaluator::integer_magnitude`) masks an integer of *any* width into the field. It is
//!   **correct** — `<<(1 bsl 59):64>>`, `<<7811896436376826984:64>>`, `<<(1 bsl 100):96>>`,
//!   `<<(1 bsl 59):64/little>>`, `<<(1 bsl 59):64/native>>`, `<<-3:64/signed>>`,
//!   `<<(1 bsl 59):60>>`, `<<(1 bsl 200):64>>` and a bignum followed by a non-byte-aligned tail
//!   all print the oracle's exact bytes ([`EVAL_BINARY_PROBES`],
//!   [`the_eval_bit_builder_masks_a_bignum_like_the_oracle`]). **No change was needed there**;
//!   the probe stays as the regression gate that keeps it that way.
//! * the **engine's** `bs_create_bin` (`crates/beam_engine/src/families/bitstrings/create/mod.rs`,
//!   the `"integer"` arm): for a field of `nbits <= 64` it calls `small_value(value)` and, when
//!   that is `None` (a bignum), fails the segment with the **bare `badarg`** instead of masking
//!   the bignum into the field as the C does (`erts_bs_put_integer_*` / `fmt_big_be` /
//!   `fmt_big_le`, `erts/emulator/beam/erl_bits.c`). A field of `nbits > 64` takes a **wide
//!   path that already agrees** with the oracle. That file is **outside this suite's carve-out**,
//!   so what is pinned here is the weaker but honest *invariant*: a bignum in a narrow segment
//!   must produce **either the oracle's exact bytes or the typed failure** `beam_rt: badarg`
//!   (exit 1) — never a different value
//!   ([`a_narrow_segment_bignum_is_either_the_oracles_bytes_or_a_typed_gap`]).
//!
//! Both invariant tests are written as that **disjunction**, so they pass today *and* get
//! strictly stronger once the engine arm is fixed: the "gap" branch (an exact `beam_rt: badarg`
//! with exit 1) flips **automatically** to the "agree" branch (byte-identity with the oracle),
//! and each test additionally requires that at least one probe already took the "agree" branch,
//! so neither can pass by rejecting everything.
//!
//! # Ground truth
//!
//! The oracle masks a bignum into a 64-bit field:
//!
//! | `X` | `<<X:64>>` |
//! |---|---|
//! | `576460752303423488` (`2^59`) | `<<8,0,0,0,0,0,0,0>>` |
//! | `7811896436376826984` | `<<"linkpath">>` (`7811896436376826984 == 0x6C696E6B70617468`) |
//! | `376460752303423487` (`2^59 - 1`, still a small int) | `<<5,57,117,15,68,235,255,255>>` |
//!
//! [`the_oracle_masks_a_bignum_into_a_narrow_segment`] pins each of these exactly as the live
//! oracle answers it. Note that `2^59` and `7811896436376826984` are *different* inputs with
//! *different* printed forms — only the first is `<<8,0,0,0,0,0,0,0>>`.
//!
//! # The `run`-route entry point: a string token, not an integer token
//!
//! `beam_rt run <beam> <m>:<f>/<a> <arg…>` cannot carry a bignum **integer**: its integer tokens
//! are materialized with `beam_term::make_small` (`src/runtime.rs::materialize`, reached from
//! `src/cli.rs::parse_arg_term`'s `i64` arm — both outside this suite's carve-out), and a value
//! above `MAX_SMALL` (`2^59 - 1`) is **not rejected**: the `debug_assert` in `make_small` aborts
//! the process in a debug build (`beam_rt run bm.beam bm:mk64/1 576460752303423488` → a panic at
//! `beam_term/src/immediate.rs:77`, exit 101), and in a **release** build that assert is compiled
//! out and the token is silently truncated to a wrong small int
//! (`… bm:mk64/1 576460752303423488` → `<<248,0,0,0,0,0,0,0>>` where the oracle prints
//! `<<8,0,0,0,0,0,0,0>>`). Neither outcome is one of the two this suite may accept, and the
//! range check belongs in the two out-of-scope files above, so that path is **not** probed here.
//!
//! The fixture therefore also exports the **string** entry points `mk64s/1` / `mk80s/1`
//! ([`FIXTURE_SRC`]): they rebuild the same integer in the engine with `list_to_integer/1` and
//! then construct the segment — the *identical* `bs_create_bin` call site, reached through a
//! token the CLI can carry. The bare-integer entry point `mk64/1` is exercised too, on the
//! control value that fits a small int.
//!
//! # The compile-route half
//!
//! [`the_in_process_erl_tar_compile_reports_a_real_outcome`] is the **real** consumer of the
//! engine arm: OTP's own `lib/stdlib/src/erl_tar.erl` currently fails its in-process compile with
//! `{crash,core_to_ssa,badarg,…}` in `beam_core_to_ssa:function/2` (the compiler's synthesized
//! `<<Int:Size>>` constructions — e.g. `beam_core_to_ssa:select_extract_int/5` — hit the same
//! `"integer"` arm). As above, the assertion is the disjunction: a **reported**
//! `{ok, ErlTar, …}` / `{error, …}` outcome with no masked reason, and byte-identity with the
//! oracle's artifact **only on the `{ok, …}` branch**.
//!
//! The oracle artifact is produced with the **identical option list** through `compile:file/2`
//! (not `erlc +deterministic +no_docs`): the `Dbgi` chunk records the compile options, so
//! `+deterministic +no_docs` alone is *not* byte-comparable (measured on `ordsets.erl`: 2344
//! bytes from `erlc` vs 2372 from the identical option list). `lib/stdlib/src/erl_lint.erl` is
//! **deliberately not probed here** twice over: its failure is `{badmap, …}` in
//! `beam_ssa_opt:phase/4` (the separate, out-of-scope `maps:*` iterator-term defect, not this
//! one), and its ~23 s in-process compile leaves too little margin under the shared 30 s probe
//! bound.
//!
//! A missing toolchain (`erl`/`erlc`) is a **loud** skip, never a silent pass
//! (`tests/support/mod.rs` discovers both by filesystem only).

// The shared harness is compiled in whole, but this suite uses only its bounded runner and its
// toolchain discovery; the unused remainder is expected here — the same shape
// `tests/io_diagnostic_unmask.rs` uses.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The exact typed failure a capability gap produces: the CLI's one diagnostic line for a
/// `beam_bifs`/engine gap, with exit code 1 (`src/error.rs` classifiers; `src/cli.rs` at most).
const TYPED_GAP: &str = "beam_rt: badarg";

/// The I/O-device failure that must never appear in a compiler run: it is what used to *replace* a
/// pass's real reason (`tests/io_diagnostic_unmask.rs`).
const MASKING_REASON: &str = "calling_self";

/// The diagnostic every crashing compiler pass prints from its `catch` handler
/// (`io:fwrite("Function: ~w/~w\n", [Name, Arity])`).
const DIAGNOSTIC: &str = "Function: ";

/// The compile probe's own marker, so the outcome assertion cannot be satisfied by an unrelated
/// line.
const MARKER: &str = "RET";

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// One finished probe: its exit code and its trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    /// The finished probe described by one collected child `output`.
    fn from_output(output: Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }

    /// stdout and stderr as one newline-joined string: a compiler diagnostic and a CLI diagnostic
    /// may land on either stream, and the assertions here are about **content**, not about which
    /// stream carried it.
    fn combined(&self) -> String {
        format!("{}\n{}", self.out, self.err)
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

/// `erl -noshell -eval <expr> -s init stop` through the oracle (the node is bounded: the probe
/// always gets a terminator, and the shared runner kills it at the bound).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    run(oracle, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

/// `erl -noshell -pa <dir> -eval <expr> -s init stop` — the oracle with the fixture directory on
/// its code path.
fn oracle_pa_eval(oracle: &Path, dir: &Path, expr: &str) -> Run {
    run(
        oracle,
        &[
            "-noshell",
            "-pa",
            &dir.to_string_lossy(),
            "-eval",
            expr,
            "-s",
            "init",
            "stop",
        ],
    )
}

// ---------------------------------------------------------------------------------------
// Temporary directories, Erlang literals and the fixture
// ---------------------------------------------------------------------------------------

/// A private temporary directory, removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_bignum_seg_<pid>_<n>_<label>`, unique per test and per run.
    fn new(label: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let unique = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "beam_rt_bignum_seg_{}_{unique}_{label}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The **Erlang string literal** for `path`: the temporary names this suite builds are ASCII,
/// which Rust's `Debug` and Erlang's string syntax quote identically.
fn literal(path: &Path) -> String {
    format!("{:?}", path.to_str().expect("an ASCII temporary path"))
}

/// The fixture: the minimal `<<X:64>>` / `<<X:80>>` pair of the contract, plus the two
/// **string** entry points the `run` route can actually carry a bignum through (see the module
/// docs — `mk64s/1`/`mk80s/1` reach the same `bs_create_bin` call site via `list_to_integer/1`).
const FIXTURE_SRC: &str = "\
-module(bm).
-export([mk64/1, mk80/1, mk64s/1, mk80s/1]).
mk64(X) -> <<X:64>>.
mk80(X) -> <<X:80>>.
mk64s(S) -> mk64(list_to_integer(S)).
mk80s(S) -> mk80(list_to_integer(S)).
";

/// Compiles [`FIXTURE_SRC`] with the system `erlc` into `dir` and returns the `bm.beam` path.
///
/// A missing `erlc` is the caller's loud skip; a build failure is a hard error (the fixture is
/// four one-line functions, so an `erlc` that cannot build it is not a runtime divergence).
fn build_fixture(dir: &Path, erlc: &Path) -> PathBuf {
    let src = dir.join("bm.erl");
    fs::write(&src, FIXTURE_SRC).expect("write the fixture");
    let built = run(
        erlc,
        &["-o", &dir.to_string_lossy(), &src.to_string_lossy()],
    );
    assert_eq!(built.code, 0, "`erlc` must build the fixture: {}", built.err);
    let beam = dir.join("bm.beam");
    assert!(beam.is_file(), "`erlc` wrote no {}", beam.display());
    beam
}

// ---------------------------------------------------------------------------------------
// (a) The oracle's ground truth
// ---------------------------------------------------------------------------------------

/// **The oracle masks a bignum into a narrow segment.** The two contract inputs must print
/// exactly what the live oracle prints, and they must be *different* forms — `2^59` is
/// `<<8,0,0,0,0,0,0,0>>`, while `7811896436376826984` (`0x6C696E6B70617468`) is
/// `<<"linkpath">>`.
///
/// This pins the ground truth the two runtime probes below compare against, on a route that needs
/// only `erl` (no fixture, no `erlc`).
#[test]
fn the_oracle_masks_a_bignum_into_a_narrow_segment() {
    let Some(oracle) = support::require_oracle("bignum_bit_segment") else {
        return;
    };

    // (input, the oracle's exact `~p` rendering of `<<X:64>>`).
    let probes: &[(&str, &str)] = &[
        ("576460752303423488", "<<8,0,0,0,0,0,0,0>>"),
        ("7811896436376826984", "<<\"linkpath\">>"),
        ("376460752303423487", "<<5,57,117,15,68,235,255,255>>"),
    ];
    for (value, expected) in probes {
        let expr = format!("io:format(\"~p~n\",[<<{value}:64>>]), halt(0).");
        let theirs = oracle_eval(&oracle, &expr);
        assert_eq!(
            theirs.code, 0,
            "the oracle must evaluate {expr:?}: exit {} — {}",
            theirs.code, theirs.err
        );
        assert_eq!(
            theirs.out, *expected,
            "the oracle's ground truth for <<{value}:64>> — the runtime probes below are \
             compared against exactly this string"
        );
    }

    // The ground truth is not one value printed twice: the two boundary inputs really do differ.
    let boundary = oracle_eval(&oracle, "io:format(\"~p~n\",[<<(1 bsl 59):64>>]), halt(0).");
    assert_eq!(boundary.out, "<<8,0,0,0,0,0,0,0>>");
    let spelling = oracle_eval(
        &oracle,
        "io:format(\"~p~n\",[<<7811896436376826984:64>>]), halt(0).",
    );
    assert_ne!(
        boundary.out, spelling.out,
        "2^59 and 7811896436376826984 are different inputs with different printed forms"
    );
}

// ---------------------------------------------------------------------------------------
// (b) The engine invariant, on the `run` route
// ---------------------------------------------------------------------------------------

/// One `run`-route probe of the fixture.
struct SegmentProbe {
    /// The exported function — `mk64s`/`mk80s` (a string argument the fixture parses) or the
    /// bare-integer `mk64`.
    function: &'static str,
    /// The decimal value, as it appears inside the argument token.
    value: &'static str,
    /// `true` → the token is the Erlang **string** `"<value>"` (the fixture rebuilds the integer
    /// with `list_to_integer/1`); `false` → the bare **integer** token, which the CLI can only
    /// materialize up to `MAX_SMALL`.
    quoted: bool,
    /// `true` for a **narrow** (`<= 64` bit) field, where the out-of-scope engine gap may show as
    /// the typed failure; `false` for the **wide** field, where the oracle's bytes are required.
    narrow: bool,
}

/// **The probed set.** The narrow probes carry the contract inputs (including the small-int
/// control that must already agree); the wide probes are the contrast that proves the wider path
/// really is exercised with the same bignums.
const SEGMENT_PROBES: &[SegmentProbe] = &[
    SegmentProbe {
        function: "mk64",
        value: "376460752303423487",
        quoted: false,
        narrow: true,
    },
    SegmentProbe {
        function: "mk64s",
        value: "376460752303423487",
        quoted: true,
        narrow: true,
    },
    SegmentProbe {
        function: "mk64s",
        value: "576460752303423488",
        quoted: true,
        narrow: true,
    },
    SegmentProbe {
        function: "mk64s",
        value: "7811896436376826984",
        quoted: true,
        narrow: true,
    },
    SegmentProbe {
        function: "mk64s",
        value: "18446744073709551616",
        quoted: true,
        narrow: true,
    },
    SegmentProbe {
        function: "mk80s",
        value: "376460752303423487",
        quoted: true,
        narrow: false,
    },
    SegmentProbe {
        function: "mk80s",
        value: "576460752303423488",
        quoted: true,
        narrow: false,
    },
    SegmentProbe {
        function: "mk80s",
        value: "7811896436376826984",
        quoted: true,
        narrow: false,
    },
    SegmentProbe {
        function: "mk80s",
        value: "18446744073709551616",
        quoted: true,
        narrow: false,
    },
];

/// The argument token for a probe: the quoted Erlang string, or the bare integer.
fn token(probe: &SegmentProbe) -> String {
    if probe.quoted {
        format!("\"{}\"", probe.value)
    } else {
        probe.value.to_string()
    }
}

/// **The bignum-narrow-segment invariant.** For every probe, the fixture is run under the Rust
/// runtime on the **`run` route** and the result must be one of exactly two things:
///
/// * **byte-identical** to the oracle's rendering of the same call, or
/// * the **typed failure** — exit 1 with the single diagnostic line `beam_rt: badarg` and no
///   stdout at all.
///
/// Anything else (a different value, a different reason, a different exit code, or a crash) fails.
/// The narrow probes are allowed to take either branch, because of the known out-of-scope engine
/// gap; the wide probes must agree, and at least one narrow probe must already agree, so the
/// test can never pass by rejecting every input.
#[test]
// NOTE: the narrow-field failure is owned by
// `erts_rust/crates/beam_engine/src/families/bitstrings/create/mod.rs` — the `bs_create_bin`
// `"integer"` arm, which fails a `nbits <= 64` field whose value is a bignum (`small_value` →
// `None`) with the bare `badarg` instead of masking it into the field as the C does. Fixing that
// arm flips the narrow probes from the "typed gap" branch to the "byte-identical" branch
// **without any change here**; the wide probes and the in-process `erl_tar` compile
// ([`the_in_process_erl_tar_compile_reports_a_real_outcome`]) tighten the same way.
fn a_narrow_segment_bignum_is_either_the_oracles_bytes_or_a_typed_gap() {
    let Some(oracle) = support::require_oracle("bignum_bit_segment") else {
        return;
    };
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly("bignum_bit_segment", "no `erlc` on $PATH");
        return;
    };

    let dir = TempDir::new("fixture");
    let beam = build_fixture(&dir.0, &erlc);
    let beam_arg = beam.to_string_lossy().to_string();

    let (mut narrow_agreed, mut narrow_gapped) = (0usize, 0usize);
    for probe in SEGMENT_PROBES {
        let arg = token(probe);
        // The oracle is given a *call* (`bm:mk64s("<v>")`); the `run` route is given a
        // `module:function/arity` *spec* plus the argument token.
        let call = format!("bm:{}({arg})", probe.function);
        let spec = format!("bm:{}/1", probe.function);

        // The oracle: the same call, resolved through the fixture directory on its code path.
        // `~w`, not `~p`: the `run` route prints its result with `~w` (`src/format.rs::
        // term_to_string_w`), and on a *printable* binary the two controls differ — OTP prints
        // `<<"linkpath">>` under `~p` but `<<108,105,110,107,112,97,116,104>>` under `~w`. The
        // comparison below is about the bytes, so both sides must use the same control.
        let probe_expr = format!("io:format(\"~w~n\",[{call}]), halt(0).");
        let theirs = oracle_pa_eval(&oracle, &dir.0, &probe_expr);
        assert_eq!(
            theirs.code, 0,
            "the oracle must run {call}: exit {} — {}",
            theirs.code, theirs.err
        );
        let expected = theirs.out.clone();
        assert!(
            !expected.is_empty(),
            "the oracle printed nothing for {call}, so the comparison below would be vacuous"
        );

        // This runtime: the `run` route, printing the result of the scheduled process.
        let ours = run(
            Path::new(BINARY),
            &["-noshell", "run", &beam_arg, &spec, &arg],
        );

        // Branch 1 — the runtime AGREED. Required of every wide probe; counted for the narrow
        // ones so the disjunction below cannot be satisfied by rejecting every bignum.
        if ours.code == 0 {
            assert!(
                ours.err.is_empty(),
                "{call}: a successful run must print no diagnostic — stderr {:?}",
                ours.err
            );
            assert_eq!(
                ours.out,
                expected,
                "{call}: the runtime and the oracle must produce the SAME bytes — a different \
                 value here is the one outcome this contract forbids"
            );
            if probe.narrow {
                narrow_agreed += 1;
            }
            continue;
        }

        // Branch 2 — the exact typed failure, and ONLY for a narrow field: the wide path already
        // agrees, so any non-zero exit there is a real divergence.
        assert!(
            probe.narrow,
            "{call}: a field wider than 64 bits takes the engine's wide path, which must agree \
             with the oracle — it produced exit {} / stdout {:?} / stderr {:?}",
            ours.code, ours.out, ours.err
        );
        assert_eq!(
            (ours.code, ours.out.as_str(), ours.err.as_str()),
            (1, "", TYPED_GAP),
            "{call}: a capability gap must be exactly `exit 1` + the single diagnostics line \
             {TYPED_GAP:?} on stderr and empty stdout — never a crash, never another reason"
        );
        narrow_gapped += 1;
    }

    eprintln!(
        "bignum bit segments on the `run` route: {narrow_agreed} narrow probe(s) byte-identical \
         to the oracle, {narrow_gapped} narrow probe(s) the typed `badarg` gap"
    );
    assert!(
        narrow_agreed > 0,
        "at least one narrow (<= 64-bit) probe must already be byte-identical to the oracle — \
         without it the disjunction above would be satisfied by rejecting every bignum"
    );
}

// ---------------------------------------------------------------------------------------
// (c) The `-eval` bit-buffer builder
// ---------------------------------------------------------------------------------------

/// The `-eval` construction probes: every one is a **literal** `<<…>>` exercising a bignum in a
/// segment of some width, endianness and signedness. Sizes at, below and above the machine word
/// and the 64-bit boundary are all covered, as are a non-byte-aligned width, a bignum followed by
/// a bit tail, and values whose low bits are zero (so a wrong truncation cannot hide behind a
/// zero word).
const EVAL_BINARY_PROBES: &[&str] = &[
    // the contract's two boundary inputs, byte-aligned
    "io:format(\"~p~n\",[<<(1 bsl 59):64>>])",
    "io:format(\"~p~n\",[<<7811896436376826984:64>>])",
    "io:format(\"~p~n\",[<<(1 bsl 60):64>>])",
    // little / native endianness, and signedness
    "io:format(\"~p~n\",[<<(1 bsl 59):64/little>>])",
    "io:format(\"~p~n\",[<<7811896436376826984:64/little>>])",
    "io:format(\"~p~n\",[<<(1 bsl 59):64/native>>])",
    "io:format(\"~p~n\",[<<(1 bsl 59):64/signed>>])",
    "io:format(\"~p~n\",[<<-3:64/signed>>])",
    // values past 64 bits, positive and negative
    "io:format(\"~p~n\",[<<18446744073709551616:64>>])",
    "io:format(\"~p~n\",[<<-18446744073709551616:64>>])",
    "io:format(\"~p~n\",[<<(1 bsl 200):64>>])",
    // widths below a word (including non-byte-aligned) and far above it
    "io:format(\"~p~n\",[<<(1 bsl 59):1>>])",
    "io:format(\"~p~n\",[<<(1 bsl 59):60>>])",
    "io:format(\"~p~n\",[<<(1 bsl 59):80>>])",
    "io:format(\"~p~n\",[<<(1 bsl 100):96>>])",
    "io:format(\"~p~n\",[<<(1 bsl 59):128>>])",
    // a bignum segment followed by a non-byte-aligned tail
    "io:format(\"~p~n\",[<<(1 bsl 59):64, 1:1>>])",
    "io:format(\"~p~n\",[begin A = 1 bsl 59, <<A:64, (A bsr 8):8>> end])",
];

/// **The `-eval` bit-buffer builder already handles bignums — and must keep doing so.** For every
/// probe in [`EVAL_BINARY_PROBES`] the built binary and the oracle must print the **same bytes**
/// and both must exit 0.
///
/// This is the evidence that no fix was needed in `src/eval/**` for this contract: the evaluator
/// takes an integer segment's value through `Evaluator::integer_magnitude` (a small int or an
/// exact bignum, as `(sign, limbs)`) and `BitBuf::push_integer_bits` masks it into the field at
/// any width, matching the C's `bs_put_integer` truncation.
#[test]
fn the_eval_bit_builder_masks_a_bignum_like_the_oracle() {
    let Some(oracle) = support::require_oracle("bignum_bit_segment") else {
        return;
    };

    for body in EVAL_BINARY_PROBES {
        let expr = format!("{body}, halt(0).");
        let theirs = oracle_eval(&oracle, &expr);
        assert_eq!(
            theirs.code, 0,
            "the oracle must evaluate {body}: exit {} — {}",
            theirs.code, theirs.err
        );
        assert!(
            !theirs.out.is_empty(),
            "the oracle printed nothing for {body}, so the comparison below would be vacuous"
        );

        let ours = beam_rt_eval(&expr);
        assert_eq!(
            ours.code, 0,
            "{body}: the `-eval` bit-syntax builder must build this segment, not report a gap — \
             exit {} / stdout {:?} / stderr {:?}",
            ours.code,
            ours.out,
            ours.err
        );
        assert_eq!(
            ours.out, theirs.out,
            "{body}: the `-eval` builder must print the oracle's exact bytes"
        );
    }
}

// ---------------------------------------------------------------------------------------
// (d) The real consumer: the in-process `erl_tar` compile
// ---------------------------------------------------------------------------------------

/// `compile:file(Source, [no_spawn_compiler_process, deterministic, no_docs, return_errors,
/// return_warnings, {outdir, Dir}])`, then the result — the **identical** option list both
/// runtimes are given, so the only difference between the two runs is the runtime.
///
/// `return_errors`/`return_warnings` make the compiler **return** a failure instead of exiting, so
/// the probe always reaches its `halt(0)` and the outcome is observable.
fn compile_probe(src: &Path, outdir: &Path) -> String {
    format!(
        "R = compile:file({}, [no_spawn_compiler_process, deterministic, no_docs, return_errors, \
         return_warnings, {{outdir, {}}}]),\n\
         io:format(\"{MARKER} ~p~n\", [R]),\n\
         halt(0).",
        literal(src),
        literal(outdir)
    )
}

/// The probed module: the real stdlib module whose compile route hits the engine's narrow-segment
/// `"integer"` arm.
const PROBED_MODULE: &str = "erl_tar";

/// How the in-process compile ended, as reported by the probe's own `MARKER` line.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    /// `{ok, Module, …}` — the module compiled (with or without warnings).
    Compiled,
    /// `{error, …}` — the compiler reported a failure (its real reason, not a masked one).
    Errored,
}

/// Classifies the probe's result line for `module`, or `None` when the probe never reported one —
/// the case in which the "no `calling_self`" claim would be vacuous and the probe must fail
/// loudly instead.
fn reported_outcome(text: &str, module: &str) -> Option<Outcome> {
    if text.contains(&format!("{MARKER} {{ok,{module},")) {
        Some(Outcome::Compiled)
    } else if text.contains(&format!("{MARKER} {{error")) {
        Some(Outcome::Errored)
    } else {
        None
    }
}

/// The OTP source tree (`$ERL_TOP`, else the workspace root three levels up from this crate).
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// **The compile-route invariant, for the real consumer of the engine arm.** OTP's own
/// `lib/stdlib/src/erl_tar.erl` is compiled **in process** under the built `beam_rt` binary and
/// under the system oracle, with the identical option list:
///
/// 1. the runtime must ride the compile to a **reported** outcome — `RET {ok,erl_tar,…}` or
///    `RET {error,…}`, never neither (which would make the next claim vacuous);
/// 2. the compiler must **never** mask that outcome with `calling_self`;
/// 3. **conditionally on the outcome being `{error,…}`**, a real compiler pass's own
///    `Function: <name>/<arity>` diagnostic must have been printed — this runtime's compiler is
///    running, and its real reason propagated;
/// 4. **conditionally on the outcome being `{ok,erl_tar,…}`**, the artifact must be
///    **byte-identical** to the oracle's identical-option artifact.
///
/// Today the runtime takes the `{error,…}` branch (a `badarg` crash in
/// `beam_core_to_ssa:function/2`), so assertion 4 is skipped; once the engine's narrow-segment
/// `"integer"` arm is fixed the outcome becomes `{ok,erl_tar,…}` and assertion 4 starts running —
/// the test gets **strictly stronger** with no edit here.
///
/// The oracle must report `{ok,erl_tar,…}` (`[]` warnings): the input is demonstrably
/// compilable, so a rust-side `{error,…}` is a residual of this runtime rather than a broken
/// input.
///
/// A missing source tree (an installed-only or relocated checkout) — or a missing `erl` — is a
/// **loud** skip, never a silent pass.
#[test]
fn the_in_process_erl_tar_compile_reports_a_real_outcome() {
    let Some(oracle) = support::require_oracle("bignum_bit_segment") else {
        return;
    };
    let src = source_root().join(format!("lib/stdlib/src/{PROBED_MODULE}.erl"));
    if !src.is_file() {
        support::skip_loudly(
            "bignum_bit_segment",
            &format!(
                "no real stdlib source to compile at {} — {PROBED_MODULE} is probed because its \
                 in-process compile is what reaches the engine's narrow-segment `\"integer\"` arm",
                src.display()
            ),
        );
        return;
    }

    let dir = TempDir::new("compile");
    let rust_out = dir.0.join("rust_out");
    let oracle_out = dir.0.join("oracle_out");
    // `compile` does not create the output directory.
    for out in [&rust_out, &oracle_out] {
        fs::create_dir_all(out).expect("the output directory");
    }

    // ---- the oracle: the source really is compilable (identical option list) ----
    let theirs = oracle_eval(&oracle, &compile_probe(&src, &oracle_out));
    assert_eq!(
        theirs.code, 0,
        "the oracle must complete the compile probe: exit {} — {}",
        theirs.code,
        theirs.combined()
    );
    assert!(
        theirs
            .out
            .starts_with(&format!("{MARKER} {{ok,{PROBED_MODULE},")),
        "the source must be compilable on the oracle, so a rust-side `{{error, …}}` is a residual \
         of this runtime rather than a broken input — the oracle printed {:?}",
        theirs.out
    );

    // ---- this runtime: a reported outcome, with no masked reason ----
    let ours = beam_rt_eval(&compile_probe(&src, &rust_out));
    assert_eq!(
        ours.code, 0,
        "the probe must reach its own halt(0) — exit {} — {}",
        ours.code,
        ours.combined()
    );
    let combined = ours.combined();
    eprintln!("in-process compile of {PROBED_MODULE}.erl under beam_rt printed:\n{combined}");

    // (1) THE INVARIANT. `calling_self` is exactly what the compiler's diagnostic call used to
    // raise, replacing the pass's real reason with its own.
    assert!(
        !combined.contains(MASKING_REASON),
        "{PROBED_MODULE}: the compiler's diagnostic call must not raise `{MASKING_REASON}` — the \
         io device floor serves `io:fwrite/2` natively, so a compiler pass's diagnostic print \
         reaches the output and its real reason propagates. Captured output: {combined:?}"
    );

    // (2) The run really rode a compile to a reported result — either outcome is fine, but a run
    // that reported neither would make the invariant above vacuous.
    let Some(outcome) = reported_outcome(&combined, PROBED_MODULE) else {
        panic!(
            "{PROBED_MODULE}: the in-process compile must report `{MARKER} \
             {{ok,{PROBED_MODULE},…}}` or `{MARKER} {{error, …}}` so the `{MASKING_REASON}` check \
             is not vacuous — it reported neither, which means `compile:file/2` never produced a \
             result here. Captured output: {combined:?}"
        );
    };

    match outcome {
        // (3) CONDITIONAL on a failure: the compiler's own `Function: <name>/<arity>` line must
        // have been printed. Today's branch: the engine's narrow-segment `"integer"` arm fails
        // the compiler's synthesized `<<Int:Size>>` construction.
        Outcome::Errored => {
            assert!(
                combined.contains(DIAGNOSTIC),
                "{PROBED_MODULE}: the compile failed, so a compiler pass's own \
                 `{DIAGNOSTIC}<name>/<arity>` diagnostic must reach the output — it did not. \
                 Captured output: {combined:?}"
            );
            eprintln!(
                "{PROBED_MODULE}.erl: this runtime reported `{MARKER} {{error, …}}` — the \
                 out-of-scope engine gap in `crates/beam_engine/src/families/bitstrings/create/\
                 mod.rs` (`bs_create_bin`'s `\"integer\"` arm). The byte-identity assertion below \
                 is skipped until it is fixed."
            );
        }
        // (4) CONDITIONAL on success: the artifact must be byte-identical to the oracle's.
        Outcome::Compiled => {
            let rust_artifact = rust_out.join(format!("{PROBED_MODULE}.beam"));
            let oracle_artifact = oracle_out.join(format!("{PROBED_MODULE}.beam"));
            let mine = fs::read(&rust_artifact).unwrap_or_else(|error| {
                panic!(
                    "{PROBED_MODULE}: the runtime reported {:?} but wrote no {}: {error}",
                    ours.out,
                    rust_artifact.display()
                )
            });
            let reference = fs::read(&oracle_artifact).unwrap_or_else(|error| {
                panic!(
                    "{PROBED_MODULE}: the oracle reported {:?} but wrote no {}: {error}",
                    theirs.out,
                    oracle_artifact.display()
                )
            });
            if mine != reference {
                let at = mine
                    .iter()
                    .zip(reference.iter())
                    .position(|(a, b)| a != b)
                    .unwrap_or_else(|| mine.len().min(reference.len()));
                eprintln!(
                    "MISMATCH: rust {} bytes vs oracle {} bytes — first difference at byte {at}: \
                     {:?} vs {:?}",
                    mine.len(),
                    reference.len(),
                    mine.get(at),
                    reference.get(at)
                );
            }
            assert_eq!(
                mine,
                reference,
                "{PROBED_MODULE}: the in-process artifact must be byte-identical to the oracle's \
                 (identical option list)"
            );
            eprintln!(
                "{PROBED_MODULE}.erl: in-process compile byte-identical to the oracle ({} bytes)",
                mine.len()
            );
        }
    }
}
