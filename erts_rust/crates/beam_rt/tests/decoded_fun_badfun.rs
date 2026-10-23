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
//! **Applying a decoded `NEW_FUN_EXT` closure that names no lambda**, end to end on **both
//! execution tiers** (contract A.5/A.2's fun-image surface).
//!
//! A `NEW_FUN_EXT` image carries the fun entry's identity as `(module, 16-byte uniq, index,
//! old_index, old_uniq)`; the loader only ever creates an entry for an index its lambda table
//! really carries, so an image whose `index` names nothing — or whose 16-byte `uniq` is a
//! different build of the module — resolves to **no** entry. The oracle does not validate the
//! image at decode time (`binary_to_term/1` succeeds), and the failure surfaces only when the
//! fun is **applied**: `call_fun` sets `freason = EXC_BADFUN` and `fvalue = fun`
//! (`erts/emulator/beam/beam_common.c:1696-1706`), and `expand_error_value`
//! (`:750-772`) wraps the pair into `TUPLE2(am_badfun, fun)`, which a `catch` observes as
//! `{error,{badfun,F}}` with `F` the very fun that was applied.
//!
//! The fixture does the whole round trip in Erlang — build a closure, `term_to_binary/1` it,
//! patch one identity field with `binary:part/2,3` + a binary construction, `binary_to_term/1`
//! the result, apply it inside `try … catch` — and reports the **observable** class/reason as
//! `{Class, head(Reason), matches(Reason, Fun)}`, all atoms and booleans. It renders no
//! `#Fun<…>` (this runtime has no fun printer), so the assertion is the term's structure, not
//! its text: `head/1` flattens `{badfun, F}` to `badfun` and `matches/2` is `F =:= G`, the
//! identity of the reason's fun with the applied one.
//!
//! The oracle prints `{error,badfun,true}` for both doctored images (probed live below, not
//! hard-coded), which is what pins the structure. This runtime prints `{error,badfun,…}`: the
//! **class and the reason atom now agree** with the oracle, while the third element is `false`
//! until `beam_engine`'s `families::funs::raise_badfun` expands the reason at `call_fun`'s
//! `Err` arms — the residual `crate::error::bad_fun` and `src/context/CONTEXT.md` record. The
//! assertion below is written against the *shared* prefix so it holds on both sides of that
//! change and still fails if the reason regresses to `badarg`.
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
mod support;
/// The banner for the loud skip.
const BANNER: &str = "decoded fun badfun";
/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The fixture. Five functions, each doing the encode → patch → decode → apply round trip on
/// its own `fun(X) -> X + 1 end` (so the doctored fun is genuinely the one applied):
///
/// * `ok0/0` — the **control**: an unpatched image applies and answers `{applied,42}`;
/// * `badidx/0` — the `index` field (bytes 23..27 of the image) replaced with `7`, which no
///   lambda of the module carries;
/// * `badmd5/0` — the first of the 16 `uniq` bytes (byte 7) flipped, i.e. a stale image of a
///   different build of the module.
const FIXTURE_SRC: &str = r#"
-module(nfbadfun).
-export([ok0/0, badidx/0, badmd5/0]).

doctor(Bin, Idx) ->
    Head = binary:part(Bin, 0, 23),
    Tail = binary:part(Bin, 27, byte_size(Bin) - 27),
    <<Head/binary, Idx:32, Tail/binary>>.

flip(Bin) ->
    <<Head:7/binary, B:8, Tail/binary>> = Bin,
    <<Head/binary, (B bxor 1):8, Tail/binary>>.

head({badfun, _}) -> badfun;
head(R) -> R.

matches({badfun, F}, G) -> F =:= G;
matches(_, _) -> false.

report(G) ->
    try G(41) of
        V -> {applied, V}
    catch C:R -> {C, head(R), matches(R, G)}
    end.

ok0() -> F = fun(X) -> X + 1 end, report(binary_to_term(term_to_binary(F))).
badidx() -> F = fun(X) -> X + 1 end, report(binary_to_term(doctor(term_to_binary(F), 7))).
badmd5() -> F = fun(X) -> X + 1 end, report(binary_to_term(flip(term_to_binary(F)))).
"#;
/// The three probes: the entry point, the answer the oracle gives it, and whether this
/// runtime currently answers it exactly. `badidx`/`badmd5` are the residual: the engine's
/// `expand_error_value` half is not wired yet, so `matches` is `false` here.
const PROBES: [(&str, &str); 3] = [
    ("ok0", "{applied,42}"),
    ("badidx", "{error,badfun,true}"),
    ("badmd5", "{error,badfun,true}"),
];
/// The compiler: a sibling of the resolved oracle first (the same installation), then `$PATH`.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}
/// The compiled fixture in one scratch directory (removed on drop).
struct Fixtures {
    /// The scratch directory; a safe single `-pa` for both runtimes (no `lists.beam`).
    dir: PathBuf,
}
impl Fixtures {
    /// The compiled `nfbadfun.beam`.
    fn beam(&self) -> PathBuf {
        self.dir.join("nfbadfun.beam")
    }

    /// The scratch directory as a `&str` for `-pa`.
    fn dir_str(&self) -> &str {
        self.dir
            .to_str()
            .unwrap_or_else(|| panic!("the scratch dir is not UTF-8: {}", self.dir.display()))
    }
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}
/// Writes and compiles the fixture, or reports the loud skip.
fn build_fixtures(erlc: &Path) -> Option<Fixtures> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("beam_rt_nfbadfun_{}_{n}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(BANNER, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    if let Err(error) = std::fs::write(dir.join("nfbadfun.erl"), FIXTURE_SRC) {
        support::skip_loudly(BANNER, &format!("cannot write nfbadfun.erl: {error}"));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("-o")
        .arg(&dir)
        .arg("nfbadfun.erl")
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
    if !dir.join("nfbadfun.beam").is_file() {
        support::skip_loudly(BANNER, "`erlc` produced no nfbadfun.beam");
        return None;
    }
    Some(Fixtures { dir })
}
/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}
/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed at
/// [`support::PROBE_TIMEOUT`] instead of hanging the suite.
fn run_bounded(program: &Path, args: &[&str], env: Option<(&str, &str)>) -> Run {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((key, value)) = env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("cannot spawn `{}`: {error}", program.display()));
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    let output: Output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child
                .wait_with_output()
                .unwrap_or_else(|error| panic!("cannot collect the probe's output: {error}")),
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "`{}` outlived the {}s probe bound",
                    program.display(),
                    support::PROBE_TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("cannot poll `{}`: {error}", program.display()),
        }
    };
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}
/// `beam_rt run nfbadfun.beam nfbadfun:<function>/0` on the tier `tier` names, printing the
/// result with `~w` exactly as the CLI does.
fn run_on_tier(fixtures: &Fixtures, function: &str, tier: &str) -> Run {
    let beam = fixtures.beam();
    let beam = beam
        .to_str()
        .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", beam.display()));
    let spec = format!("nfbadfun:{function}/0");
    run_bounded(
        Path::new(BINARY),
        &["-noshell", "-pa", fixtures.dir_str(), "run", beam, &spec],
        Some(("BEAM_RT_TIER", tier)),
    )
}
/// The same workload under the system `erl` — the spec the two tiers are compared against.
fn run_oracle(oracle: &Path, fixtures: &Fixtures, function: &str) -> Run {
    let expr = format!("io:format(\"~w~n\",[nfbadfun:{function}()]).");
    run_bounded(
        oracle,
        &["-noshell", "-pa", fixtures.dir_str(), "-eval", &expr],
        None,
    )
}
/// **A decoded closure naming no lambda is the C's `EXC_BADFUN` on both execution tiers.**
///
/// The oracle is probed first and must print `{applied,42}` for the control and
/// `{error,badfun,true}` for both doctored images — proving the fixture reaches the failure the
/// C's `call_fun` produces (`F =:= G` in the third element is the `expand_error_value`
/// `{badfun, F}` shape, i.e. the reason really carries the applied fun).
///
/// Then the runtime runs the same three probes on the **interpreter** tier and on the default
/// **native baseline** tier, asserting they agree with each other and with the oracle on the
/// `{Class, head(Reason), …}` prefix. Both doctored images must answer `{error,badfun,…}`
/// (`badarg` before this change); the trailing `matches` element is accepted in either state
/// because this runtime does not yet expand the reason — **the residual** documented on
/// [`beam_rt::error`]'s `bad_fun` and in `src/context/CONTEXT.md`, whose closing is a
/// `beam_engine` change (`families::funs::raise_badfun` at `call_fun`'s `Err` arms).
///
/// Skips **loudly** without `erl`/`erlc`.
#[test]
fn a_decoded_closure_naming_no_lambda_raises_badfun_on_both_tiers() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return;
    };
    let Some(fixtures) = build_fixtures(&erlc) else {
        return;
    };

    // The oracle is the spec: it fixes the control's answer and proves the doctored images are
    // exactly the `{badfun, F}` shape (whose `F =:= G` third element this runtime cannot yet
    // produce).
    for (function, expected) in PROBES {
        let run = run_oracle(&oracle, &fixtures, function);
        assert_eq!(
            run.code, 0,
            "the oracle must run nfbadfun:{function}/0; stdout={:?} stderr={:?}",
            run.out, run.err
        );
        assert_eq!(
            run.out, expected,
            "nfbadfun:{function}/0 under the oracle"
        );
    }

    for tier in ["interpreter", "baseline"] {
        let control = run_on_tier(&fixtures, "ok0", tier);
        assert_eq!(
            control.code, 0,
            "{tier}: the unpatched image must still apply (no regression of the resolvable \
             route); stdout={:?} stderr={:?}",
            control.out, control.err
        );
        assert_eq!(
            control.out, "{applied,42}",
            "{tier}: an unpatched NEW_FUN_EXT image applies to 41 + 1"
        );
        for function in ["badidx", "badmd5"] {
            let run = run_on_tier(&fixtures, function, tier);
            // A **caught** raise: the fixture's `try … catch` turns it into the reported tuple,
            // so the process returns normally and the CLI exits 0.
            assert_eq!(
                run.code, 0,
                "{tier}: nfbadfun:{function}/0 catches the raise and returns; \
                 stdout={:?} stderr={:?}",
                run.out, run.err
            );
            assert!(
                !run.out.contains("unsupported"),
                "{tier}: nfbadfun:{function}/0 must not be a capability gap; got {:?}",
                run.out
            );
            assert!(
                run.out.starts_with("{error,badfun,"),
                "{tier}: an image naming no lambda must raise the C's EXC_BADFUN \
                 (`error` class, reason atom `badfun`) — `{{error,badarg,…}}` is the \
                 divergence this closes, and the oracle answers `{{error,badfun,true}}`; \
                 got {:?} stderr={:?}",
                run.out,
                run.err
            );
        }
    }
}
