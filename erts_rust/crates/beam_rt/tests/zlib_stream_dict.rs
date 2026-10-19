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

//! The **`zlib` stream-API host suite**: the stash path, the `need_dictionary` protocol, the
//! `safeInflate/2` chunking, the reset/dictionary accessors and the truncated-stream
//! detection — the whole `lib/kernel`-facing surface of `erts/preloaded/src/zlib.erl`, driven
//! through the **real preloaded `zlib.beam`** and cross-checked against the live system `erl`.
//!
//! # What this suite pins
//!
//! | Surface | Pinned shape |
//! |---|---|
//! | `deflateParams/3`'s **stash** path | the level-0 stream's `[2]` header leaf, `deflateParams/3` → `ok`, and the **two** recovered leaves `[26, 8]` (36 B in total) from the next `deflate/3`, whose concatenation `zlib:uncompress/1` round-trips |
//! | the **`need_dictionary`** protocol | the Adler digest `2662864480`, the exact 15-byte stream, the raising `inflate/2`, the `{need_dictionary, _, []}` shape of both non-raising routes, the recovery after `inflateSetDictionary/2`, and the need's per-handle continuity |
//! | `safeInflate/2` **chunking** | six `{continue, [16384]}` chunks, then `{finished, [1696]}` and a final `{finished, []}`, on a 100 000-byte payload |
//! | `inflateReset/1`, `deflateReset/1`, `inflateGetDictionary/1` | a mid-stream reset followed by a whole-stream inflate, an empty fresh dictionary vs the 59-byte fed one, and a deflate reset that restarts from the zlib header |
//! | a **truncated** stream | `inflate/2` does **not** raise on the prefix missing the Adler trailer; only `inflateEnd/1` raises `data_error` |
//!
//! # Two routes × two execution tiers — four combinations, all asserted
//!
//! 1. **`-eval`** — `beam_rt -noshell -eval EXPR` against the oracle's
//!    `erl -noshell -eval EXPR -s init stop`. The probes are the `*_PROGRAM` constants and are
//!    recorded in the `E*_GOLDEN` constants.
//! 2. **the scheduled `run` route** — `beam_rt run <zlib_stream.beam> zlib_stream:<f>/0`, where
//!    the fixture ([`FIXTURE_ERL`], committed as `tests/fixtures/zlib_stream.erl`) is compiled
//!    **at test time** by the system `erlc` into a scratch directory, and the oracle is asked
//!    the same function (`io:format("~w~n",[zlib_stream:<f>()])`). Its goldens are the
//!    `R*_GOLDEN` constants.
//! 3. Both are run on **both** execution tiers — the default **native baseline** tier and
//!    `BEAM_RT_TIER=interpreter` — and every assertion is stated against the same golden, so a
//!    tier divergence fails the test by construction. Each test's docs name the combination
//!    its assertions cover.
//!
//! # Why the goldens travel as integer lists, sizes and booleans
//!
//! The `run` route prints its result with `~w`, and this runtime's `~w`/`~p` printer renders a
//! **binary** as `<<55,56,…>>` while the oracle prints `<<"AB">>` for printable content — a
//! pre-existing printer limitation with nothing to do with `zlib`. Every fixture therefore
//! returns integers, sizes and booleans: a hex digest travels as
//! `binary_to_list(binary:encode_hex/1)` (an integer list, identical on both sides), a payload
//! as its size plus a `=:=` boolean. The `-eval` probes are free to print a hex digest as a
//! string (`~s`), and do so for the two cases where the compressed bytes themselves are the
//! golden.
//!
//! # The `deflateParams/3` stash, in one paragraph
//!
//! `zlib:deflateParams/3` (`zlib.erl:352-360`) runs its own `deflate(Z, <<>>, sync)` and then
//! **stows** that output in the stream handle through `setStash_nif/2`, returning only `ok`.
//! The next `deflate/3` starts with `restore_progress/2`, which clears the stash and prepends
//! it — so the sync-flush bytes reappear as the *first* leaf of the following iolist, and
//! `-1` of the observed leaves is a nested list rather than a binary (`lists:flatten/1` is what
//! makes the two leaves visible: `[26, 8]`). Losing the stash silently would still produce a
//! plausible-looking compressed tail, which is why the golden is the **concatenation** and the
//! round-trip rather than the tail alone.
//!
//! # The `need_dictionary` protocol, and what "continuity" means here
//!
//! `deflateSetDictionary/2` returns the dictionary's Adler-32 (`2662864480`), and the deflate
//! output carries only that digest — the decompressor must be handed the same dictionary.
//! `inflate/2` raises `error:{need_dictionary, Adler}` (and, before raising, saves the partial
//! output in the stash); `inflate/3` with `[{exception_on_need_dict, false}]` and
//! `safeInflate/2` return `{need_dictionary, Adler, Output}` instead. The suite pins that the
//! **need is stream state, not global**: a fresh handle and a reset handle both raise it again,
//! while the handle that was actually fed the dictionary recovers the payload byte-exactly, and
//! a second `inflate/2` on the already-raised handle answers `data_error` (not a second need).
//!
//! # A note on the recorded 15-byte dictionary stream
//!
//! The golden for the dictionary-compressed payload is
//! `78BB9EB8166023240F21011D420E92` — **30 hex digits, 15 bytes**, byte-identical between this
//! runtime and the oracle on this host (the deflate output is a function of the input, level
//! and zlib's own emit order, so it is stable). The task statement for this suite transcribed
//! the same value with the `01` pair missing (a 14-byte string it described as 15 bytes); the
//! **measured** and asserted golden is the one above, and the length is asserted separately
//! (`byte_size(S) == 15`) so a transcription slip cannot survive as a silent shape change.
//!
//! # Skip policy and toolchain
//!
//! `erl` is resolved from the **filesystem** only ([`support::require_oracle`], never by running
//! the tool merely to probe for it) and a missing toolchain is a **loud skip** — never a silent
//! pass and never a panic. Every `-eval` probe additionally boots the runtime, which resolves
//! the installed system code path, so the same gate applies. The `run`-route half additionally
//! needs `erlc`, discovered the same way; when it is absent that half skips **loudly** on its
//! own and the `-eval` half still runs.
#![deny(unsafe_code)]
#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const BANNER: &str = "zlib_stream_dict";

/// The **real** executable under test (the test target's build guarantees it is current).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The committed fixture source (`tests/fixtures/zlib_stream.erl`), compiled at test time.
const FIXTURE_ERL: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/zlib_stream.erl"
);

/// The fixture module's name.
const FIXTURE_MODULE: &str = "zlib_stream";

// ---------------------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------------------

/// The live `erl` oracle, resolved **once**: a missing toolchain is then a single loud skip
/// rather than one banner per comparison.
fn oracle() -> Option<PathBuf> {
    static ORACLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    ORACLE
        .get_or_init(|| support::require_oracle(BANNER))
        .clone()
}

// ---------------------------------------------------------------------------------------
// The two execution tiers
// ---------------------------------------------------------------------------------------

/// The engine's execution tier, pinned per probe through the child's own environment.
///
/// `BEAM_RT_TIER` is read by the runtime at start-up (`src/runner/tier.rs`); the baseline tier
/// is the default, and the variable is **removed** for it so an inherited value cannot silently
/// turn the "default tier" probe into an interpreter probe.
#[derive(Clone, Copy)]
enum Tier {
    /// The native baseline tier (`JitMode::Auto`), the runtime's default.
    Baseline,
    /// The direct-threaded interpreter (`BEAM_RT_TIER=interpreter`).
    Interpreter,
}

impl Tier {
    /// Both tiers, in the order every test asserts them.
    const ALL: [Tier; 2] = [Tier::Baseline, Tier::Interpreter];

    /// The tier's label for a failure message.
    fn label(self) -> &'static str {
        match self {
            Tier::Baseline => "default (native baseline) tier",
            Tier::Interpreter => "interpreter tier",
        }
    }

    /// The environment pin this tier needs (an explicit `None` **removes** the variable).
    fn env(self) -> (&'static str, Option<&'static str>) {
        match self {
            Tier::Baseline => ("BEAM_RT_TIER", None),
            Tier::Interpreter => ("BEAM_RT_TIER", Some("interpreter")),
        }
    }
}

// ---------------------------------------------------------------------------------------
// The CLI driver
// ---------------------------------------------------------------------------------------

/// One finished `beam_rt` invocation: its exit code and captured, trimmed output.
struct Cli {
    /// The process exit code (`-1` when killed by a signal, which no probe here does).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// Runs the built `beam_rt` binary with `args` and `env`, bounded
/// (`support::run_bounded_with_env`), so a probe that blocks cannot hang the suite.
///
/// `ERL_CRASH_DUMP_SECONDS=0` keeps a failing probe from littering the working directory.
fn beam_rt(args: &[&str], tier: Tier) -> Cli {
    let output = support::run_bounded_with_env(
        Path::new(BINARY),
        args,
        &[("ERL_CRASH_DUMP_SECONDS", Some("0")), tier.env()],
    )
    .unwrap_or_else(|| {
        panic!(
            "the built beam_rt binary must finish `{args:?}` on the {} within the probe bound",
            tier.label()
        )
    });
    Cli {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// This runtime's trimmed stdout for the `-eval` `program` on `tier`, which must succeed.
fn ours_eval(tier: Tier, program: &str) -> String {
    let cli = beam_rt(&["-noshell", "-eval", program], tier);
    assert_eq!(
        cli.code, 0,
        "the `-eval` probe must succeed on the {};\nprogram:\n{program}\nstderr:\n{}",
        tier.label(),
        cli.err
    );
    assert!(
        cli.err.is_empty(),
        "the `-eval` probe must not write to stderr on the {}: {}",
        tier.label(),
        cli.err
    );
    cli.out
}

/// This runtime's trimmed stdout for `module:function/0` through the scheduled `run` route on
/// `tier`, which must succeed.
fn ours_run(tier: Tier, fixture: &Fixture, function: &str) -> String {
    let spec = format!("{FIXTURE_MODULE}:{function}/0");
    let cli = beam_rt(&["run", fixture.beam_str(), &spec], tier);
    assert_eq!(
        cli.code, 0,
        "`beam_rt run … {spec}` must succeed on the {}; stderr:\n{}",
        tier.label(),
        cli.err
    );
    assert!(
        cli.err.is_empty(),
        "the `run` route must not write to stderr for {spec} on the {}: {}",
        tier.label(),
        cli.err
    );
    cli.out
}

/// The oracle's trimmed stdout for the same `-eval` `program`, or `None` — loudly — when the
/// probe could not answer.
fn theirs_eval(oracle: &Path, program: &str) -> Option<String> {
    match support::oracle_eval(oracle, None, program) {
        Some(answer) => Some(answer),
        None => {
            eprintln!("warning: the oracle could not answer the probe {program:?}");
            None
        }
    }
}

/// The oracle's trimmed stdout for `zlib_stream:<function>()`, asked through `-pa <scratch>`
/// (a directory holding **only** the fixture module, which is what makes it a safe single
/// `-pa` — `tests/fixtures/` as a whole is never put on the oracle's code path).
fn theirs_run(oracle: &Path, fixture: &Fixture, function: &str) -> Option<String> {
    let program = format!("io:format(\"~w~n\",[{FIXTURE_MODULE}:{function}()])");
    match support::oracle_eval(oracle, Some(fixture.dir()), &program) {
        Some(answer) => Some(answer),
        None => {
            eprintln!("warning: the oracle could not answer {FIXTURE_MODULE}:{function}/0");
            None
        }
    }
}

// ---------------------------------------------------------------------------------------
// The compiled fixture
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory.
///
/// The directory holds **only** `zlib_stream.erl`/`.beam`, so it is a safe single `-pa` for the
/// oracle. The suite hands the compiled fixture out of a `OnceLock`, so it lives for the whole
/// test binary (one compilation, shared by the five `run`-route tests) and the small scratch
/// directory is left for the OS temp area to reclaim — [`Fixture::drop`] removes it on the
/// paths where a caller owns one.
struct Fixture {
    /// The scratch directory holding the fixture's `.erl` and `.beam`.
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

    /// The scratch directory, for the oracle's `-pa`.
    fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}

/// The compiler beside the resolved oracle first (the same installation), then on `$PATH`.
///
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists
/// (`tests/support/mod.rs`).
fn resolve_erlc() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = support::resolve_oracle()
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .into_iter()
        .collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path).filter(|dir| !dir.as_os_str().is_empty()));
    }
    dirs.into_iter()
        .map(|dir| dir.join("erlc"))
        .find(|candidate| support::is_executable(candidate))
}

/// The fixture, compiled **once** per test binary.
///
/// A `OnceLock` keeps the compilation single even though cargo runs the tests of one binary in
/// parallel threads; `None` means the run-route half of the suite skipped loudly and the caller
/// returns.
fn fixture() -> Option<&'static Fixture> {
    static FIXTURE: OnceLock<Option<Fixture>> = OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let Some(erlc) = resolve_erlc() else {
                support::skip_loudly(
                    BANNER,
                    "no `erlc` on $PATH (and none beside $ERTS_ORACLE), so the `run` route's \
                     fixture cannot be compiled",
                );
                return None;
            };
            build_fixture(&erlc)
        })
        .as_ref()
}

/// Copies [`FIXTURE_ERL`] into a fresh scratch directory and compiles it there.
///
/// `None` — after a loud skip — when the directory cannot be prepared, `erlc` cannot be run,
/// or `erlc` produced no `.beam`.
fn build_fixture(erlc: &Path) -> Option<Fixture> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_zlib_stream_{}_{n}",
        std::process::id()
    ));
    let source = dir.join(format!("{FIXTURE_MODULE}.erl"));
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| {
        std::fs::copy(FIXTURE_ERL, &source).map(|_copied| ())
    }) {
        support::skip_loudly(
            BANNER,
            &format!("cannot prepare the fixture in {}: {error}", dir.display()),
        );
        return None;
    }
    // `erlc` runs with the scratch directory as cwd and the bare file name, so nothing outside
    // it can be picked up and the module stays self-contained.
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg(
            source
                .file_name()
                .unwrap_or_else(|| panic!("the fixture source has no name")),
        )
        .current_dir(&dir)
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(BANNER, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            BANNER,
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join(format!("{FIXTURE_MODULE}.beam"));
    if !beam.is_file() {
        support::skip_loudly(BANNER, &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// 1. `deflateParams/3`'s stash path
// ---------------------------------------------------------------------------------------

/// The `-eval` probe: a level-0 stream, its `[2]`-byte zlib header, `deflateParams/3`'s stashed
/// sync flush recovered at the head of the next `deflate/3`, and the 36-byte round trip.
///
/// The two leaf sizes are taken through `lists:flatten/1` because the recovered stash leaf is
/// itself a **list** (`[[<<…26 bytes…>>], <<115,2,2,…>>]` — `zlib.erl`'s `append_iolist/2`
/// prepends the restored progress as one element), so `byte_size/1` on the unflattened head
/// would be `badarg` under both runtimes.
const E1_PROGRAM: &str = r#"
Z = zlib:open(),
ok = zlib:deflateInit(Z, 0, deflated, 15, 8, default),
D1 = zlib:deflate(Z, binary:copy(<<"A">>,16), none),
ok = zlib:deflateParams(Z, 9, default),
D2 = zlib:deflate(Z, <<"BBBB">>, finish),
Bin = iolist_to_binary([D1|D2]),
Round = zlib:uncompress(Bin),
io:format("~w~n",[{[byte_size(X1) || X1 <- lists:flatten(D1)], [byte_size(X2) || X2 <- lists:flatten(D2)], byte_size(Bin), Round =:= <<"AAAAAAAAAAAAAAAABBBB">>, byte_size(Round)}]),
io:format("hex=~s~n",[binary:encode_hex(Bin)]),
ok = zlib:deflateEnd(Z), zlib:close(Z), halt(0)"#;

/// The oracle's answer to [`E1_PROGRAM`]: the header leaf `[2]`, the recovered leaves
/// `[26, 8]`, `36` bytes in total, the round trip, and the exact 36 compressed bytes
/// (`7801` zlib header, a stored block carrying the 16 `A`s, the sync flush's empty stored
/// block, then the level-9 final block of `BBBB`).
const E1_GOLDEN: &str = "{[2],[26,8],36,true,20}\n\
hex=7801001000EFFF41414141414141414141414141414141000000FFFF7302020035700519";

/// The `run`-route golden for `zlib_stream:params_stash/0`: the same values as [`E1_GOLDEN`],
/// plus `deflateParams/3`'s own `ok` and with the compressed bytes as an integer list.
const R1_GOLDEN: &str = "{ok,[2],[26,8],36,[55,56,48,49,48,48,49,48,48,48,69,70,70,70,52,49,\
52,49,52,49,52,49,52,49,52,49,52,49,52,49,52,49,52,49,52,49,52,49,\
52,49,52,49,52,49,52,49,48,48,48,48,48,48,70,70,70,70,55,51,48,50,\
48,50,48,48,51,53,55,48,48,53,49,57],true}";

/// `deflateParams/3`'s stash path, on the `-eval` route and **both** execution tiers.
#[test]
fn the_deflate_params_stash_path_agrees_on_the_eval_route() {
    let Some(oracle) = oracle() else { return };
    for tier in Tier::ALL {
        let ours = ours_eval(tier, E1_PROGRAM);
        assert_eq!(
            ours, E1_GOLDEN,
            "`deflateParams/3`'s stashed sync flush must reappear at the head of the next \
             `deflate/3` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs_eval(&oracle, E1_PROGRAM) {
            assert_eq!(
                theirs, E1_GOLDEN,
                "the oracle's own answer on the {}",
                tier.label()
            );
        }
    }
}

/// The same case through the scheduled `run` route (`zlib_stream:params_stash/0`, a compiled
/// fixture run by the real scheduler), on **both** execution tiers.
#[test]
fn the_deflate_params_stash_path_agrees_on_the_run_route() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = fixture() else { return };
    let theirs = theirs_run(&oracle, fixture, "params_stash");
    for tier in Tier::ALL {
        let ours = ours_run(tier, fixture, "params_stash");
        assert_eq!(
            ours, R1_GOLDEN,
            "`zlib_stream:params_stash/0` through `beam_rt run` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs.as_deref() {
            assert_eq!(
                theirs, R1_GOLDEN,
                "the oracle running the same fixture function ({})",
                tier.label()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// 2. The `need_dictionary` protocol
// ---------------------------------------------------------------------------------------

/// The `-eval` probe: the Adler digest, the exact stream, the raising `inflate/2`, the
/// non-raising `inflate/3` and `safeInflate/2`, the recovery, and the need's continuity (a
/// fresh handle and a reset handle both raise it again).
const E2_PROGRAM: &str = r#"
Dict = <<"a dictionary with repeated words words words and more words">>,
P = <<"words words words and more words words">>,
Z = zlib:open(), ok = zlib:deflateInit(Z),
Adler = zlib:deflateSetDictionary(Z, Dict),
S = iolist_to_binary(zlib:deflate(Z, P, finish)),
ok = zlib:deflateEnd(Z), zlib:close(Z),
I = zlib:open(), ok = zlib:inflateInit(I),
Raise = try zlib:inflate(I, S) of V1 -> {value, iolist_size(V1)} catch error:R1 -> {error, R1} end,
Again = try zlib:inflate(I, <<>>) of V2 -> {value, iolist_size(V2)} catch error:R2 -> {error, R2} end,
ok = zlib:inflateSetDictionary(I, Dict),
Recovered = iolist_to_binary(zlib:inflate(I, <<>>)) =:= P,
I2 = zlib:open(), ok = zlib:inflateInit(I2),
Opt = zlib:inflate(I2, S, [{exception_on_need_dict, false}]),
OptSet = zlib:inflateSetDictionary(I2, Dict),
OptRecovered = iolist_to_binary(zlib:inflate(I2, <<>>)) =:= P,
I3 = zlib:open(), ok = zlib:inflateInit(I3),
Safe = zlib:safeInflate(I3, S),
Fresh = zlib:open(), ok = zlib:inflateInit(Fresh),
FreshNeed = try zlib:inflate(Fresh, S) of V3 -> {value, iolist_size(V3)} catch error:R3 -> {error, R3} end,
ok = zlib:inflateReset(I),
AfterReset = try zlib:inflate(I, S) of V4 -> {value, iolist_size(V4)} catch error:R4 -> {error, R4} end,
io:format("~w~n",[{Adler, byte_size(S), binary_to_list(binary:encode_hex(S)), byte_size(Dict), Raise, Again, Recovered, Opt, OptSet, OptRecovered, Safe, FreshNeed, AfterReset}]),
io:format("hex=~s~n",[binary:encode_hex(S)]),
halt(0)"#;

/// The `need_dictionary` tuple both routes answer, shared by [`E2_PROGRAM`] and
/// `zlib_stream:dict_protocol/0`.
///
/// Read in order: the Adler-32 `2662864480`; the 15-byte stream; its hex bytes
/// (`78BB9EB8166023240F21011D420E92`, decoded from the list); the 59-byte dictionary; the
/// raising `inflate/2`; the second ``inflate(I, <<>>)``'s `data_error`; the recovered payload;
/// the non-raising `inflate/3` answer; the `inflateSetDictionary/2` `ok` and the recovery it
/// enables; the `safeInflate/2` answer; and the need re-raised by a fresh handle and by a reset
/// one.
const DICTIONARY_GOLDEN: &str = "{2662864480,15,[55,56,66,66,57,69,66,56,49,54,54,48,50,51,\
50,52,48,70,50,49,48,49,49,68,52,50,48,69,57,50],59,{error,{need_dictionary,2662864480}},\
{error,data_error},true,{need_dictionary,2662864480,[]},ok,true,{need_dictionary,2662864480,\
[]},{error,{need_dictionary,2662864480}},{error,{need_dictionary,2662864480}}}";

/// The hex line [`E2_PROGRAM`] prints for the 15-byte dictionary stream.
const E2_HEX_GOLDEN: &str = "hex=78BB9EB8166023240F21011D420E92";

/// The `need_dictionary` protocol, on the `-eval` route and **both** execution tiers.
#[test]
fn the_need_dictionary_protocol_agrees_on_the_eval_route() {
    let Some(oracle) = oracle() else { return };
    let golden = format!("{DICTIONARY_GOLDEN}\n{E2_HEX_GOLDEN}");
    for tier in Tier::ALL {
        let ours = ours_eval(tier, E2_PROGRAM);
        assert_eq!(
            ours, golden,
            "the `need_dictionary` protocol — the digest, the 15-byte stream, the raising and \
             the two non-raising routes, the recovery and the per-handle continuity ({})",
            tier.label()
        );
        if let Some(theirs) = theirs_eval(&oracle, E2_PROGRAM) {
            assert_eq!(
                theirs, golden,
                "the oracle's own answer on the {}",
                tier.label()
            );
        }
    }
}

/// The same case through the scheduled `run` route (`zlib_stream:dict_protocol/0`), on
/// **both** execution tiers.
#[test]
fn the_need_dictionary_protocol_agrees_on_the_run_route() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = fixture() else { return };
    let theirs = theirs_run(&oracle, fixture, "dict_protocol");
    for tier in Tier::ALL {
        let ours = ours_run(tier, fixture, "dict_protocol");
        assert_eq!(
            ours, DICTIONARY_GOLDEN,
            "`zlib_stream:dict_protocol/0` through `beam_rt run` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs.as_deref() {
            assert_eq!(
                theirs, DICTIONARY_GOLDEN,
                "the oracle running the same fixture function ({})",
                tier.label()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// 3. `safeInflate/2` chunking
// ---------------------------------------------------------------------------------------

/// The `-eval` probe: `safeInflate/2` over a 100 000-byte payload, called eight times.
const E3_PROGRAM: &str = r#"
P = binary:copy(<<"0123456789">>, 10000),
C = zlib:compress(P),
Z = zlib:open(), ok = zlib:inflateInit(Z),
Rs = [zlib:safeInflate(Z, C), zlib:safeInflate(Z, <<>>), zlib:safeInflate(Z, <<>>), zlib:safeInflate(Z, <<>>), zlib:safeInflate(Z, <<>>), zlib:safeInflate(Z, <<>>), zlib:safeInflate(Z, <<>>), zlib:safeInflate(Z, <<>>)],
Shape = [{T, [byte_size(X) || X <- O]} || {T, O} <- Rs],
Total = lists:sum([iolist_size(O) || {_T, O} <- Rs]),
io:format("~w~n",[{Shape, Total, byte_size(P), byte_size(C)}]),
ok = zlib:inflateEnd(Z), zlib:close(Z), halt(0)"#;

/// The chunking golden, shared by [`E3_PROGRAM`] and `zlib_stream:safe_inflate_chunks/0`:
/// six full `?INFLATE_OUT_CHUNKSIZE` (16 384-byte) chunks, the 1 696-byte remainder, then a
/// call that reports `finished` with nothing left (`6 × 16 384 + 1 696 = 100 000`), the two
/// payload sizes, and the 228-byte compressed stream.
const CHUNK_GOLDEN: &str =
    "{[{continue,[16384]},{continue,[16384]},{continue,[16384]},{continue,[16384]},\
{continue,[16384]},{continue,[16384]},{finished,[1696]},{finished,[]}],100000,100000,228}";

/// `safeInflate/2`'s chunking, on the `-eval` route and **both** execution tiers.
#[test]
fn safe_inflate_chunking_agrees_on_the_eval_route() {
    let Some(oracle) = oracle() else { return };
    for tier in Tier::ALL {
        let ours = ours_eval(tier, E3_PROGRAM);
        assert_eq!(
            ours, CHUNK_GOLDEN,
            "`safeInflate/2` must return six full output chunks, the remainder, and then an \
             empty `finished` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs_eval(&oracle, E3_PROGRAM) {
            assert_eq!(
                theirs, CHUNK_GOLDEN,
                "the oracle's own answer on the {}",
                tier.label()
            );
        }
    }
}

/// The same case through the scheduled `run` route
/// (`zlib_stream:safe_inflate_chunks/0`), on **both** execution tiers.
#[test]
fn safe_inflate_chunking_agrees_on_the_run_route() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = fixture() else { return };
    let theirs = theirs_run(&oracle, fixture, "safe_inflate_chunks");
    for tier in Tier::ALL {
        let ours = ours_run(tier, fixture, "safe_inflate_chunks");
        assert_eq!(
            ours, CHUNK_GOLDEN,
            "`zlib_stream:safe_inflate_chunks/0` through `beam_rt run` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs.as_deref() {
            assert_eq!(
                theirs, CHUNK_GOLDEN,
                "the oracle running the same fixture function ({})",
                tier.label()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// 4. `inflateReset/1`, `inflateGetDictionary/1` and `deflateReset/1`
// ---------------------------------------------------------------------------------------

/// The `-eval` probe for the reset and dictionary accessors.
const E4_PROGRAM: &str = r#"
I0 = zlib:open(), ok = zlib:inflateInit(I0),
FreshDict = zlib:inflateGetDictionary(I0),
Payload = binary:copy(<<"abcdefgh">>, 1000),
Good = zlib:compress(Payload),
Half = binary:part(Good, 0, 10),
I1 = zlib:open(), ok = zlib:inflateInit(I1),
Part = zlib:inflate(I1, Half),
ResetOk = zlib:inflateReset(I1),
AfterReset = zlib:inflate(I1, Good),
Full = iolist_to_binary(AfterReset) =:= Payload,
Dict = <<"a dictionary with repeated words words words and more words">>,
P2 = <<"words words words and more words words">>,
Z = zlib:open(), ok = zlib:deflateInit(Z),
Adler = zlib:deflateSetDictionary(Z, Dict),
S = iolist_to_binary(zlib:deflate(Z, P2, finish)),
ok = zlib:deflateEnd(Z), zlib:close(Z),
I2 = zlib:open(), ok = zlib:inflateInit(I2),
_ = zlib:inflate(I2, S, [{exception_on_need_dict, false}]),
ok = zlib:inflateSetDictionary(I2, Dict),
DictGot = zlib:inflateGetDictionary(I2),
Recovered = iolist_to_binary(zlib:inflate(I2, <<>>)) =:= P2,
DictAfter = zlib:inflateGetDictionary(I2),
D = zlib:open(), ok = zlib:deflateInit(D),
_ = zlib:deflate(D, <<"junk">>, finish),
DReset = zlib:deflateReset(D),
DOut = iolist_to_binary(zlib:deflate(D, <<"fresh">>, finish)),
ok = zlib:deflateEnd(D), zlib:close(D),
io:format("~w~n",[{byte_size(FreshDict), iolist_size(Part), ResetOk, iolist_size(AfterReset), Full, Adler, byte_size(DictGot), DictGot =:= Dict, Recovered, byte_size(DictAfter), DictAfter =:= Dict, DReset, byte_size(DOut)}]),
io:format("hex=~s~n",[binary:encode_hex(DOut)]),
halt(0)"#;

/// The reset/accessor golden shared by [`E4_PROGRAM`] and (minus the printed size, which the
/// fixture expresses as the hex list) `zlib_stream:reset_and_dictionary/0`.
///
/// Read in order: the fresh dictionary is **empty** (0 bytes); the 10-byte prefix of a valid
/// stream yields no output; `inflateReset/1` answers `ok`; the whole stream then inflates to
/// 8 000 bytes byte-identically; the dictionary-fed stream's Adler-32 and the 59-byte fetched
/// dictionary that `=:=` the fed one (before **and** after the payload was recovered); the
/// recovery itself; `deflateReset/1`'s `ok`; and the 13-byte fresh stream a finished deflate
/// handle restarts.
const E4_GOLDEN: &str = "{0,0,ok,8000,true,2662864480,59,true,true,59,true,ok,13}\n\
hex=789C4B2B4A2DCE000006480219";

/// The `run`-route golden for `zlib_stream:reset_and_dictionary/0` — the same values, with the
/// 13-byte stream as an integer list instead of a `~s` line.
const R4_GOLDEN: &str = "{0,0,ok,8000,true,2662864480,59,true,true,59,true,ok,[55,56,57,67,52,\
66,50,66,52,65,50,68,67,69,48,48,48,48,48,54,52,56,48,50,49,57]}";

/// The reset and dictionary-accessor surface, on the `-eval` route and **both** tiers.
#[test]
fn the_reset_and_dictionary_accessors_agree_on_the_eval_route() {
    let Some(oracle) = oracle() else { return };
    for tier in Tier::ALL {
        let ours = ours_eval(tier, E4_PROGRAM);
        assert_eq!(
            ours, E4_GOLDEN,
            "`inflateReset/1` mid-stream, `inflateGetDictionary/1` on an empty and on a \
             dictionary-fed stream, and `deflateReset/1` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs_eval(&oracle, E4_PROGRAM) {
            assert_eq!(
                theirs, E4_GOLDEN,
                "the oracle's own answer on the {}",
                tier.label()
            );
        }
    }
}

/// The same case through the scheduled `run` route
/// (`zlib_stream:reset_and_dictionary/0`), on **both** execution tiers.
#[test]
fn the_reset_and_dictionary_accessors_agree_on_the_run_route() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = fixture() else { return };
    let theirs = theirs_run(&oracle, fixture, "reset_and_dictionary");
    for tier in Tier::ALL {
        let ours = ours_run(tier, fixture, "reset_and_dictionary");
        assert_eq!(
            ours, R4_GOLDEN,
            "`zlib_stream:reset_and_dictionary/0` through `beam_rt run` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs.as_deref() {
            assert_eq!(
                theirs, R4_GOLDEN,
                "the oracle running the same fixture function ({})",
                tier.label()
            );
        }
    }
}

// ---------------------------------------------------------------------------------------
// 5. A truncated stream is only detected by `inflateEnd/1`
// ---------------------------------------------------------------------------------------

/// The `-eval` probe: a valid 31-byte stream with the last 4 bytes (the Adler-32 trailer)
/// dropped, its whole-stream control, and the `safeInflate/2` view of the same prefix.
const E5_PROGRAM: &str = r#"
P = binary:copy(<<"hello world ">>, 100),
Good = zlib:compress(P),
Trunc = binary:part(Good, 0, byte_size(Good) - 4),
I = zlib:open(), ok = zlib:inflateInit(I),
R = try zlib:inflate(I, Trunc) of V1 -> {value, iolist_size(V1), iolist_to_binary(V1) =:= P} catch error:E1 -> {error, E1} end,
EndTrunc = try zlib:inflateEnd(I) of V2 -> {value, V2} catch error:E2 -> {error, E2} end,
ok = zlib:close(I),
I2 = zlib:open(), ok = zlib:inflateInit(I2),
R2 = try zlib:inflate(I2, Good) of V3 -> {value, iolist_size(V3), iolist_to_binary(V3) =:= P} catch error:E3 -> {error, E3} end,
EndWhole = try zlib:inflateEnd(I2) of V4 -> {value, V4} catch error:E4 -> {error, E4} end,
ok = zlib:close(I2),
I3 = zlib:open(), ok = zlib:inflateInit(I3),
S = zlib:safeInflate(I3, Trunc),
S2 = zlib:safeInflate(I3, <<>>),
EndSafe = try zlib:inflateEnd(I3) of V5 -> {value, V5} catch error:E5 -> {error, E5} end,
ok = zlib:close(I3),
Shape = fun({Tag, O}) -> {Tag, iolist_size(O)} end,
io:format("~w~n",[{byte_size(Good), byte_size(Trunc), R, EndTrunc, R2, EndWhole, Shape(S), Shape(S2), EndSafe}]),
halt(0)"#;

/// The truncated-stream golden, shared by [`E5_PROGRAM`] and
/// `zlib_stream:truncated_stream/0`.
///
/// The truncated `inflate/2` **returns the whole 1 200-byte payload** — it does not raise; the
/// control (the whole stream) inflates identically and its `inflateEnd/1` answers `ok`; the
/// truncated stream's `inflateEnd/1` is the only place the damage surfaces, as `error:data_error`;
/// and `safeInflate/2` reports `{finished, 1200}` over the prefix while its own
/// `inflateEnd/1` raises the same `data_error`.
const TRUNCATED_GOLDEN: &str = "{31,27,{value,1200,true},{error,data_error},{value,1200,true},\
{value,ok},{finished,1200},{finished,0},{error,data_error}}";

/// The truncated-stream case, on the `-eval` route and **both** execution tiers.
#[test]
fn a_truncated_stream_is_only_detected_by_inflate_end_on_the_eval_route() {
    let Some(oracle) = oracle() else { return };
    for tier in Tier::ALL {
        let ours = ours_eval(tier, E5_PROGRAM);
        assert_eq!(
            ours, TRUNCATED_GOLDEN,
            "a stream missing its Adler trailer must inflate without raising and only fail at \
             `inflateEnd/1` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs_eval(&oracle, E5_PROGRAM) {
            assert_eq!(
                theirs, TRUNCATED_GOLDEN,
                "the oracle's own answer on the {}",
                tier.label()
            );
        }
    }
}

/// The same case through the scheduled `run` route
/// (`zlib_stream:truncated_stream/0`), on **both** execution tiers.
#[test]
fn a_truncated_stream_is_only_detected_by_inflate_end_on_the_run_route() {
    let Some(oracle) = oracle() else { return };
    let Some(fixture) = fixture() else { return };
    let theirs = theirs_run(&oracle, fixture, "truncated_stream");
    for tier in Tier::ALL {
        let ours = ours_run(tier, fixture, "truncated_stream");
        assert_eq!(
            ours, TRUNCATED_GOLDEN,
            "`zlib_stream:truncated_stream/0` through `beam_rt run` ({})",
            tier.label()
        );
        if let Some(theirs) = theirs.as_deref() {
            assert_eq!(
                theirs, TRUNCATED_GOLDEN,
                "the oracle running the same fixture function ({})",
                tier.label()
            );
        }
    }
}
