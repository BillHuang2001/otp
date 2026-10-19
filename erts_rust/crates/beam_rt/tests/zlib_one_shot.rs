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

//! The **one-shot `zlib` fast-path non-regression + libz-byte-exactness (A.9)** suite.
//!
//! `zlib`'s six convenience functions — `compress/1`, `uncompress/1`, `zip/1`, `unzip/1`,
//! `gzip/1`, `gunzip/1` — are `zlib.erl` functions rather than `bif.tab` NIFs, so this runtime
//! serves them as **stdlib fast paths**: registered BIF keys that intercept before the
//! `.beam` wrappers run (`beam_bifs::zlib::register`). That makes them a *shortcut* past the
//! wrapper route, and a shortcut is exactly the kind of code that can drift away from the
//! observable it was meant to reproduce. This suite pins three things about it:
//!
//! 1. **The fast paths are unchanged, byte for byte** — three goldens that must hold exactly
//!    ([`GOLDEN_ZIP_HELLO3`], [`GOLDEN_COMPRESS_HELLO3`], [`GOLDEN_GZIP_HELLO`]), and the three
//!    round trips (`uncompress ∘ compress`, `unzip ∘ zip`, `gunzip ∘ gzip`) recovering the
//!    payload's own bytes over the six-payload table [`PAYLOADS_ERL`] builds, plus
//!    `gunzip/1` over **concatenated gzip members**.
//! 2. **The wrapper route agrees with the fast paths.** Each of `compress/1`, `zip/1` and
//!    `gzip/1` is a documented wrapper around `deflateInit` + `deflate(…, finish)` +
//!    `deflateEnd` (`zlib.erl`), differing only in the `deflateInit/6` **window bits** that
//!    select the framing: `15` for the RFC 1950 zlib stream, `-15` for raw RFC 1951 and
//!    `16 + 15 = 31` for RFC 1952 gzip (memLevel 8, the `default` strategy and the `default`
//!    level — zlib's `Z_DEFAULT_COMPRESSION`, which resolves to 6 — in all three). Driving
//!    that transcript through the **public stream API** must produce the one-shot bytes
//!    byte for byte, for every payload and every framing.
//! 3. **A.9: `zlib:compress/1` is libz-byte-exact** — the property
//!    `common_test/test_server_node`'s `compressed_literals` depends on. The whole compressed
//!    image is compared against the **live oracle** for the empty binary, `<<"a">>`, the
//!    1024-byte `F1` below, a 100 000-byte repetitive payload and a deterministic
//!    mixed runs/random-looking payload.
//!
//! # The payload table ([`PAYLOADS_ERL`])
//!
//! | Label | Bytes | md5 | What it pins |
//! |---|---|---|---|
//! | `empty` | 0 | `D41D8CD9…` | the framing overhead alone — `compress(<<>>)` is 8 bytes, `zip(<<>>)` 2, `gzip(<<>>)` 20 |
//! | `one` | 1 | `0CC175B9…` | the smallest non-empty input |
//! | `f1` | 1 024 | `A1D3CA40959CD47D539D2A0115E99AF2` | the `test_server_node` payload (`binary:part(binary:copy(Source, 6), 0, 1024)`) |
//! | `repetitive` | 100 000 | `C9F2F7DB…` | long, highly compressible — crossing every internal block boundary |
//! | `all_bytes` | 256 | `E2C865DB…` | every byte value once (the Huffman tables' whole alphabet) |
//! | `mixed` | 1 024 | `B3164CB7…` | deterministic `erlang:md5/1`-chained blocks interleaved with runs — **no `rand`**, so no seeding differences |
//!
//! Every payload is built by the *same Erlang source text* on both routes (the `-eval`
//! expression's [`PAYLOADS_ERL`] list and the fixture's `payload/1`); the transcript's own
//! `payload <label> bytes <N> md5 <HEX>` line pins the two constructions to the same bytes,
//! because the `-eval` transcript and the fixture transcript are compared against each other.
//!
//! # The two routes and the two tiers (four runtime combinations, two oracle shapes)
//!
//! Each surface is probed in **six** shapes, and all six must be byte-identical:
//!
//! 1. `beam_rt -noshell -eval EXPR` — the tree-walking evaluator, **default (native baseline)
//!    tier**;
//! 2. the same, **interpreter tier** (`BEAM_RT_TIER=interpreter`);
//! 3. `beam_rt -noshell run <fixture.beam> zlib_one_shot_fixture:<function>/0` — the
//!    **scheduled** route over a compiled fixture (`tests/fixtures/zlib_one_shot_fixture.erl`,
//!    compiled at test time by the system `erlc` into a throw-away scratch dir), default tier;
//! 4. the same, interpreter tier;
//! 5. the live **oracle** (`erl -noshell -eval EXPR`, OTP 29.1.1 / erts-17.1);
//! 6. the oracle running the **same compiled fixture** (`-pa <scratch> -eval 'M:F(), halt(0).'`).
//!
//! Each test states which shapes it covers in its own docs; [`checked_surface`] is the matrix.
//!
//! **The `run` route prints a result line.** The CLI's `run` action renders the fixture's
//! return value with `~w~n`, and on this route a binary renders as `<<55,56,…>>` (a
//! pre-existing printer difference unrelated to `zlib`). Every fixture function therefore
//! **prints** its transcript with `io:format/2` and returns `ok`: the compared transcript is
//! the stdout minus that one trailing `ok` line, and no binary is ever printed (byte counts
//! are integers, digests and hex images are `binary:encode_hex/1` charlists).
//!
//! # Toolchain
//!
//! The oracle is resolved from the **filesystem** ([`support::require_oracle`], never by
//! running the tool). Every probe here boots this runtime, and boot resolves the *system code
//! path*, so the installed OTP is part of the suite's fixture as well; without it the suite
//! declines with the shared loud-skip banner instead of failing, and the `run` route's missing
//! `erlc` is a loud skip too ([`build_fixture`]). Nothing here is a silent pass and nothing is
//! a panic for a missing toolchain.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

// ---------------------------------------------------------------------------------------
// Names, paths and the tier axis
// ---------------------------------------------------------------------------------------

/// The suite's name in the loud-skip banner.
const SUITE: &str = "zlib_one_shot";

/// The fixture module (its source lives in `tests/fixtures/`, its `.beam` is never committed).
const MODULE: &str = "zlib_one_shot_fixture";

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// `zlib:zip(<<"hello hello hello">>)` — raw RFC 1951, no header, no trailer.
const GOLDEN_ZIP_HELLO3: &str = "CB48CDC9C957C8409000";

/// `zlib:compress(<<"hello hello hello">>)` — RFC 1950: `78 9C` header (deflate, 32K window,
/// `Z_DEFAULT_COMPRESSION`), the raw body, then the big-endian Adler-32.
const GOLDEN_COMPRESS_HELLO3: &str = "789CCB48CDC9C957C84090003A2E067D";

/// `zlib:gzip(<<"hello">>)` — RFC 1952: `1f 8b 08 00` (magic, CM=deflate, FLG=0),
/// `00000000` (MTIME), `00` (XFL, level 6), `03` (OS = Unix), the raw body, then the
/// little-endian CRC-32 and ISIZE.
const GOLDEN_GZIP_HELLO: &str = "1F8B0800000000000003CB48CDC9C9070086A6103605000000";

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
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists.
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

/// The environment every probe gets: no crash-dump litter, and the requested execution tier.
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

/// The oracle's transcript for the same `-eval` expression (the expression is self-contained,
/// so no `-pa` is needed), with the `-noshell` / `-s init stop` shape the shared helper uses —
/// spelled out here so the probe also carries the suite's environment
/// (`ERL_CRASH_DUMP_SECONDS=0`).
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

/// Compiles `tests/fixtures/zlib_one_shot_fixture.erl` into a fresh scratch directory, or
/// `None` after a **loud** skip naming what is missing.
///
/// The directory is unique per call (label + pid + a process-wide counter) and holds **only**
/// the fixture's `.beam`, so it is safe to hand the oracle as a single `-pa` (a stray
/// `lists.beam` shadowing stdlib is what `tests/fixtures/README.md` warns about, and it cannot
/// happen here).
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
        "beam_rt_zlib_one_shot_{label}_{}_{n}",
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
// The `-eval` expressions (route 1 — the tree-walking evaluator)
// ---------------------------------------------------------------------------------------

/// The placeholder the templates below carry in place of the payload table, so the Erlang text
/// needs no `format!` brace-doubling (tuples and `#{…}`-free maps are everywhere in it).
const PAYLOADS_PLACEHOLDER: &str = "@PAYLOADS@";

/// The payload table as Erlang source — the **same six payloads** the fixture's `payload/1`
/// builds (`tests/fixtures/zlib_one_shot_fixture.erl`), in the same order. A drift between the
/// two shows up immediately: the `--eval` transcript and the fixture transcript are compared
/// against each other, and the `payload <label> bytes <N> md5 <HEX>` line is part of them.
///
/// * `empty` — the empty binary;
/// * `one` — `<<"a">>`, the smallest non-empty input;
/// * `f1` — 1 024 bytes, md5 `A1D3CA40959CD47D539D2A0115E99AF2` (the `test_server_node` shape);
/// * `repetitive` — `binary:copy(<<"0123456789abcdef">>, 6250)`, exactly 100 000 bytes;
/// * `all_bytes` — every byte value once;
/// * `mixed` — 1 024 bytes of deterministic `erlang:md5/1`-chained blocks interleaved with runs
///   (**no `rand`**, so there is no seeding difference between the two runtimes).
const PAYLOADS_ERL: &str = r#"[{empty, <<>>},
            {one, <<"a">>},
            {f1, binary:part(binary:copy(<<"It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity.\n">>, 6), 0, 1024)},
            {repetitive, binary:copy(<<"0123456789abcdef">>, 6250)},
            {all_bytes, list_to_binary(lists:seq(0,255))},
            {mixed, iolist_to_binary([[erlang:md5(integer_to_binary(I)), binary:copy(<<"ab">>, 8)] || I <- lists:seq(1,32)])}]"#;

/// The three named goldens, as the fixture's `goldens/0` prints them.
const EXPR_GOLDENS: &str = r#"Hex = fun(B) -> binary_to_list(binary:encode_hex(B)) end,
io:format("zip hello_hello_hello ~s~n",[Hex(zlib:zip(<<"hello hello hello">>))]),
io:format("compress hello_hello_hello ~s~n",[Hex(zlib:compress(<<"hello hello hello">>))]),
io:format("gzip hello ~s~n",[Hex(zlib:gzip(<<"hello">>))]),
halt(0)."#;

/// The three compressors' bytes (size + md5) and the three round trips (recovered size, md5 and
/// an equality witness against the payload's own bytes), one line pair per payload.
///
/// The digest is `erlang:md5/1` of the whole compressed value — a changed block boundary, a
/// changed flush marker or a changed header/trailer moves it.
const EXPR_FAST_PATHS: &str = r#"Hex = fun(B) -> binary_to_list(binary:encode_hex(B)) end,
Payloads = @PAYLOADS@,
[begin
     C = zlib:compress(D), Z = zlib:zip(D), G = zlib:gzip(D),
     io:format("payload ~w bytes ~w md5 ~s~n",[L, byte_size(D), Hex(erlang:md5(D))]),
     io:format("~w compress ~w ~s zip ~w ~s gzip ~w ~s~n",
               [L, byte_size(C), Hex(erlang:md5(C)),
                byte_size(Z), Hex(erlang:md5(Z)),
                byte_size(G), Hex(erlang:md5(G))]),
     U = zlib:uncompress(C), UZ = zlib:unzip(Z), UG = zlib:gunzip(G),
     io:format("~w roundtrip uncompress ~w ~s ~w unzip ~w ~s ~w gunzip ~w ~s ~w~n",
               [L,
                byte_size(U), Hex(erlang:md5(U)), U =:= D,
                byte_size(UZ), Hex(erlang:md5(UZ)), UZ =:= D,
                byte_size(UG), Hex(erlang:md5(UG)), UG =:= D])
 end || {L, D} <- Payloads],
halt(0)."#;

/// The wrapper transcript: for each framing (`compress` = window bits 15, `zip` = `-15`,
/// `gzip` = `31`) and each payload, `deflateInit/6` + `deflate/3` with `finish` +
/// `deflateEnd/1` + `close/1` driven through the public stream API, next to the one-shot
/// function's own bytes.
const EXPR_WRAPPER: &str = r#"Hex = fun(B) -> binary_to_list(binary:encode_hex(B)) end,
Payloads = @PAYLOADS@,
W = fun(Init, D) ->
        Z = zlib:open(), ok = Init(Z),
        Out = iolist_to_binary(zlib:deflate(Z, D, finish)),
        ok = zlib:deflateEnd(Z), ok = zlib:close(Z),
        Out
    end,
Framings = [{compress, fun(Z) -> zlib:deflateInit(Z, default) end},
            {zip, fun(Z) -> zlib:deflateInit(Z, default, deflated, -15, 8, default) end},
            {gzip, fun(Z) -> zlib:deflateInit(Z, default, deflated, 31, 8, default) end}],
[begin
     OneShot = case Fn of
                   compress -> zlib:compress(D);
                   zip -> zlib:zip(D);
                   gzip -> zlib:gzip(D)
               end,
     Wrapped = W(Init, D),
     io:format("wrapper ~w ~w bytes ~w md5 ~s one_shot ~w md5 ~s same ~w~n",
               [Fn, L,
                byte_size(Wrapped), Hex(erlang:md5(Wrapped)),
                byte_size(OneShot), Hex(erlang:md5(OneShot)), Wrapped =:= OneShot])
 end || {Fn, Init} <- Framings, {L, D} <- Payloads],
halt(0)."#;

/// A.9: the **whole** `zlib:compress/1` image for the five payloads the libz-byte-exactness
/// property is pinned on, as source size + md5 and compressed size + md5 + full hex image.
///
/// `f1` dominates the transcript (102 bytes of output, 204 hex chars) and `mixed` follows;
/// `repetitive`'s 235 bytes are a long run of `EE`s, which is itself the observation (the
/// stored/`default`-level choice and the run encoding agree with libz's).
const EXPR_LIBZ: &str = r#"Hex = fun(B) -> binary_to_list(binary:encode_hex(B)) end,
Payloads = @PAYLOADS@,
[begin
     C = zlib:compress(D),
     io:format("libz ~w source ~w ~s compressed ~w ~s image ~s~n",
               [L, byte_size(D), Hex(erlang:md5(D)),
                byte_size(C), Hex(erlang:md5(C)), Hex(C)])
 end || {L, D} <- Payloads, lists:member(L, [empty, one, f1, repetitive, mixed])],
halt(0)."#;

/// `gunzip/1` over **concatenated gzip members**: three separately compressed members joined
/// into one binary recover as the payloads concatenated (`zlib.erl` inflates with
/// `inflateInit(Z, 16 + 15, reset)` — the `reset` option is what continues past a member's
/// trailer), and a single member still recovers on its own.
const EXPR_CONCAT: &str = r#"Hex = fun(B) -> binary_to_list(binary:encode_hex(B)) end,
F1 = binary:part(binary:copy(<<"It was the best of times, it was the worst of times, it was the age of wisdom, it was the age of foolishness, it was the epoch of belief, it was the epoch of incredulity.\n">>, 6), 0, 1024),
Parts = [<<"first member ">>, <<"second member">>, F1],
Members = [zlib:gzip(Part) || Part <- Parts],
Joined = iolist_to_binary(Members),
Recovered = zlib:gunzip(Joined),
Whole = iolist_to_binary(Parts),
io:format("concat members ~w joined ~w recovered ~w md5 ~s equal ~w~n",
          [length(Members), byte_size(Joined), byte_size(Recovered),
           Hex(erlang:md5(Recovered)), Recovered =:= Whole]),
Single = zlib:gunzip(hd(Members)),
io:format("concat single member ~w recovered ~w equal ~w~n",
          [byte_size(hd(Members)), byte_size(Single), Single =:= hd(Parts)]),
halt(0)."#;

/// Substitutes the payload table into an expression template.
fn with_payloads(template: &str) -> String {
    template.replace(PAYLOADS_PLACEHOLDER, PAYLOADS_ERL)
}

// ---------------------------------------------------------------------------------------
// The matrix every surface is checked on
// ---------------------------------------------------------------------------------------

/// Runs one surface in all six shapes and asserts they are **byte-identical**, returning the
/// (single) transcript they all produced.
///
/// The six shapes are, in probe order: `-eval` and scheduled `run` on each of [`TIERS`] (four
/// runtime combinations), then the oracle's own `-eval` answer and the oracle running the
/// compiled fixture. `surface` names the surface in a failure. The reference is the oracle's
/// `-eval` transcript, so a divergence is reported against the live system `erl` rather than
/// against a constant; callers that also carry a golden assert it against the returned
/// transcript (see [`the_three_named_goldens_are_byte_identical`]).
fn checked_surface(
    oracle: &Path,
    fixture: &Fixture,
    surface: &str,
    expr: &str,
    function: &str,
) -> String {
    let expr = with_payloads(expr);
    let mut shapes: Vec<(String, String)> = Vec::with_capacity(6);
    for tier in TIERS {
        shapes.push((
            format!("the `-eval` route on {}", tier.label()),
            ours_eval(&expr, tier),
        ));
    }
    for tier in TIERS {
        shapes.push((
            format!("the scheduled `run` route on {}", tier.label()),
            ours_run(fixture, function, tier),
        ));
    }
    let reference = theirs_eval(
        oracle,
        &expr,
        Tier::Native,
        &format!("{surface}: the oracle's `-eval`"),
    );
    shapes.push((
        "the oracle's own `-eval` answer".to_string(),
        reference.clone(),
    ));
    shapes.push((
        "the oracle running the compiled fixture".to_string(),
        theirs_fixture(oracle, fixture, function, Tier::Native),
    ));
    for (shape, answer) in &shapes {
        assert_eq!(
            answer, &reference,
            "{surface}: {shape} must produce the oracle's own bytes"
        );
    }
    reference
}

// ---------------------------------------------------------------------------------------
// 1. The goldens' own shape (no toolchain needed)
// ---------------------------------------------------------------------------------------

/// The three golden hex strings are well formed and carry the framing each function promises —
/// a guard that keeps a typo from passing as "the oracle's answer" in the comparison tests.
///
/// This is the only test here that needs no toolchain: it reads the constants alone.
#[test]
fn the_golden_hex_images_are_well_formed() {
    for (name, golden) in [
        ("zip", GOLDEN_ZIP_HELLO3),
        ("compress", GOLDEN_COMPRESS_HELLO3),
        ("gzip", GOLDEN_GZIP_HELLO),
    ] {
        assert_eq!(golden.len() % 2, 0, "`{name}`: a hex image has whole bytes");
        assert!(
            golden
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()),
            "`{name}`: an uppercase hex image, as `binary:encode_hex/1` renders it, got `{golden}`"
        );
    }
    // RFC 1950: `78 9C` is deflate with a 32K window at `Z_DEFAULT_COMPRESSION`, then a
    // big-endian Adler-32 trailer that these two bytes of the payload cannot collide with.
    assert!(
        GOLDEN_COMPRESS_HELLO3.starts_with("789C"),
        "`compress` writes the RFC 1950 header, got `{GOLDEN_COMPRESS_HELLO3}`"
    );
    // RFC 1952: magic `1f 8b`, CM = 8 (deflate).
    assert!(
        GOLDEN_GZIP_HELLO.starts_with("1F8B08"),
        "`gzip` writes the RFC 1952 header, got `{GOLDEN_GZIP_HELLO}`"
    );
    // RFC 1951 raw: no header at all, so it is neither of the two above and is strictly
    // shorter than the same payload framed as a zlib stream (which adds 6 bytes: 2 + 4).
    assert!(
        !GOLDEN_ZIP_HELLO3.starts_with("789C") && !GOLDEN_ZIP_HELLO3.starts_with("1F8B"),
        "`zip` is the raw framing, got `{GOLDEN_ZIP_HELLO3}`"
    );
    assert_eq!(
        GOLDEN_COMPRESS_HELLO3.len() - GOLDEN_ZIP_HELLO3.len(),
        6 * 2,
        "RFC 1950 adds exactly the 2-byte header and the 4-byte Adler-32 to the raw body"
    );
    // The three goldens describe the same deflate body for the same 17-byte payload.
    assert!(
        GOLDEN_COMPRESS_HELLO3.contains(GOLDEN_ZIP_HELLO3),
        "`compress` and `zip` share the raw body: `{GOLDEN_ZIP_HELLO3}` inside \
         `{GOLDEN_COMPRESS_HELLO3}`"
    );
    // The gzip member of `<<"hello">>`: `1f 8b 08 00` + MTIME(4) + XFL + OS, so the body
    // starts 10 bytes in and the CRC-32/ISIZE trailer ends it (8 bytes).
    assert_eq!(
        GOLDEN_GZIP_HELLO.len(),
        (10 + 7 + 8) * 2,
        "a 5-byte payload's member is the 10-byte header, 7 deflate bytes and the 8-byte trailer"
    );
}

// ---------------------------------------------------------------------------------------
// 2. The three named goldens
// ---------------------------------------------------------------------------------------

/// **The three named goldens hold exactly, and the live oracle produces them too.**
///
/// `zlib:zip(<<"hello hello hello">>)`, `zlib:compress(<<"hello hello hello">>)` and
/// `zlib:gzip(<<"hello">>)` are compared as **whole hex images** against
/// [`GOLDEN_ZIP_HELLO3`], [`GOLDEN_COMPRESS_HELLO3`] and [`GOLDEN_GZIP_HELLO`] — and that same
/// transcript must be what all six shapes produce (the four runtime combinations plus the
/// oracle's `-eval` and the oracle running the compiled fixture).
///
/// # What each assertion covers
///
/// * the golden text is built from the three constants and compared to the **oracle's** answer
///   first, so a stale golden cannot pass;
/// * then every one of the six shapes must equal it (see [`checked_surface`]).
#[test]
fn the_three_named_goldens_are_byte_identical() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("goldens") else {
        return;
    };
    let golden = [
        format!("zip hello_hello_hello {GOLDEN_ZIP_HELLO3}"),
        format!("compress hello_hello_hello {GOLDEN_COMPRESS_HELLO3}"),
        format!("gzip hello {GOLDEN_GZIP_HELLO}"),
    ]
    .join("\n");
    let transcript = checked_surface(
        &oracle,
        &fixture,
        "the three named one-shot goldens",
        EXPR_GOLDENS,
        "goldens",
    );
    assert_eq!(
        golden, transcript,
        "the three named goldens (zip/compress of `<<\"hello hello hello\">>`, gzip of \
         `<<\"hello\">>`) are unchanged, byte for byte"
    );
}

// ---------------------------------------------------------------------------------------
// 3. The six fast paths over the payload table
// ---------------------------------------------------------------------------------------

/// **The six one-shot functions over the six-payload table**, byte-identical on all six shapes
/// and round-tripping to the payload's own bytes.
///
/// Per payload the transcript records the payload's `bytes`/`md5` (so the two routes are known
/// to feed the same input), each compressor's output size and md5, and each decompressor's
/// recovered size, md5 and an `=:=` witness against the original — the recovered **bytes**, not
/// merely `true`. The payloads are the empty binary, one byte, the 1024-byte `F1`, a
/// 100 000-byte repetitive payload, all 256 byte values and a deterministic md5-chained mix.
#[test]
fn the_six_one_shot_fast_paths_are_byte_identical() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("fast_paths") else {
        return;
    };
    let transcript = checked_surface(
        &oracle,
        &fixture,
        "the six one-shot fast paths",
        EXPR_FAST_PATHS,
        "fast_paths",
    );
    // The transcript is not vacuous: one payload header and two result lines per payload.
    assert_eq!(
        transcript.lines().count(),
        3 * 6,
        "one header plus two lines per payload, got:\n{transcript}"
    );
    for label in ["empty", "one", "f1", "repetitive", "all_bytes", "mixed"] {
        assert!(
            transcript.contains(&format!("payload {label} bytes ")),
            "the transcript covers `{label}:\n{transcript}"
        );
        assert!(
            transcript.contains(&format!("{label} roundtrip uncompress ")),
            "the round trip for `{label}` is recorded:\n{transcript}"
        );
    }
    // The empty binary is its own observation: the framings' overhead alone, and every
    // decompressor recovering 0 bytes from it.
    assert!(
        transcript.contains(
            "empty compress 8 FB0FC3AB8C050179A378DCB10368ACF6 \
             zip 2 598F4FE64AEFAB8F00BCBEA4C9239ABF \
             gzip 20 7029066C27AC6F5EF18D660D5741979A"
        ),
        "`zlib:compress(<<>>)` is the 8-byte empty zlib stream (`78 9C 03 00 00 00 00 01`), \
         `zip(<<>>)` the 2-byte raw one and `gzip(<<>>)` the 20-byte member:\n{transcript}"
    );
    // `F1`'s compressed bytes are the figures the sibling stream suites pin for level 6 at
    // window bits 15 and -15 — the two suites must agree about the same payload.
    assert!(
        transcript.contains(
            "f1 compress 102 6518CCA11F87CB76E961FB89CC729D7A \
             zip 96 D4C219FCE1CCF7630F2F3C8468048FD2"
        ),
        "`F1`'s compress/zip images are the level-6 figures `tests/zlib_stream_levels.rs` \
         pins (102/`6518CCA1…` as an RFC 1950 stream, 96/`D4C219FC…` raw):\n{transcript}"
    );
}

// ---------------------------------------------------------------------------------------
// 4. The wrapper route reproduces the fast paths
// ---------------------------------------------------------------------------------------

/// **The wrapper route reproduces each one-shot function's bytes exactly.**
///
/// `zlib.erl` documents the three compressors as thin wrappers whose only difference is the
/// `deflateInit/6` **window bits** that select the framing:
///
/// | One-shot | Transcript | Window bits |
/// |---|---|---|
/// | `compress/1` | `deflateInit(Z, default)` (which forwards to `deflateInit/6` with `deflated, 15, 8, default`) | **15** (RFC 1950) |
/// | `zip/1` | `deflateInit(Z, default, deflated, -15, 8, default)` | **-15** (RFC 1951, raw) |
/// | `gzip/1` | `deflateInit(Z, default, deflated, 31, 8, default)` | **31** = `16 + 15` (RFC 1952) |
///
/// followed by `deflate(Z, Data, finish)`, `deflateEnd(Z)` and `close(Z)`. Driving exactly that
/// through the public stream API must produce the fast path's bytes — the assertion is
/// `Wrapped =:= OneShot` **and** equal sizes/md5s, per framing and per payload, on all six
/// shapes.
///
/// No framing needs to be excused: all three are reproducible through the public wrapper, at
/// every payload including the empty binary (where the framing — not the payload — decides
/// every byte).
#[test]
fn the_wrapper_route_reproduces_the_one_shot_bytes() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("wrapper") else {
        return;
    };
    let transcript = checked_surface(
        &oracle,
        &fixture,
        "the wrapper route",
        EXPR_WRAPPER,
        "wrapper",
    );
    assert_eq!(
        transcript.lines().count(),
        3 * 6,
        "one line per (framing, payload) pair, got:\n{transcript}"
    );
    // Every row must witness equality; a `same false` row is exactly the regression this
    // surface exists to catch, so it is named rather than left to the transcript comparison.
    let false_rows: Vec<&str> = transcript
        .lines()
        .filter(|line| line.ends_with("same false"))
        .collect();
    assert!(
        false_rows.is_empty(),
        "the wrapper route must reproduce every one-shot image byte for byte; \
         these rows do not:\n{}",
        false_rows.join("\n")
    );
    for framing in ["compress", "zip", "gzip"] {
        for label in ["empty", "one", "f1", "repetitive", "all_bytes", "mixed"] {
            assert!(
                transcript.contains(&format!("wrapper {framing} {label} bytes ")),
                "the transcript covers `{framing}` on `{label}`:\n{transcript}"
            );
        }
    }
    // The empty binary is where a framing difference would be invisible in a size alone, so
    // the three rows are pinned by md5: the wrapper writes the *same* header/trailer bytes.
    for row in [
        "wrapper compress empty bytes 8 md5 FB0FC3AB8C050179A378DCB10368ACF6 \
         one_shot 8 md5 FB0FC3AB8C050179A378DCB10368ACF6 same true",
        "wrapper zip empty bytes 2 md5 598F4FE64AEFAB8F00BCBEA4C9239ABF \
         one_shot 2 md5 598F4FE64AEFAB8F00BCBEA4C9239ABF same true",
        "wrapper gzip empty bytes 20 md5 7029066C27AC6F5EF18D660D5741979A \
         one_shot 20 md5 7029066C27AC6F5EF18D660D5741979A same true",
    ] {
        assert!(
            transcript.contains(row),
            "the empty-binary row `{row}` is missing from:\n{transcript}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// 5. A.9 — libz byte exactness
// ---------------------------------------------------------------------------------------

/// **A.9: `zlib:compress/1` is libz-byte-exact** — the property
/// `common_test/test_server_node`'s `compressed_literals` depends on.
///
/// The **whole** compressed image (not only a digest) is compared against the live oracle for
/// the five payloads the property is stated on: the empty binary, `<<"a">>`, the 1024-byte
/// `F1`, the 100 000-byte repetitive payload and the deterministic mixed one. The transcript
/// carries source size + md5 and compressed size + md5 + hex image, so a divergence names the
/// payload and shows both byte strings.
///
/// This is the A.9 half of the suite: the runtime's deflate is a port of the same
/// `erts/emulator/zlib/{deflate,trees}.c` the oracle links, and this is where that claim is
/// re-measured against the installed `erl` rather than asserted in prose — on the four runtime
/// combinations and both oracle shapes.
#[test]
fn zlib_compress_is_libz_byte_exact() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("libz") else {
        return;
    };
    let transcript = checked_surface(
        &oracle,
        &fixture,
        "the libz-byte-exact `zlib:compress/1` images",
        EXPR_LIBZ,
        "libz",
    );
    assert_eq!(
        transcript.lines().count(),
        5,
        "one row per payload the property is pinned on, got:\n{transcript}"
    );
    // The empty zlib stream is the framing alone, and it is fully specified by RFC 1950:
    // `78 9C` (deflate, 32K window, default level) + an empty final stored block `03 00`
    // + the Adler-32 of the empty string `00000001`.
    assert!(
        transcript.contains(
            "libz empty source 0 D41D8CD98F00B204E9800998ECF8427E \
             compressed 8 FB0FC3AB8C050179A378DCB10368ACF6 image 789C030000000001"
        ),
        "`zlib:compress(<<>>)` is exactly `789C030000000001`:\n{transcript}"
    );
    // One byte: `78 9C` + the raw body `4B 04 00` + Adler-32 `62 00 62`. The body's stored
    // vs compressed choice at this size is libz's own tie-break (`stored_len + 4 <=
    // opt_lenb`), which is what makes a one-byte image a real observation.
    assert!(
        transcript.contains(
            "libz one source 1 0CC175B9C0F1B6A831C399E269772661 \
             compressed 9 68939314EBCDD39B453A1D86B65970B5 image 789C4B040000620062"
        ),
        "`zlib:compress(<<\"a\">>)` is exactly `789C4B040000620062`:\n{transcript}"
    );
    // `F1` — the `test_server_node` payload — is the 102-byte level-6 stream the sibling
    // stream suite pins at the same level/window (`6518CCA1…`).
    assert!(
        transcript.contains("libz f1 source 1024 A1D3CA40959CD47D539D2A0115E99AF2 compressed 102 6518CCA11F87CB76E961FB89CC729D7A image 789CEDCEC10980301044D1BB556C01621F96A171621692ACB82BC1EEC59308F1E631E737037F342A939205D00C35124FC609DA133F5264FFA269C50D85759154132F12594386BE8FD8C4857B3023327CDD38BB1DCB11D9CEA11B5B6A4B6DA9FFA65E499E6670"),
        "`F1`'s whole 102-byte image must be the oracle's:\n{transcript}"
    );
    // The long repetitive payload: 235 bytes, and the sizes/md5 of the other two rows must be
    // present too (their hex images are long; the transcript comparison covers them).
    assert!(
        transcript.contains("libz repetitive source 100000 C9F2F7DB06F1915BBF035EBA63660B92 compressed 235 EF7D9EA60825313126B1956BB1B2378D image "),
        "the 100 000-byte payload's whole image must be the oracle's:\n{transcript}"
    );
    assert!(
        transcript.contains("libz mixed source 1024 B3164CB76138B91D9A6BCC489AE2E42E compressed 611 C53223A947C179E6C1AA11F7FE5CADA8 image "),
        "the mixed payload's whole image must be the oracle's:\n{transcript}"
    );
}

// ---------------------------------------------------------------------------------------
// 6. Concatenated gzip members
// ---------------------------------------------------------------------------------------

/// **`gunzip/1` over concatenated gzip members concatenates.**
///
/// Three members are compressed separately and joined; `zlib:gunzip/1` on the joined binary must
/// recover the three payloads **concatenated** (1050 bytes here) — `zlib.erl` inflates with
/// `inflateInit(Z, 16 + 15, reset)`, and it is that `reset` option which reads past a member's
/// trailer instead of stopping at the first stream end. A single member still recovers on its
/// own, so the concatenating path is not the only one the suite exercises.
///
/// On all six shapes; the oracle's `-eval` answer is the reference.
#[test]
fn gunzip_of_concatenated_members_concatenates() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = build_fixture("concat") else {
        return;
    };
    let transcript = checked_surface(
        &oracle,
        &fixture,
        "`gunzip/1` over concatenated gzip members",
        EXPR_CONCAT,
        "concat",
    );
    assert_eq!(
        transcript.lines().count(),
        2,
        "the concatenated and the single-member rows, got:\n{transcript}"
    );
    // 13 + 13 + 1024 bytes recovered from 3 members joined into 180 bytes: the members are
    // the gzip framing plus the compressed payloads, and the recovered value is their
    // concatenation (md5 `EE098CD2DA191DFFD2F6AF55F5438234`).
    assert!(
        transcript.contains(
            "concat members 3 joined 180 recovered 1050 \
             md5 EE098CD2DA191DFFD2F6AF55F5438234 equal true"
        ),
        "the three members recover as their payloads concatenated:\n{transcript}"
    );
    assert!(
        transcript.contains("concat single member 33 recovered 13 equal true"),
        "one member on its own still recovers its payload:\n{transcript}"
    );
}
