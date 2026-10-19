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

//! The **per-level multi-`sync` byte-identity suite** for the `zlib` stream API, driven through
//! the real preloaded `zlib.beam` and pinned against the live system `erl` oracle.
//!
//! `zlib`'s 19 non-exported NIF stubs are served by this runtime, so the whole *wrapper* runs as
//! compiled (preloaded) Erlang code: `deflate/3` enqueues the iodata and drains
//! `fun deflate_nif/4` out of a `#zlib_opts{}` record, `deflateParams/3` is a `sync` flush plus
//! `save_progress/3`, and `deflateInit/1,2` forward to `deflateInit/6`. Every byte this suite
//! compares is therefore the product of the wrapper **and** the NIF behind it, which is what
//! makes a byte-count vector or an md5 a meaningful pin.
//!
//! # The transcript (`multi_sync/0`)
//!
//! The fixture payload is
//!
//! ```erlang
//! F1 = binary:part(binary:copy(<<"It was the best of times, …incredulity.\n">>, 6), 0, 1024)
//! ```
//!
//! — 1 024 bytes, md5 `A1D3CA40959CD47D539D2A0115E99AF2` — and the transcript is
//! `deflateInit(Z, Level)`, **4 ×** `deflate(Z, <<256-byte part>>, sync)`, then
//! `deflate(Z, [], finish)`, `deflateEnd(Z)`, `close(Z)`. Per level 0–9 it records the
//! **per-call byte-count vector** (the five calls), the total and the md5 of the concatenation
//! ([`LEVELS`], the golden table, which is also the oracle's own answer — [`PAYLOAD_BYTES`] and
//! [`PAYLOAD_MD5`] pin the input the same way).
//!
//! Nothing here is derived from a running runtime: the table is a **golden**. A byte-count
//! vector is the most sensitive observable available (a single changed block boundary, a
//! changed `Z_SYNC_FLUSH` marker or a changed Huffman header moves it), and the md5 catches
//! everything it does not.
//!
//! # The rest of the wrapper surface, on the same goldens
//!
//! | Surface | Fixture function | Golden | What it pins |
//! |---|---|---|---|
//! | `deflate/2` (no flush argument) and `deflate/3` with `none`/`sync`/`full`/`finish` | `flush_modes/0` | [`FLUSH_GOLDEN`] | the wrapper's `none` default and each flush's emitted byte counts |
//! | `deflateInit/1`, `/2` (the atom and integer levels `zlib.erl` accepts), `/6` (window bits positive **and** negative, memlevels 1/5/8/9, `default`/`filtered`/`huffman_only`/`rle`) | `init_forms/0` | [`FORMS_GOLDEN`] | each init form's compressed size + md5, plus a round-trip witness |
//! | `deflateReset/1` mid-stream | `reset_mid_stream/0` | [`RESET_GOLDEN`] | a reset stream reproduces a **fresh** stream's transcript exactly |
//! | `deflateParams/3` mid-stream | `deflate_params/0` | [`PARAMS_GOLDEN`] | the level/strategy switch (a `sync` flush + `save_progress/3`) keeps the stream readable and the counts oracle-exact |
//! | `deflate` → `inflate/2` | `round_trip/0` | [`ROUND_TRIP_GOLDEN`] | the payload comes back byte-exact, in one `inflate/2` call and in three chunks |
//!
//! `zlib.erl` has **no** `{level, L}` tuple form for `deflateInit/2`: `arg_level/1` accepts
//! `default`/`none`/`best_speed`/`best_compression` or an integer 0..9, and `deflateInit/2`
//! forwards to `deflateInit/6` with `deflated`, 15, 8, `default`. The forms the suite drives are
//! exactly those ([`EXPR_FORMS`]/`init_forms/0`), not a shape the wrapper would reject.
//!
//! # The two routes and the two tiers
//!
//! Every surface is checked on **four** runtime combinations — the same golden each time:
//!
//! 1. `beam_rt -noshell -eval EXPR` — the tree-walking `-eval` evaluator
//!    ([`EXPR_LEVELS`] and friends), which reaches `zlib` through the runtime's BIF/`call_ext`
//!    seams;
//! 2. `beam_rt -noshell run <fixture.beam> <module>:<function>/0` — the **scheduled** route, a
//!    compiled fixture module (`tests/fixtures/zlib_stream_levels_fixture.erl`, compiled at test
//!    time by the system `erlc` into a throw-away scratch directory);
//! 3. ×2: the **default** tier (the native baseline JIT) and `BEAM_RT_TIER=interpreter` (the
//!    direct-threaded interpreter), which are separate execution engines over the same
//!    semantics.
//!
//! Each test states which combinations it covers in its own docs; all four must be identical,
//! so a divergence on either engine is a failure.
//!
//! **The `run` route prints a result line.** The CLI's `run` action renders the fixture's return
//! value with `~w~n`, and on this route a binary renders as `<<55,56,…>>` (a pre-existing
//! printer difference unrelated to `zlib`). Every fixture function therefore **prints** its own
//! transcript with `io:format/2` and returns `ok`: the compared transcript is the stdout *minus*
//! that one trailing `ok` line, and no binary is ever printed (byte counts are integers and
//! digests are `binary:encode_hex/1` charlists).
//!
//! # Toolchain
//!
//! The oracle (`erl`) is resolved from the **filesystem** ([`support::require_oracle`], never by
//! running the tool). Every probe here boots this runtime, and boot resolves the *system code
//! path*, so the **installed OTP** is part of the fixture of the suite as well; without it the
//! suite declines with the shared loud-skip banner instead of failing. The `run` route
//! additionally needs the system `erlc`; a missing one is likewise a loud skip
//! ([`build_fixture`]). Nothing here is a silent pass and nothing is a panic for a missing
//! toolchain.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// The shared helper module is compiled into every integration-test binary, so this binary would
// otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

// ---------------------------------------------------------------------------------------
// Names, paths and the tier axis
// ---------------------------------------------------------------------------------------

/// The suite's name in the loud-skip banner.
const SUITE: &str = "zlib_stream_levels";

/// The fixture module (its source lives in `tests/fixtures/`, its `.beam` is never committed).
const MODULE: &str = "zlib_stream_levels_fixture";

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The execution tier a probe pins (`beam_rt`'s `PERF.md` tier switch).
#[derive(Clone, Copy)]
enum Tier {
    /// The default: the native baseline JIT for every process entry.
    Native,
    /// The direct-threaded interpreter (`BEAM_RT_TIER=interpreter`), the only kill switch.
    Interpreter,
}

impl Tier {
    /// How a failure names the tier.
    fn label(self) -> &'static str {
        match self {
            Tier::Native => "the default (native baseline) tier",
            Tier::Interpreter => "the interpreter tier (BEAM_RT_TIER=interpreter)",
        }
    }

    /// The `BEAM_RT_TIER` setting: `None` **removes** the variable, so the default tier is
    /// really the default even when the ambient environment sets it.
    fn env_setting(self) -> Option<&'static str> {
        match self {
            Tier::Native => None,
            Tier::Interpreter => Some("interpreter"),
        }
    }
}

/// Both tiers, in the order the probes run.
const TIERS: [Tier; 2] = [Tier::Native, Tier::Interpreter];

// ---------------------------------------------------------------------------------------
// The oracle and the compiler (filesystem discovery only)
// ---------------------------------------------------------------------------------------

/// The live `erl` oracle, resolved **once** (so a missing toolchain is a single loud skip
/// rather than one banner per comparison).
fn oracle() -> Option<PathBuf> {
    static ORACLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    ORACLE
        .get_or_init(|| support::require_oracle(SUITE))
        .clone()
}

/// The compiler beside the resolved oracle first (the same installation), then `$PATH`.
///
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists
/// (`tests/support/mod.rs`).
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}

// ---------------------------------------------------------------------------------------
// Bounded probes: this runtime, and the oracle
// ---------------------------------------------------------------------------------------

/// One finished CLI invocation: its exit code and captured output.
struct Cli {
    /// The process exit code (`-1` when killed by a signal, which no probe here does).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// The environment every probe gets: no crash dump litter, and the requested execution tier.
fn probe_env(tier: Tier) -> [(&'static str, Option<&'static str>); 2] {
    [
        ("ERL_CRASH_DUMP_SECONDS", Some("0")),
        ("BEAM_RT_TIER", tier.env_setting()),
    ]
}

/// Runs `program` with `args`, bounded ([`support::run_bounded_with_env`], a 30 s probe bound),
/// so a probe that blocks cannot hang the suite.
fn run(program: &Path, args: &[&str], tier: Tier, what: &str) -> Cli {
    let output =
        support::run_bounded_with_env(program, args, &probe_env(tier)).unwrap_or_else(|| {
            panic!(
                "{what}: `{}` did not finish within the probe bound",
                program.display()
            )
        });
    Cli {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// This runtime's `-eval` transcript for `expr`, which must succeed with a clean stderr.
fn ours_eval(expr: &str, tier: Tier) -> String {
    let what = format!("the `-eval` route on {}", tier.label());
    let cli = run(Path::new(BINARY), &["-noshell", "-eval", expr], tier, &what);
    assert_eq!(cli.code, 0, "{what} must succeed; stderr:\n{}", cli.err);
    assert!(
        cli.err.is_empty(),
        "{what} must not write to stderr:\n{}",
        cli.err
    );
    cli.out
}

/// This runtime's scheduled-route transcript for `fixture:function/0`.
///
/// The `run` action appends `~w~n` of the fixture's return value, which every fixture function
/// returns as `ok`; the compared transcript is the stdout **before** that line.
fn ours_run(fixture: &Fixture, function: &str, tier: Tier) -> String {
    let what = format!("the scheduled `run` route on {}", tier.label());
    let spec = format!("{MODULE}:{function}/0");
    let cli = run(
        Path::new(BINARY),
        &["-noshell", "run", fixture.beam_str(), &spec],
        tier,
        &what,
    );
    assert_eq!(
        cli.code, 0,
        "{what} (`{spec}`) must succeed; stderr:\n{}",
        cli.err
    );
    assert!(
        cli.err.is_empty(),
        "{what} (`{spec}`) must not write to stderr:\n{}",
        cli.err
    );
    let (transcript, result) = cli.out.rsplit_once('\n').unwrap_or_else(|| {
        panic!(
            "{what}: expected a transcript and a result line, got:\n{}",
            cli.out
        )
    });
    assert_eq!(
        result, "ok",
        "{what}: the fixture returns `ok`, so the `run` action's `~w~n` result line is `ok`"
    );
    transcript.to_string()
}

/// The oracle's transcript for the same `-eval` expression (`-pa` is not needed: the expression
/// is self-contained), with the same `-noshell` / `-s init stop` shape
/// [`support::oracle_eval`] uses — spelled out here so the probe also carries the suite's
/// environment (`ERL_CRASH_DUMP_SECONDS=0`).
fn theirs_eval(oracle: &Path, expr: &str, tier: Tier, what: &str) -> String {
    let cli = run(
        oracle,
        &["-noshell", "-eval", expr, "-s", "init", "stop"],
        tier,
        what,
    );
    assert_eq!(
        cli.code, 0,
        "{what}: the oracle exited with {}; stderr:\n{}",
        cli.code, cli.err
    );
    cli.out
}

/// The oracle running the **compiled fixture** — the same module the scheduled route runs, so
/// the oracle's answer is the one the fixture shape produces (not only the `-eval` shape).
fn theirs_fixture(oracle: &Path, fixture: &Fixture, function: &str, tier: Tier) -> String {
    let what = format!("the oracle running `{MODULE}:{function}/0`");
    let expr = format!("{MODULE}:{function}(), halt(0).");
    let cli = run(
        oracle,
        &[
            "-noshell",
            "-pa",
            fixture.dir_str(),
            "-eval",
            &expr,
            "-s",
            "init",
            "stop",
        ],
        tier,
        &what,
    );
    assert_eq!(
        cli.code, 0,
        "{what}: the oracle exited with {}; stderr:\n{}",
        cli.code, cli.err
    );
    cli.out
}

// ---------------------------------------------------------------------------------------
// The goldens
// ---------------------------------------------------------------------------------------

/// The fixture payload's byte length.
const PAYLOAD_BYTES: u64 = 1024;

/// The fixture payload's md5, recorded independently of any runtime (the first transcript line
/// must reproduce it, which is what pins the *input* every other golden is a function of).
const PAYLOAD_MD5: &str = "A1D3CA40959CD47D539D2A0115E99AF2";

/// The golden multi-`sync` table: `(level, per-call byte counts, total, md5)`.
///
/// The per-call vector is the five `deflate/3` calls of the transcript (four `sync` parts, then
/// `finish`) and `total` is their sum. Recorded from the system `erl` (OTP 29.1.1 / erts-17.1)
/// and re-confirmed by [`the_multi_sync_transcript_is_byte_identical_at_every_level`]'s oracle
/// probes on every run.
const LEVELS: [(u8, [u64; 5], u64, &str); 10] = [
    (
        0,
        [268, 266, 266, 266, 9],
        1075,
        "744E6AB1F1679A65419D31433B4CE009",
    ),
    (
        1,
        [102, 11, 9, 9, 6],
        137,
        "1483D581C7312DE6CF3B5536AC760D8D",
    ),
    (
        2,
        [100, 11, 9, 9, 6],
        135,
        "CA8610AA9D9C8629E0A160295B113D3D",
    ),
    (
        3,
        [100, 11, 9, 9, 6],
        135,
        "CA8610AA9D9C8629E0A160295B113D3D",
    ),
    (4, [96, 9, 9, 9, 6], 129, "A33B7229B7EDF94FDA7E879668E71BDE"),
    (5, [96, 9, 9, 9, 6], 129, "A33B7229B7EDF94FDA7E879668E71BDE"),
    (6, [96, 9, 9, 9, 6], 129, "559A0ED1917748B7D484AE0BF201F1DD"),
    (7, [96, 9, 9, 9, 6], 129, "FA42B81D126127EA40E9AA4A1F456C6B"),
    (8, [96, 9, 9, 9, 6], 129, "FA42B81D126127EA40E9AA4A1F456C6B"),
    (9, [96, 9, 9, 9, 6], 129, "FA42B81D126127EA40E9AA4A1F456C6B"),
];

/// [`LEVELS`] rendered exactly as the fixture and the `-eval` expression print it.
fn golden_levels() -> String {
    let mut lines = vec![format!("payload length {PAYLOAD_BYTES} md5 {PAYLOAD_MD5}")];
    for (level, counts, total, md5) in LEVELS {
        lines.push(format!(
            "level {level} counts [{},{},{},{},{}] total {total} md5 {md5}",
            counts[0], counts[1], counts[2], counts[3], counts[4]
        ));
    }
    lines.join("\n")
}

/// `deflate/2` (the wrapper's `none` default) and `deflate/3` with each flush, level 6.
const FLUSH_GOLDEN: &[&str] = &[
    "flush deflate_2 counts [2,100] total 102 md5 6518CCA11F87CB76E961FB89CC729D7A",
    "flush none counts [2,100] total 102 md5 6518CCA11F87CB76E961FB89CC729D7A",
    "flush sync counts [103,6] total 109 md5 B6124A9CDAD995D574DA6B302AEE8580",
    "flush full counts [103,6] total 109 md5 B6124A9CDAD995D574DA6B302AEE8580",
    "flush finish counts [102] total 102 md5 6518CCA11F87CB76E961FB89CC729D7A",
];

/// Every `deflateInit` form the suite drives: compressed size, md5 and the round-trip witness.
const FORMS_GOLDEN: &[&str] = &[
    "form deflateInit_1 compressed 102 md5 6518CCA11F87CB76E961FB89CC729D7A recovered 1024 equal true",
    "form deflateInit_2_default compressed 102 md5 6518CCA11F87CB76E961FB89CC729D7A recovered 1024 equal true",
    "form deflateInit_2_none compressed 1035 md5 91B16EEC30D6401DCC232E43E7A393EB recovered 1024 equal true",
    "form deflateInit_2_best_speed compressed 110 md5 C4D48A4306FCA6ACD204EA5FE349D2C0 recovered 1024 equal true",
    "form deflateInit_2_best_compression compressed 102 md5 76D3FC7E41AA4E1EA7D010CD9765C84B recovered 1024 equal true",
    "form deflateInit_2_integer_3 compressed 112 md5 BEDAFC04B1031656E28478DE6AA41BDF recovered 1024 equal true",
    "form deflateInit_6_default compressed 102 md5 6518CCA11F87CB76E961FB89CC729D7A recovered 1024 equal true",
    "form deflateInit_6_no_header compressed 96 md5 D4C219FCE1CCF7630F2F3C8468048FD2 recovered 1024 equal true",
    "form deflateInit_6_window_9_mem_1_filtered compressed 104 md5 C420980631B6C9D00C17E8B0B1E21192 recovered 1024 equal true",
    "form deflateInit_6_window_12_mem_9_huffman_only compressed 546 md5 73E3F9579758BB178D70E5BBFD80F61E recovered 1024 equal true",
    "form deflateInit_6_window_10_mem_5_rle compressed 546 md5 3EE83CA335CDA511FF0F8984F7673765 recovered 1024 equal true",
];

/// `deflateReset/1` mid-stream: the reset stream's transcript equals a fresh stream's.
const RESET_GOLDEN: &[&str] = &[
    "reset fresh counts [96,9,9,9,6] total 129 md5 559A0ED1917748B7D484AE0BF201F1DD",
    "reset after_reset counts [96,9,9,9,6] total 129 md5 559A0ED1917748B7D484AE0BF201F1DD",
    "reset identical true",
];

/// `deflateParams/3` mid-stream: the level/strategy switch keeps the stream readable.
const PARAMS_GOLDEN: &[&str] = &[
    "params counts [268,9,9,9,6] total 301 md5 317981B4BF5A903FD9BB0B7AB26FAEE9",
    "params recovered 1024 equal true",
];

/// `deflate` → `inflate/2`: the payload comes back byte-exact, whole and in three chunks.
const ROUND_TRIP_GOLDEN: &[&str] = &[
    "roundtrip compressed 102 compressed_md5 76D3FC7E41AA4E1EA7D010CD9765C84B",
    "roundtrip whole 1024 whole_md5 A1D3CA40959CD47D539D2A0115E99AF2 whole_equal true",
    "roundtrip chunked 1024 chunked_md5 A1D3CA40959CD47D539D2A0115E99AF2 chunked_equal true",
];

/// A golden line list as the single trimmed stdout string the probes produce.
fn golden(lines: &[&str]) -> String {
    lines.join("\n")
}

// ---------------------------------------------------------------------------------------
// The `-eval` expressions (route 1 — the tree-walking evaluator)
// ---------------------------------------------------------------------------------------

/// The placeholder the templates below carry in place of the payload literal, so the Erlang
/// text needs no `format!` brace-doubling (tuples are everywhere in it).
const PAYLOAD_PLACEHOLDER: &str = "@PAYLOAD@";

/// The fixture payload's Erlang source form — byte-identical to the one in
/// `tests/fixtures/zlib_stream_levels_fixture.erl` (the transcript's own
/// `payload length … md5 …` line is what pins the two to the same bytes).
const PAYLOAD_LITERAL: &str = r#"<<"It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity.\n">>"#;

/// `payload/0` + `hex/1` of the fixture, plus the per-level transcript.
const EXPR_LEVELS: &str = r#"F1 = binary:part(binary:copy(@PAYLOAD@, 6), 0, 1024),
Hex = fun(B) -> binary_to_list(binary:encode_hex(erlang:md5(B))) end,
io:format("payload length ~w md5 ~s~n",[byte_size(F1), Hex(F1)]),
[begin
     Z = zlib:open(), ok = zlib:deflateInit(Z, L),
     S = [iolist_to_binary(zlib:deflate(Z, binary:part(F1, I*256, 256), sync)) || I <- [0,1,2,3]],
     La = iolist_to_binary(zlib:deflate(Z, [], finish)),
     ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
     A = iolist_to_binary(S ++ [La]),
     io:format("level ~w counts ~w total ~w md5 ~s~n",
               [L, [byte_size(O) || O <- S] ++ [byte_size(La)], byte_size(A), Hex(A)])
 end || L <- lists:seq(0,9)],
halt(0)."#;

/// `deflate/2`, then `deflate/3` with `none`/`sync`/`full`/`finish`.
const EXPR_FLUSH: &str = r#"F1 = binary:part(binary:copy(@PAYLOAD@, 6), 0, 1024),
Hex = fun(B) -> binary_to_list(binary:encode_hex(erlang:md5(B))) end,
Run = fun(Flush) ->
        Z = zlib:open(), ok = zlib:deflateInit(Z, 6),
        Outs = case Flush of
                   finish -> [iolist_to_binary(zlib:deflate(Z, F1, finish))];
                   _ -> [iolist_to_binary(zlib:deflate(Z, F1, Flush)),
                         iolist_to_binary(zlib:deflate(Z, [], finish))]
               end,
        ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
        A = iolist_to_binary(Outs),
        io:format("flush ~w counts ~w total ~w md5 ~s~n",
                  [Flush, [byte_size(O) || O <- Outs], byte_size(A), Hex(A)])
    end,
Z0 = zlib:open(), ok = zlib:deflateInit(Z0, 6),
D1 = iolist_to_binary(zlib:deflate(Z0, F1)),
D2 = iolist_to_binary(zlib:deflate(Z0, [], finish)),
ok = zlib:deflateEnd(Z0), ok = zlib:close(Z0),
A0 = iolist_to_binary([D1, D2]),
io:format("flush deflate_2 counts ~w total ~w md5 ~s~n",
          [[byte_size(D1), byte_size(D2)], byte_size(A0), Hex(A0)]),
[Run(Fl) || Fl <- [none, sync, full, finish]],
halt(0)."#;

/// Every `deflateInit` form: `deflateInit/1`, `/2` and `/6`, one line each.
const EXPR_FORMS: &str = r#"F1 = binary:part(binary:copy(@PAYLOAD@, 6), 0, 1024),
Hex = fun(B) -> binary_to_list(binary:encode_hex(erlang:md5(B))) end,
One = fun({Label, Bits, Init}) ->
        Z = zlib:open(), ok = Init(Z),
        C = iolist_to_binary([zlib:deflate(Z, F1, none), zlib:deflate(Z, [], finish)]),
        ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
        I = zlib:open(), ok = zlib:inflateInit(I, Bits),
        R = iolist_to_binary(zlib:inflate(I, C)),
        ok = zlib:inflateEnd(I), ok = zlib:close(I),
        io:format("form ~w compressed ~w md5 ~s recovered ~w equal ~w~n",
                  [Label, byte_size(C), Hex(C), byte_size(R), R =:= F1])
    end,
Forms = [{deflateInit_1, 15, fun(Z) -> zlib:deflateInit(Z) end},
         {deflateInit_2_default, 15, fun(Z) -> zlib:deflateInit(Z, default) end},
         {deflateInit_2_none, 15, fun(Z) -> zlib:deflateInit(Z, none) end},
         {deflateInit_2_best_speed, 15, fun(Z) -> zlib:deflateInit(Z, best_speed) end},
         {deflateInit_2_best_compression, 15,
          fun(Z) -> zlib:deflateInit(Z, best_compression) end},
         {deflateInit_2_integer_3, 15, fun(Z) -> zlib:deflateInit(Z, 3) end},
         {deflateInit_6_default, 15,
          fun(Z) -> zlib:deflateInit(Z, default, deflated, 15, 8, default) end},
         {deflateInit_6_no_header, -15,
          fun(Z) -> zlib:deflateInit(Z, default, deflated, -15, 8, default) end},
         {deflateInit_6_window_9_mem_1_filtered, 9,
          fun(Z) -> zlib:deflateInit(Z, best_compression, deflated, 9, 1, filtered) end},
         {deflateInit_6_window_12_mem_9_huffman_only, 12,
          fun(Z) -> zlib:deflateInit(Z, best_speed, deflated, 12, 9, huffman_only) end},
         {deflateInit_6_window_10_mem_5_rle, 10,
          fun(Z) -> zlib:deflateInit(Z, 4, deflated, 10, 5, rle) end}],
[One(Fm) || Fm <- Forms],
halt(0)."#;

/// `deflateReset/1` mid-stream, compared against a fresh stream.
const EXPR_RESET: &str = r#"F1 = binary:part(binary:copy(@PAYLOAD@, 6), 0, 1024),
Parts = [binary:part(F1, O, 256) || O <- [0,256,512,768]],
Hex = fun(B) -> binary_to_list(binary:encode_hex(erlang:md5(B))) end,
Run = fun(Z) ->
        S = [iolist_to_binary(zlib:deflate(Z, P, sync)) || P <- Parts],
        La = iolist_to_binary(zlib:deflate(Z, [], finish)),
        ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
        A = iolist_to_binary(S ++ [La]),
        {[byte_size(O) || O <- S] ++ [byte_size(La)], A}
    end,
Fresh = zlib:open(), ok = zlib:deflateInit(Fresh, 6),
{FC, FA} = Run(Fresh),
Reused = zlib:open(), ok = zlib:deflateInit(Reused, 6),
_ = iolist_to_binary(zlib:deflate(Reused, hd(Parts), sync)),
_ = iolist_to_binary(zlib:deflate(Reused, lists:nth(2, Parts), none)),
ok = zlib:deflateReset(Reused),
{RC, RA} = Run(Reused),
io:format("reset fresh counts ~w total ~w md5 ~s~n",[FC, byte_size(FA), Hex(FA)]),
io:format("reset after_reset counts ~w total ~w md5 ~s~n",[RC, byte_size(RA), Hex(RA)]),
io:format("reset identical ~w~n",[RA =:= FA]),
halt(0)."#;

/// `deflateParams/3` mid-stream: two level/strategy switches, then the round trip.
const EXPR_PARAMS: &str = r#"F1 = binary:part(binary:copy(@PAYLOAD@, 6), 0, 1024),
Hex = fun(B) -> binary_to_list(binary:encode_hex(erlang:md5(B))) end,
Parts = [binary:part(F1, O, 256) || O <- [0,256,512,768]],
Z = zlib:open(), ok = zlib:deflateInit(Z, none),
A = iolist_to_binary(zlib:deflate(Z, hd(Parts), sync)),
ok = zlib:deflateParams(Z, best_compression, default),
B = iolist_to_binary(zlib:deflate(Z, lists:nth(2, Parts), sync)),
ok = zlib:deflateParams(Z, 1, filtered),
C = iolist_to_binary(zlib:deflate(Z, lists:nth(3, Parts), sync)),
D = iolist_to_binary(zlib:deflate(Z, lists:nth(4, Parts), sync)),
E = iolist_to_binary(zlib:deflate(Z, [], finish)),
ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
Outs = [A, B, C, D, E],
All = iolist_to_binary(Outs),
io:format("params counts ~w total ~w md5 ~s~n",
          [[byte_size(X) || X <- Outs], byte_size(All), Hex(All)]),
I = zlib:open(), ok = zlib:inflateInit(I),
R = iolist_to_binary(zlib:inflate(I, All)),
ok = zlib:inflateEnd(I), ok = zlib:close(I),
io:format("params recovered ~w equal ~w~n",[byte_size(R), R =:= F1]),
halt(0)."#;

/// The `deflate` → `inflate/2` round trip, whole and in three chunks.
const EXPR_ROUND_TRIP: &str = r#"F1 = binary:part(binary:copy(@PAYLOAD@, 6), 0, 1024),
Hex = fun(B) -> binary_to_list(binary:encode_hex(erlang:md5(B))) end,
Z = zlib:open(), ok = zlib:deflateInit(Z, best_compression),
C = iolist_to_binary([zlib:deflate(Z, F1, none), zlib:deflate(Z, [], finish)]),
ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
W = zlib:open(), ok = zlib:inflateInit(W),
Whole = iolist_to_binary(zlib:inflate(W, C)),
ok = zlib:inflateEnd(W), ok = zlib:close(W),
Chunk = (byte_size(C) + 2) div 3,
Offsets = [O || O <- [0, Chunk, 2*Chunk], O < byte_size(C)],
K = zlib:open(), ok = zlib:inflateInit(K),
Chunked = iolist_to_binary(
            [zlib:inflate(K, binary:part(C, O, min(Chunk, byte_size(C) - O))) || O <- Offsets]),
ok = zlib:inflateEnd(K), ok = zlib:close(K),
io:format("roundtrip compressed ~w compressed_md5 ~s~n",[byte_size(C), Hex(C)]),
io:format("roundtrip whole ~w whole_md5 ~s whole_equal ~w~n",
          [byte_size(Whole), Hex(Whole), Whole =:= F1]),
io:format("roundtrip chunked ~w chunked_md5 ~s chunked_equal ~w~n",
          [byte_size(Chunked), Hex(Chunked), Chunked =:= F1]),
halt(0)."#;

/// Substitutes the payload literal into an expression template.
fn with_payload(template: &str) -> String {
    template.replace(PAYLOAD_PLACEHOLDER, PAYLOAD_LITERAL)
}

// ---------------------------------------------------------------------------------------
// The compiled fixture (route 2 — the scheduled route)
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory (removed on drop).
struct Fixture {
    /// The scratch directory holding the fixture's `.beam`.
    dir: PathBuf,
    /// The compiled `.beam`.
    beam: PathBuf,
}

impl Fixture {
    /// The compiled image as a `str`, for the CLI's argv.
    fn beam_str(&self) -> &str {
        self.beam
            .to_str()
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", self.beam.display()))
    }

    /// The scratch directory as a `str`, for the oracle's `-pa`.
    fn dir_str(&self) -> &str {
        self.dir
            .to_str()
            .unwrap_or_else(|| panic!("the fixture dir is not UTF-8: {}", self.dir.display()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort, but never silent: a leftover scratch dir must not fail the test, yet a
        // cleanup failure is reported rather than swallowed.
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// Compiles `tests/fixtures/zlib_stream_levels_fixture.erl` into a fresh scratch directory, or
/// `None` after a **loud** skip naming what is missing.
///
/// The directory is unique per call (label + pid + a process-wide counter) and holds **only**
/// the fixture's `.beam`, so it is safe to hand the oracle as a single `-pa` (a stray `lists.beam`
/// shadowing stdlib is what `tests/fixtures/README.md` warns about, and it cannot happen here).
fn build_fixture(label: &str) -> Option<Fixture> {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(SUITE, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("{MODULE}.erl"));
    assert!(
        source.is_file(),
        "the committed fixture source is missing: {}",
        source.display()
    );
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_zlib_stream_levels_{label}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let output = match std::process::Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(SUITE, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            SUITE,
            &format!(
                "`erlc` exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
        return None;
    }
    let beam = dir.join(format!("{MODULE}.beam"));
    assert!(
        beam.is_file(),
        "`erlc` produced no {}; stderr:\n{}",
        beam.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The one matrix every surface is checked on
// ---------------------------------------------------------------------------------------

/// Runs one surface on **all four** runtime combinations plus both oracle shapes, asserting each
/// against `golden`.
///
/// `surface` names the surface in a failure (the probes name the route and the tier on their
/// own). The four combinations are `-eval` and scheduled `run`, each on the default and the
/// interpreter tier; the two oracle shapes are the same `-eval` expression under the system
/// `erl` and the oracle running the **compiled fixture**.
fn check_surface(
    oracle: &Path,
    fixture: &Fixture,
    surface: &str,
    expr: &str,
    function: &str,
    golden: &str,
) {
    for tier in TIERS {
        assert_eq!(
            ours_eval(&with_payload(expr), tier),
            golden,
            "{surface}: the `-eval` route on {}",
            tier.label()
        );
        assert_eq!(
            ours_run(fixture, function, tier),
            golden,
            "{surface}: the scheduled `run` route on {}",
            tier.label()
        );
    }
    assert_eq!(
        theirs_eval(
            oracle,
            &with_payload(expr),
            Tier::Native,
            &format!("{surface}: the oracle's `-eval`")
        ),
        golden,
        "{surface}: the oracle's own `-eval` answer"
    );
    assert_eq!(
        theirs_fixture(oracle, fixture, function, Tier::Native),
        golden,
        "{surface}: the oracle running the compiled fixture"
    );
}

// ---------------------------------------------------------------------------------------
// 1. The golden table's own consistency (no toolchain needed)
// ---------------------------------------------------------------------------------------

/// The golden table is internally consistent: ten levels 0–9 in order, five per-call counts
/// each, and `total` equal to their sum.
///
/// This is the guard that keeps a typo from passing as "the oracle's answer": the table is
/// compared byte-for-byte against both runtimes, so an internally inconsistent row would have to
/// be wrong in a way both `zlib` implementations reproduce (impossible for the sum).
#[test]
fn the_golden_level_table_is_internally_consistent() {
    assert_eq!(LEVELS.len(), 10, "one row per compression level");
    for (index, (level, counts, total, md5)) in LEVELS.iter().enumerate() {
        assert_eq!(*level as usize, index, "the table is ordered 0..9");
        assert_eq!(
            counts.iter().sum::<u64>(),
            *total,
            "level {level}: `total` is the sum of the five per-call counts {counts:?}"
        );
        assert_eq!(md5.len(), 32, "level {level}: an md5 is 32 hex digits");
        assert!(
            md5.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
            "level {level}: an uppercase hex digest, as `binary:encode_hex/1` renders it"
        );
    }
    // The level boundaries are visible in the counts: level 0 (stored) emits far more than any
    // compressed level, which is what makes the vector a real observation and not a constant.
    assert!(
        LEVELS[0].1[0] > LEVELS[1].1[0],
        "level 0 stores, level 1 compresses"
    );
}

// ---------------------------------------------------------------------------------------
// 2. The headline: the multi-`sync` transcript, byte-for-byte, per level
// ---------------------------------------------------------------------------------------

/// **The per-level multi-`sync` byte-identity table** — the suite's headline.
///
/// For each level 0–9: `deflateInit(Z, Level)`, 4 × `deflate(Z, <<256-byte part>>, sync)`,
/// `deflate(Z, [], finish)`, `deflateEnd(Z)`, `close(Z)`; the per-call byte-count vector, the
/// total and the md5 of the concatenation must equal [`LEVELS`] (`(level, counts, total, md5)`,
/// with the payload pinned as [`PAYLOAD_BYTES`] / [`PAYLOAD_MD5`]).
///
/// # What each assertion covers
///
/// * `-eval` route, **default (native baseline) tier** — the tree-walking evaluator over the
///   JIT-compiled `zlib.beam`;
/// * `-eval` route, **interpreter tier** — the same expression on the direct-threaded
///   interpreter;
/// * scheduled `run` route, **both tiers** — the compiled fixture module
///   `zlib_stream_levels_fixture:multi_sync/0`;
/// * the **oracle** answering the same `-eval` expression, and the oracle running the same
///   compiled fixture module — so the golden is re-confirmed against the live system `erl` on
///   every run, never merely trusted.
#[test]
fn the_multi_sync_transcript_is_byte_identical_at_every_level() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("levels") else {
        return;
    };
    let golden = golden_levels();
    for tier in TIERS {
        assert_eq!(
            ours_eval(&with_payload(EXPR_LEVELS), tier),
            golden,
            "the multi-`sync` transcript: the `-eval` route on {}",
            tier.label()
        );
        assert_eq!(
            ours_run(&fixture, "multi_sync", tier),
            golden,
            "the multi-`sync` transcript: the scheduled `run` route on {}",
            tier.label()
        );
    }
    assert_eq!(
        theirs_eval(
            &oracle,
            &with_payload(EXPR_LEVELS),
            Tier::Native,
            "the multi-`sync` oracle `-eval`"
        ),
        golden,
        "the multi-`sync` transcript: the oracle's own `-eval` answer"
    );
    assert_eq!(
        theirs_fixture(&oracle, &fixture, "multi_sync", Tier::Native),
        golden,
        "the multi-`sync` transcript: the oracle running the compiled fixture"
    );
}

// ---------------------------------------------------------------------------------------
// 3. The rest of the wrapper surface
// ---------------------------------------------------------------------------------------

/// **`deflate/2` and `deflate/3` with every flush** — on all four runtime combinations and both
/// oracle shapes (see [`check_surface`] for the matrix).
///
/// `deflate/2` is the wrapper's `none` default; the four `deflate/3` rows are one whole-payload
/// call with `none`/`sync`/`full`/`finish` plus the closing `finish` (a `finish` row needs no
/// second call). Note the oracle's own shape, which the goldens record: `sync` and `full` emit
/// the same 103 + 6 bytes here (the difference between the two flushes is in the *state* a
/// later block may exploit, not in this single-block output), while `none` defers its output.
#[test]
fn the_deflate_flush_modes_are_byte_identical() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("flush") else {
        return;
    };
    check_surface(
        &oracle,
        &fixture,
        "`deflate/2` and `deflate/3` with each flush",
        EXPR_FLUSH,
        "flush_modes",
        &golden(FLUSH_GOLDEN),
    );
}

/// **Every `deflateInit` form** — `deflateInit/1`, `deflateInit/2` (`default`, `none`,
/// `best_speed`, `best_compression`, and the integer `3`) and `deflateInit/6` (`deflated` with
/// window bits **15**, **-15**, **9**, **12** and **10**; memlevels 8, 1, 9 and 5; strategies
/// `default`, `filtered`, `huffman_only` and `rle`) — on all four runtime combinations and both
/// oracle shapes.
///
/// Each row records the compressed size and md5 plus a round-trip witness, so a form that
/// silently fell back to another level (or lost the raw-stream `-15` window) fails on the md5
/// even where the size coincides. `zlib.erl` accepts no `{level, L}` tuple in `deflateInit/2`
/// (`arg_level/1` takes the atoms above or an integer 0..9), so no such form appears here.
#[test]
fn the_deflate_init_forms_are_byte_identical() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("forms") else {
        return;
    };
    check_surface(
        &oracle,
        &fixture,
        "the `deflateInit` forms",
        EXPR_FORMS,
        "init_forms",
        &golden(FORMS_GOLDEN),
    );
}

/// **`deflateReset/1` mid-stream** — a stream that has already compressed two parts (one
/// `sync`, one `none`), reset, then driven through the full transcript must produce exactly the
/// fresh stream's transcript, counts and md5 included (`reset identical true`) — on all four
/// runtime combinations and both oracle shapes.
#[test]
fn a_mid_stream_deflate_reset_reproduces_a_fresh_stream() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("reset") else {
        return;
    };
    check_surface(
        &oracle,
        &fixture,
        "`deflateReset/1` mid-stream",
        EXPR_RESET,
        "reset_mid_stream",
        &golden(RESET_GOLDEN),
    );
}

/// **`deflateParams/3` mid-stream** — `none` → `best_compression`/`default` → `1`/`filtered`
/// across the four `sync` parts, then `finish`: the byte counts and md5 match the oracle and the
/// concatenation still inflates back to the payload — on all four runtime combinations and both
/// oracle shapes.
///
/// This is the wrapper's `save_progress/3` path: `zlib:deflateParams/3` flushes with `sync`
/// first and re-injects the pending output, so a runtime that dropped or duplicated those bytes
/// would move the counts.
#[test]
fn a_mid_stream_deflate_params_switch_is_byte_identical() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("params") else {
        return;
    };
    check_surface(
        &oracle,
        &fixture,
        "`deflateParams/3` mid-stream",
        EXPR_PARAMS,
        "deflate_params",
        &golden(PARAMS_GOLDEN),
    );
}

/// **A `deflate` → `inflate/2` round trip recovers the payload** — in one call and in three
/// chunks (`whole_equal true`, `chunked_equal true`), with the compressed bytes' size and md5
/// pinned too — on all four runtime combinations and both oracle shapes.
#[test]
fn a_deflate_inflate_round_trip_recovers_the_payload() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("roundtrip") else {
        return;
    };
    check_surface(
        &oracle,
        &fixture,
        "the `deflate` → `inflate/2` round trip",
        EXPR_ROUND_TRIP,
        "round_trip",
        &golden(ROUND_TRIP_GOLDEN),
    );
}
