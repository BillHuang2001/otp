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

//! End-to-end CLI tests for the `differential` binary.
//!
//! These drive the real binary as a child process, so the documented exit-code contract
//! (0 agreed / 1 mismatch / 2 usage-env-IO) and the `--help` surface are locked in, and
//! environment overrides (`$ERTS_ORACLE`, `$BEAM_RT`) can be injected without touching
//! this test process. Tests that need the oracle **skip loudly** when `erl` is missing.
//!
//! **Every test that drives a full `suite` holds [`common::corpus_run_lock`] for its whole
//! body.** Cargo runs this binary's tests on parallel threads, and two corpus cases
//! (`flags/sname_switch_value` `-sname mynode`, `flags/name_switch_value` `-name node@host`)
//! start distribution and register their node name with `epmd`; overlapping full-corpus runs
//! make the second `erl` lose that name race, so the *oracle* stops reproducing its golden
//! and the case is reported as a spurious `mismatch`. The lock serializes the corpus sweeps
//! so at most one `-sname`/`-name` `erl` runs at a time — see the lock's own doc comment in
//! `tests/common/mod.rs` for the full root cause. Do not remove it.

use std::path::PathBuf;
use std::process::Command;

mod common;

/// The observed result of one child-process invocation.
struct Invocation {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Invocation {
    fn describe(&self) -> String {
        format!(
            "exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

fn invoke(args: &[&str], envs: &[(&str, &str)]) -> Invocation {
    let mut command = Command::new(env!("CARGO_BIN_EXE_differential"));
    command.args(args).envs(envs.iter().copied());
    // Point the child at a **worktree-local** temp dir instead of the ambient `$TMPDIR`: the
    // child's own scratch (e.g. the `.beam` files `module_build` compiles for module-run cases)
    // would otherwise land in the shared/volatile `$TMPDIR` this task exists to keep test
    // fixtures out of (`common::scratch_root`). This makes the tests independent of `$TMPDIR`
    // entirely — they pass even when it is missing or read-only.
    command.env("TMPDIR", common::scratch_root());
    let output = command.output().expect("spawn the differential binary");
    Invocation {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A unique scratch output directory (removed on drop).
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        // Derived from the worktree-local scratch root, not the shared/volatile `$TMPDIR`
        // (see `common::scratch_root`); only the program path differs, the tag + pid + thread
        // id suffix keeps each scratch unique within the run.
        let dir = common::scratch_root().join(format!(
            "differential-cli-{}-{:?}-{tag}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Scratch(dir)
    }

    fn arg(&self) -> String {
        self.0.display().to_string()
    }

    fn report(&self) -> PathBuf {
        self.0.join("report.json")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn oracle_available() -> bool {
    differential::runtime::resolve_oracle().is_ok()
}

/// Whether the fresh Rust runtime (the one the tests inject as `$BEAM_RT`) is available.
///
/// This must mirror the path actually injected: left to its own default resolution the
/// child binary would prefer `target/release/beam_rt`, so a stale release build could
/// shadow the fresh debug build. [`common::fresh_rust_runtime`] is the sibling of this
/// crate's test binary — the artifact cargo just built.
fn rust_runtime_available() -> bool {
    common::fresh_rust_runtime().is_some()
}

/// The **workspace-default** Rust runtime the child binary resolves when the test injects
/// no `$BEAM_RT`: `<CARGO_MANIFEST_DIR>/../../target/{release,debug}/beam_rt` (release
/// preferred), exactly as [`differential::runtime::resolve_rust_from`] computes it.
///
/// A child invocation only gets the fresh debug build when one was produced; otherwise the
/// test injects nothing and the child falls back to this default, so this is what actually
/// decides whether agreement is observable in the release-only build state (a
/// `target/release/beam_rt` with no debug sibling). Resolution cannot fail here: with no
/// `$BEAM_RT` value the only error is an underivable target dir, and `$CARGO_MANIFEST_DIR`
/// is always a real crate directory.
fn workspace_default_runtime() -> Option<differential::runtime::RuntimeSpec> {
    differential::runtime::resolve_rust_from(None, std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("resolving the workspace default cannot fail without a $BEAM_RT value")
}

/// The `$BEAM_RT` injection for a child invocation that would otherwise rely on **default**
/// resolution: the freshly built debug runtime, or nothing when it was not built (the child
/// then reports the Rust side as `unsupported` — honest, never a fabricated pass).
fn fresh_beam_rt_env<'a>(fresh: &'a Option<PathBuf>) -> Vec<(&'static str, &'a str)> {
    fresh
        .as_ref()
        .map(|path| ("BEAM_RT", path.to_str().expect("utf-8 runtime path")))
        .into_iter()
        .collect()
}

/// The `$BEAM_DIST_PROBE` injection for a child invocation that would otherwise rely on
/// **default** resolution: the freshly built `dist_probe` helper, or nothing when it was not
/// built (the child then reports the `dist/*` command cases as `skipped` — honest, never a
/// fabricated pass). Mirrors [`fresh_beam_rt_env`].
fn fresh_helper_env<'a>(helper: &'a Option<PathBuf>) -> Vec<(&'static str, &'a str)> {
    helper
        .as_ref()
        .map(|path| ("BEAM_DIST_PROBE", path.to_str().expect("utf-8 helper path")))
        .into_iter()
        .collect()
}

/// Whether the module-run cases can be exercised (they need the system `erlc`).
fn erlc_available() -> bool {
    matches!(differential::module_build::resolve_erlc(), Ok(Some(_)))
}

/// Write an executable shell script into `scratch` (a fake `$BEAM_RT`) and return its path.
///
/// The script is written through an explicit [`std::fs::File`] that is **flushed,
/// `sync_all`'d and closed** (by falling out of scope) before this function returns —
/// i.e. before anything can `exec` it. A descriptor left open *for writing* makes the
/// kernel refuse to `exec` the file with `ETXTBSY` ("Text file busy", errno 26), which is
/// exactly the transient failure the child binary's bounded [`differential::runtime`]
/// retry exists for; closing it here, up front, removes the race at its source. (The
/// bounded retry in the child remains the safety net for a sibling `fork()` inheriting a
/// writer's descriptor.)
#[cfg(unix)]
fn fake_runtime(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let path = scratch.0.join(name);
    std::fs::create_dir_all(&scratch.0).expect("create scratch dir");

    {
        let mut file = std::fs::File::create(&path).expect("create fake runtime");
        file.write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .expect("write fake runtime");
        file.flush().expect("flush fake runtime");
        file.sync_all().expect("sync fake runtime");
        // `file` is dropped (closed) here, before the script can ever be executed.
    }

    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}
#[test]
fn help_is_usable_on_every_subcommand_and_exits_0() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["run", "--help"],
        vec!["compare", "--help"],
        vec!["suite", "--help"],
        vec!["list", "--help"],
    ] {
        let invocation = invoke(&args, &[]);
        assert_eq!(invocation.code, Some(0), "{args:?}: {}", invocation.describe());
        assert!(
            invocation.stdout.contains("USAGE") || invocation.stdout.contains("OPTIONS"),
            "{args:?}: {}",
            invocation.describe()
        );
        assert!(
            invocation.stdout.contains("-h, --help") || args == ["--help"] || args == ["-h"],
            "{args:?}: {}",
            invocation.describe()
        );
        assert!(invocation.stderr.is_empty(), "{args:?}: {}", invocation.describe());
    }
}

#[test]
fn usage_errors_exit_2() {
    for (args, expected) in [
        (vec!["frobnicate"], "unknown subcommand"),
        (vec!["run"], "'run' requires a <case-id>"),
        (vec!["run", "no/such-case"], "unknown case id"),
        (vec!["suite", "--nope"], "unknown option"),
        (vec!["list", "--out"], "requires a directory argument"),
    ] {
        let invocation = invoke(&args, &[]);
        assert_eq!(invocation.code, Some(2), "{args:?}: {}", invocation.describe());
        assert!(
            invocation.stderr.contains(expected),
            "{args:?}: expected {expected:?} in {}",
            invocation.describe()
        );
    }

    // No arguments at all: a usage error that still shows the root help.
    let invocation = invoke(&[], &[]);
    assert_eq!(invocation.code, Some(2));
    assert!(invocation.stderr.contains("missing subcommand"));
    assert!(invocation.stderr.contains("SUBCOMMANDS"));
}

#[test]
fn list_needs_no_runtime_at_all() {
    let scratch = Scratch::new("list");
    let invocation = invoke(
        &["list", "--out", &scratch.arg()],
        &[
            ("ERTS_ORACLE", "/nonexistent/erl"),
            ("BEAM_RT", "/nonexistent/beam_rt"),
        ],
    );
    assert_eq!(invocation.code, Some(0), "{}", invocation.describe());
    assert!(
        invocation
            .stdout
            .contains("etf/term_to_binary_tuple — A.2 — term_to_binary/1"),
        "{}",
        invocation.describe()
    );
    assert_eq!(
        invocation.stdout.lines().count(),
        differential::corpus::cases().len(),
        "one line per corpus case"
    );

    // The JSON listing is written next to the human one.
    let written = std::fs::read_to_string(scratch.report()).expect("report written");
    assert!(written.contains("\"schema\": \"differential/list/1\""));
    assert!(invocation.stderr.contains("wrote"));
}

#[test]
fn list_json_is_machine_readable() {
    let invocation = invoke(&["list", "--json", "--out", &Scratch::new("list-json").arg()], &[]);
    assert_eq!(invocation.code, Some(0), "{}", invocation.describe());
    let json: serde_json::Value =
        serde_json::from_str(&invocation.stdout).expect("the JSON listing parses");
    assert_eq!(json["schema"], "differential/list/1");
    assert_eq!(json["cases"][0]["contracts"][0]["id"], "A.2");
}

#[test]
fn a_bad_oracle_or_rust_runtime_is_a_loud_env_error() {
    let scratch = Scratch::new("bad-env");
    let bad_oracle = invoke(
        &["suite", "--out", &scratch.arg()],
        &[("ERTS_ORACLE", "/nonexistent/erl")],
    );
    assert_eq!(bad_oracle.code, Some(2), "{}", bad_oracle.describe());
    assert!(
        bad_oracle.stderr.contains("$ERTS_ORACLE"),
        "{}",
        bad_oracle.describe()
    );
    assert!(!scratch.report().exists(), "no report for a failed resolution");

    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    let bad_rust = invoke(
        &["suite", "--out", &scratch.arg()],
        &[("BEAM_RT", "/nonexistent/beam_rt")],
    );
    assert_eq!(bad_rust.code, Some(2), "{}", bad_rust.describe());
    assert!(bad_rust.stderr.contains("$BEAM_RT"), "{}", bad_rust.describe());
}

/// Case ids whose Rust-side result is a **known, documented runtime gap** that surfaces as a
/// genuine `mismatch` — a real divergence from the oracle that the rewrite has not implemented
/// yet, and that the runtime does *not* label via its own `unsupported` convention.
///
/// This list is **empty**: there are currently **zero** mismatches. A capability gap the
/// runtime signals honestly (its documented `unsupported` convention — exit `3` with a
/// `beam_rt: unsupported: …` stderr line, or a probe-caught `unsupported…` reason) is
/// classified `unsupported`, never a mismatch; e.g. the port/driver seam
/// (`erlang:open_port/2` not wired) now reports `unsupported` (exit `3`) and `gate.sh` maps it
/// to exit `3`.
///
/// This allowlist is the enclosing invariant around the suite test below: since it is empty,
/// **any** `mismatch` is unexpected and fails the test. It exists only as an escape hatch so a
/// newly-discovered, understood divergence can be recorded explicitly rather than silently
/// tolerated — it must never be widened to mask a genuine behavioral regression.
const KNOWN_RUNTIME_GAPS: &[&str] = &[];

/// The suite must report genuine module agreement (freshly compiled `.beam`s really execute
/// under both runtimes) while surfacing the remaining capability gaps honestly — never a
/// fabricated agreement, and never an unexpected `mismatch`.
///
/// Agreement is only observable when the child `differential` process actually has a Rust
/// runtime **and** the system `erlc` exists. "Has a runtime" is decided exactly as the child
/// decides it: the test injects the fresh debug build as `$BEAM_RT` when it exists, and
/// otherwise injects nothing, so the child falls back to its workspace default
/// (`target/{release,debug}/beam_rt`). Both are therefore consulted — a release-only build
/// (the mandated `build --release …` then `test` workflow) still yields agreement, which is
/// what the `agree == 0` branch guards against.
#[test]
fn the_suite_reports_module_agreement_and_expression_capability_gaps() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    // Serialize the full corpus sweep against the binary's other corpus-running tests: two
    // corpus cases register an `epmd` node name (`-sname`/`-name`), so overlapping sweeps
    // would make the oracle lose the name race and report a spurious mismatch. Held for the
    // whole body — see `common::corpus_run_lock`.
    let _corpus_run = common::corpus_run_lock();
    // The child's effective runtime: the freshly built debug `beam_rt` (injected as
    // `$BEAM_RT`) if one exists, else whatever its workspace-default resolution finds. This
    // mirrors `runtime::resolve_rust_from` so the gate can never disagree with the child.
    let fresh = common::fresh_rust_runtime();
    let helper = common::fresh_helper();
    let default_runtime = workspace_default_runtime();
    let have_erlc = erlc_available();
    let agreement_possible = (fresh.is_some() || default_runtime.is_some()) && have_erlc;
    if !agreement_possible {
        if !have_erlc {
            eprintln!("NOTE: system erlc unavailable — module agreement not exercised");
        } else if default_runtime.is_some() {
            eprintln!(
                "NOTE: fresh beam_rt absent, but the workspace default ({}) is present — the \
                 child falls back to it and agreement IS exercised",
                default_runtime
                    .as_ref()
                    .expect("checked to be Some")
                    .program
                    .display()
            );
        } else {
            eprintln!(
                "NOTE: no Rust runtime at all (no fresh build, no workspace default) — module \
                 agreement not exercised"
            );
        }
    }
    let scratch = Scratch::new("suite");
    // Inject the freshly built debug `beam_rt`: the workspace default would prefer a
    // `target/release/beam_rt`, and a stale release artifact must never shadow the fresh
    // build this test is meant to exercise (when no fresh build exists the injection is
    // empty and the child uses that very workspace default). The fresh `dist_probe` helper is
    // injected the same way so the `dist/*` command cases are exercised by the fresh build.
    let env: Vec<(&str, &str)> = fresh_beam_rt_env(&fresh)
        .into_iter()
        .chain(fresh_helper_env(&helper))
        .collect();

    let lenient = invoke(&["suite", "--json", "--out", &scratch.arg()], &env);
    let json: serde_json::Value =
        serde_json::from_str(&lenient.stdout).expect("the suite report parses");
    assert_eq!(json["schema"], "differential/1");
    assert_eq!(json["mode"], "suite");
    assert_eq!(json["oracle"]["available"], true);

    let mut agree = 0u64;
    let mut unsupported = 0u64;
    let mut skipped = 0u64;
    let mut mismatched: Vec<String> = Vec::new();
    for case in json["cases"].as_array().expect("cases") {
        let id = case["case_id"].as_str().expect("a case id");
        assert_eq!(
            case["oracle"]["matched_golden"], true,
            "the oracle drifted from a golden: {case}"
        );
        match case["verdict"].as_str().expect("verdict string") {
            "agree" => {
                agree += 1;
                // No fabricated agreement: an `agree` case reproduced the oracle exactly.
                assert_eq!(
                    case["rust"]["available"], true,
                    "an agree case must carry a Rust observation: {case}"
                );
                assert_eq!(case["rust"]["stdout"], case["oracle"]["stdout"], "{case}");
                assert_eq!(case["rust"]["exit_code"], case["oracle"]["exit_code"], "{case}");
            }
            "unsupported" => unsupported += 1,
            "skipped" => skipped += 1,
            "mismatch" => {
                assert!(
                    KNOWN_RUNTIME_GAPS.contains(&id),
                    "unexpected mismatch for `{id}` — not in KNOWN_RUNTIME_GAPS: {case}"
                );
                mismatched.push(id.to_string());
            }
            other => panic!("unexpected verdict {other}: {case}"),
        }
    }

    // The summary must count exactly the cases we saw (a mismatch is never fabricated or
    // hidden — zero mismatches is the target, and an unexpected one still fails above).
    assert_eq!(json["summary"]["agree"], agree);
    assert_eq!(json["summary"]["unsupported"], unsupported);
    assert_eq!(json["summary"]["skipped"], skipped);
    assert_eq!(
        json["summary"]["mismatch"].as_u64().expect("a mismatch count"),
        mismatched.len() as u64,
        "summary.mismatch must count exactly the mismatching cases: {}",
        lenient.stdout
    );

    // The suite exit code is `1` iff a mismatch remains (unsupported/skipped are non-fatal
    // without `--strict`).
    let expected_exit = if mismatched.is_empty() { 0 } else { 1 };
    assert_eq!(lenient.code, Some(expected_exit), "{}", lenient.describe());

    if agreement_possible {
        assert!(
            agree >= 1,
            "the freshly-compiled module-run cases must agree: {}",
            lenient.stdout
        );
    } else {
        // Reached only when the child has *no* Rust runtime at all — neither a fresh debug
        // build nor a workspace default (`target/{release,debug}/beam_rt`), or when `erlc`
        // is missing (in which case the module-run cases are `skipped` and no `eval` case
        // exercises the runtime either). Either way nothing may agree: an `agree` verdict
        // requires a Rust observation, which cannot exist without a resolvable runtime.
        // `tests/oracle.rs` pins the same invariant for a forced-absent runtime (it injects
        // a nonexistent `$BEAM_RT` and asserts the Rust side is `unsupported`).
        assert_eq!(agree, 0, "nothing may agree without a usable Rust runtime");
    }
    // Every case is accounted for by exactly one verdict bucket, so this holds both today
    // (with capability gaps) and once the gate is fully green — unlike a hard-coded
    // `unsupported + skipped >= 1`, which would pin the current gap count.
    assert_eq!(
        agree + unsupported + skipped + mismatched.len() as u64,
        json["summary"]["total"].as_u64().expect("a total count"),
        "every case must fall into exactly one verdict bucket: {}",
        lenient.stdout
    );

    let strict = invoke(&["suite", "--strict", "--out", &scratch.arg()], &env);
    let strict_expected = if !mismatched.is_empty() || unsupported + skipped > 0 {
        1
    } else {
        0
    };
    assert_eq!(strict.code, Some(strict_expected), "{}", strict.describe());

    // A single case can be re-run by id without touching anything else.
    let one = invoke(
        &["run", "etf/term_to_binary_tuple", "--json", "--out", &scratch.arg()],
        &env,
    );
    assert_eq!(one.code, Some(0), "{}", one.describe());
    let json: serde_json::Value = serde_json::from_str(&one.stdout).expect("parses");
    assert_eq!(json["mode"], "run");
    assert_eq!(json["summary"]["total"], 1);
    assert_eq!(json["cases"][0]["oracle"]["matched_golden"], true);
}

#[test]
fn compare_evaluates_an_ad_hoc_expression_without_a_golden() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    if !rust_runtime_available() {
        eprintln!("SKIP: no fresh Rust runtime (target/debug/beam_rt) — nothing to compare");
        return;
    }
    // Default resolution would prefer a (possibly stale) `target/release/beam_rt`; inject
    // the freshly built debug runtime instead. The fresh `dist_probe` helper is injected too
    // (harmless here — a `compare` runs an expression case — but keeps the corpus-adjacent
    // resolution consistent).
    let fresh = common::fresh_rust_runtime();
    let helper = common::fresh_helper();
    let env: Vec<(&str, &str)> = fresh_beam_rt_env(&fresh)
        .into_iter()
        .chain(fresh_helper_env(&helper))
        .collect();
    let invocation = invoke(
        &["compare", "length([a,b,c])", "--json", "--out", &Scratch::new("compare").arg()],
        &env,
    );
    assert_eq!(invocation.code, Some(0), "{}", invocation.describe());
    let json: serde_json::Value = serde_json::from_str(&invocation.stdout).expect("parses");
    assert_eq!(json["mode"], "compare");
    assert_eq!(json["cases"][0]["case_id"], "ad-hoc");
    assert_eq!(json["cases"][0]["oracle"]["stdout"], "ok <<131,97,3>>\n");
    assert_eq!(
        json["cases"][0]["oracle"]["matched_golden"],
        serde_json::Value::Null,
        "an ad-hoc comparison asserts no golden"
    );
    // `length/1` is inside the Rust runtime's evaluator, so both runtimes agree exactly —
    // and `beam_rt -eval` must not auto-print a second line (the probe prints the one line).
    assert_eq!(
        json["cases"][0]["verdict"], "agree",
        "{}",
        invocation.describe()
    );
    assert_eq!(json["cases"][0]["rust"]["available"], true);
    assert_eq!(
        json["cases"][0]["rust"]["stdout"], "ok <<131,97,3>>\n",
        "{}",
        invocation.describe()
    );
    assert_eq!(
        json["cases"][0]["rust"]["stdout"], json["cases"][0]["oracle"]["stdout"],
        "{}",
        invocation.describe()
    );
    assert_eq!(
        json["cases"][0]["rust"]["exit_code"], json["cases"][0]["oracle"]["exit_code"],
        "{}",
        invocation.describe()
    );
}

/// A fake `$BEAM_RT` that prints a *different* stdout must still be a hard `[MISMATCH]`.
#[cfg(unix)]
#[test]
fn a_rust_runtime_with_differing_output_is_still_a_mismatch() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    let scratch = Scratch::new("fake-mismatch");
    let fake = fake_runtime(&scratch, "beam_rt-fake", "echo 'ok bogus'\nexit 0");
    let fake = fake.to_str().expect("utf-8 path");

    let invocation = invoke(
        &["run", "etf/term_to_binary_tuple", "--json", "--out", &scratch.arg()],
        &[("BEAM_RT", fake)],
    );
    assert_eq!(invocation.code, Some(1), "{}", invocation.describe());
    let json: serde_json::Value = serde_json::from_str(&invocation.stdout).expect("parses");
    assert_eq!(json["cases"][0]["verdict"], "mismatch");
    assert_eq!(json["cases"][0]["rust"]["available"], true);
    assert_eq!(json["cases"][0]["rust"]["stdout"], "ok bogus\n");
}

/// The stderr classifier: the runtime's documented boot banner is not an observable, but
/// **any other** stderr line still is — the classification can never fabricate agreement.
#[cfg(unix)]
#[test]
fn a_boot_banner_is_ignored_but_other_stderr_is_a_mismatch() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    // `length([a,b,c])` → the probe prints `ok <term_to_binary(3)>` on stdout, which is
    // exactly what the oracle produces; only the stderr differs below.
    let expr = "length([a,b,c])";

    // Identical stdout + exit status, plus the runtime's boot banner on stderr: AGREES.
    let banner = Scratch::new("banner-only");
    let fake = fake_runtime(
        &banner,
        "beam_rt-fake",
        "echo 'ok <<131,97,3>>'\n\
         echo 'beam_rt erts-17.1 booted: 22 preloaded modules, 21 core atoms, 1 process' 1>&2\n\
         exit 0",
    );
    let fake = fake.to_str().expect("utf-8 path");
    let invocation = invoke(
        &["compare", expr, "--json", "--out", &banner.arg()],
        &[("BEAM_RT", fake)],
    );
    assert_eq!(invocation.code, Some(0), "{}", invocation.describe());
    let json: serde_json::Value = serde_json::from_str(&invocation.stdout).expect("parses");
    assert_eq!(
        json["cases"][0]["verdict"], "agree",
        "a banner-only stderr difference must not turn agreement into a mismatch: {}",
        invocation.describe()
    );
    assert_eq!(
        json["cases"][0]["diffs"].as_array().map(Vec::len),
        Some(0),
        "{}",
        invocation.describe()
    );

    // Banner *plus* a genuine diagnostic: a hard MISMATCH (exit 1), reported on stderr.
    let noise = Scratch::new("banner-noise");
    let fake = fake_runtime(
        &noise,
        "beam_rt-fake",
        "echo 'ok <<131,97,3>>'\n\
         echo 'beam_rt erts-17.1 booted: 22 preloaded modules' 1>&2\n\
         echo 'warning: something changed' 1>&2\n\
         exit 0",
    );
    let fake = fake.to_str().expect("utf-8 path");
    let invocation = invoke(
        &["compare", expr, "--json", "--out", &noise.arg()],
        &[("BEAM_RT", fake)],
    );
    assert_eq!(invocation.code, Some(1), "{}", invocation.describe());
    let json: serde_json::Value = serde_json::from_str(&invocation.stdout).expect("parses");
    assert_eq!(
        json["cases"][0]["verdict"], "mismatch",
        "{}",
        invocation.describe()
    );
    let diffs = json["cases"][0]["diffs"].as_array().expect("diffs array");
    assert!(
        diffs.iter().any(|d| d["field"] == "stderr"),
        "the surviving non-banner line is reported as a stderr diff: {}",
        invocation.describe()
    );
    let stderr_diff = diffs
        .iter()
        .find(|d| d["field"] == "stderr")
        .expect("a stderr diff");
    assert!(
        stderr_diff["rust"]
            .as_str()
            .unwrap_or_default()
            .contains("warning: something changed"),
        "the reported diff carries the significant line: {}",
        invocation.describe()
    );
    assert!(
        !stderr_diff["rust"]
            .as_str()
            .unwrap_or_default()
            .contains("booted"),
        "the banner is not part of the reported diff: {}",
        invocation.describe()
    );
}

/// A fake `$BEAM_RT` that signals `unsupported` (exit 3) is honest: `unsupported` lenient,
/// a failure under `--strict` — and never a `mismatch`.
#[cfg(unix)]
#[test]
fn a_rust_runtime_signalling_unsupported_is_honest_not_a_mismatch() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    // Serialize the full corpus sweep against the binary's other corpus-running tests: two
    // corpus cases register an `epmd` node name (`-sname`/`-name`), so overlapping sweeps
    // would make the oracle lose the name race and report a spurious mismatch. Held for the
    // whole body — see `common::corpus_run_lock`.
    let _corpus_run = common::corpus_run_lock();
    let scratch = Scratch::new("fake-unsupported");
    let fake = fake_runtime(
        &scratch,
        "beam_rt-fake",
        "echo 'beam_rt: unsupported: bogus feature' 1>&2\nexit 3",
    );
    let fake = fake.to_str().expect("utf-8 path");

    let lenient = invoke(
        &["run", "etf/term_to_binary_tuple", "--json", "--out", &scratch.arg()],
        &[("BEAM_RT", fake)],
    );
    assert_eq!(lenient.code, Some(0), "{}", lenient.describe());
    let json: serde_json::Value = serde_json::from_str(&lenient.stdout).expect("parses");
    assert_eq!(json["cases"][0]["verdict"], "unsupported");
    assert_eq!(json["cases"][0]["rust"]["available"], false);
    let reason = json["cases"][0]["rust"]["reason"]
        .as_str()
        .expect("an unsupported side carries a reason");
    assert!(
        reason.contains("beam_rt: unsupported: bogus feature"),
        "the reason carries the runtime's stderr line: {reason}"
    );

    // `--strict` escalates the capability gap to a non-zero exit.
    let strict = invoke(&["suite", "--strict", "--out", &scratch.arg()], &[("BEAM_RT", fake)]);
    assert_eq!(strict.code, Some(1), "{}", strict.describe());
}

/// Write a synthetic known-gaps allowlist (`<case-id> beam_bifs fixture` per line, with a
/// leading `#`-comment) into `path`, creating its parent directory if needed.
#[cfg(unix)]
fn write_allowlist(path: &std::path::Path, case_ids: &[&str]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the allowlist dir");
    }
    let mut body = String::from("# a synthetic allowlist for the ratchet end-to-end test\n");
    for id in case_ids {
        body.push_str(&format!("{id} beam_bifs fixture\n"));
    }
    std::fs::write(path, body).expect("write the allowlist");
}

/// The ratchet, end to end, against the real corpus and a fake exit-3 runtime.
///
/// A fake `$BEAM_RT` that signals `beam_rt: unsupported: …` (exit 3) for **every** case makes
/// every corpus case a capability gap (`unsupported`, or `skipped` when a host tool such as
/// `erlc` is absent), so:
///   * an allowlist recording **every** case id is a clean run (`exit 0`), with every gap
///     classified `allowed` — state-independent, the corpus size is never hard-coded;
///   * an allowlist that omits one id (an *unauthorized* gap) and adds a bogus id (a *stale*
///     entry) is `exit 1`, and the generated `GAPS.md` names both.
#[cfg(unix)]
#[test]
fn the_ratchet_tolerates_only_the_recorded_gaps_end_to_end() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    // Serialize the full corpus sweep against the binary's other corpus-running tests: two
    // corpus cases register an `epmd` node name (`-sname`/`-name`), so overlapping sweeps
    // would make the oracle lose the name race and report a spurious mismatch. Held for the
    // whole body — see `common::corpus_run_lock`.
    let _corpus_run = common::corpus_run_lock();
    let scratch = Scratch::new("ratchet");
    let fake = fake_runtime(
        &scratch,
        "beam_rt-fake",
        "echo 'beam_rt: unsupported: bogus feature' 1>&2\nexit 3",
    );
    let fake = fake.to_str().expect("utf-8 path");

    let ids: Vec<&str> = differential::corpus::cases()
        .iter()
        .map(|case| case.id)
        .collect();
    assert!(!ids.is_empty(), "the corpus must not be empty");

    // ---- clean run: an allowlist recording every case id is exit 0 ----------------------
    let clean_list = scratch.0.join("all-gaps.txt");
    write_allowlist(&clean_list, &ids);
    let clean = invoke(
        &[
            "suite",
            "--strict",
            "--allowlist",
            &clean_list.display().to_string(),
            "--json",
            "--out",
            &scratch.arg(),
        ],
        // The fake is injected as BOTH `$BEAM_RT` and `$BEAM_DIST_PROBE`: the `dist/*` command
        // cases spawn the helper (not `beam_rt`), so without the second injection a built
        // workspace would let them `agree`, their allowlist entries would go stale, and the
        // clean run would no longer be exit 0.
        &[("BEAM_RT", fake), ("BEAM_DIST_PROBE", fake)],
    );
    assert_eq!(clean.code, Some(0), "{}", clean.describe());
    let json: serde_json::Value = serde_json::from_str(&clean.stdout).expect("the report parses");
    assert_eq!(json["summary"]["mismatch"], 0, "{}", clean.stdout);
    assert_eq!(json["ratchet"]["unauthorized"], 0, "{}", clean.stdout);
    assert_eq!(json["ratchet"]["stale"], 0, "{}", clean.stdout);
    // State-independent: every gap is allowlisted, so `allowed` equals the gap total — the
    // corpus size / split is never hard-coded.
    let gaps = json["summary"]["unsupported"].as_u64().expect("unsupported count")
        + json["summary"]["skipped"].as_u64().expect("skipped count");
    assert!(
        gaps >= 1,
        "the fake exit-3 runtime must make at least one case a gap: {}",
        clean.stdout
    );
    assert_eq!(
        json["ratchet"]["allowed"].as_u64(),
        Some(gaps),
        "every gap is allowlisted: {}",
        clean.stdout
    );

    // ---- dirty run: one gap omitted (unauthorized) + a bogus entry (stale) --------------
    let omitted = ids[0];
    let dirty_list = scratch.0.join("dirty-gaps.txt");
    write_allowlist(&dirty_list, &ids[1..]);
    let mut dirty_body = std::fs::read_to_string(&dirty_list).expect("read the dirty allowlist");
    dirty_body.push_str("zzz/not-a-case beam_bifs a deliberate typo\n");
    std::fs::write(&dirty_list, dirty_body).expect("write the dirty allowlist");

    let gaps_path = scratch.0.join("GAPS.md");
    let dirty = invoke(
        &[
            "suite",
            "--strict",
            "--allowlist",
            &dirty_list.display().to_string(),
            "--gaps",
            &gaps_path.display().to_string(),
            "--json",
            "--out",
            &scratch.arg(),
        ],
        // Both injections (see the clean run above): every case — including the `dist/*`
        // command cases driven through the helper — must be a capability gap.
        &[("BEAM_RT", fake), ("BEAM_DIST_PROBE", fake)],
    );
    assert_eq!(dirty.code, Some(1), "{}", dirty.describe());
    let json: serde_json::Value = serde_json::from_str(&dirty.stdout).expect("the report parses");
    assert_eq!(json["summary"]["mismatch"], 0, "{}", dirty.stdout);
    assert_eq!(json["ratchet"]["unauthorized"], 1, "{}", dirty.stdout);
    assert_eq!(json["ratchet"]["stale"], 1, "{}", dirty.stdout);

    let markdown = std::fs::read_to_string(&gaps_path).expect("the gaps report was written");
    assert!(
        markdown.contains("| unauthorized |"),
        "the report carries an unauthorized status cell: {markdown}"
    );
    assert!(
        markdown.contains("| stale |"),
        "the report carries a stale status cell: {markdown}"
    );
    assert!(
        markdown.contains(omitted),
        "the omitted (unauthorized) case id `{omitted}` is named: {markdown}"
    );
    assert!(
        markdown.contains("zzz/not-a-case"),
        "the bogus (stale) entry is named: {markdown}"
    );
}

/// A `Mismatch` whose case id **is** recorded in an allowlist is still a hard failure: the
/// allowlist tolerates only `unsupported`/`skipped` capability gaps, so it can never convert a
/// genuine divergence into a pass.
///
/// A fake `$BEAM_RT` that prints a **differing** stdout (exit 0) makes every corpus case a
/// `mismatch`; a synthetic allowlist records the target eval case's id. The binary must still
/// report the mismatch and exit `1`, classify the recorded id `stale` (never `allowed`), and
/// never let `ratchet.allowed` absorb it — so `gate.sh`'s `mismatch > 0 → exit 1` rule is what
/// the binary enforces.
#[cfg(unix)]
#[test]
fn a_mismatch_is_never_masked_by_an_allowlist_end_to_end() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    // Serialize the full corpus sweep against the binary's other corpus-running tests: two
    // corpus cases register an `epmd` node name (`-sname`/`-name`), so overlapping sweeps
    // would make the oracle lose the name race and report a spurious mismatch. Held for the
    // whole body — see `common::corpus_run_lock`.
    let _corpus_run = common::corpus_run_lock();
    let scratch = Scratch::new("ratchet-mismatch");
    // Prints a different stdout than the oracle for EVERY invocation: a genuine divergence.
    let fake = fake_runtime(&scratch, "beam_rt-fake", "echo 'ok bogus'\nexit 0");
    let fake = fake.to_str().expect("utf-8 path");

    // Record the (mismatching) eval case's id in the allowlist — this is the entry that must
    // NOT be able to tolerate the mismatch.
    let target = "etf/term_to_binary_tuple";
    let list = scratch.0.join("mismatch-gaps.txt");
    write_allowlist(&list, &[target]);

    let invocation = invoke(
        &[
            "suite",
            "--strict",
            "--allowlist",
            &list.display().to_string(),
            "--json",
            "--out",
            &scratch.arg(),
        ],
        // The fake is injected as BOTH `$BEAM_RT` and `$BEAM_DIST_PROBE`: the `dist/*` command
        // cases spawn the helper, so without the second injection (on a machine with a built
        // helper) they would silently agree instead of diverging.
        &[("BEAM_RT", fake), ("BEAM_DIST_PROBE", fake)],
    );
    assert_eq!(invocation.code, Some(1), "{}", invocation.describe());
    let json: serde_json::Value =
        serde_json::from_str(&invocation.stdout).expect("the report parses");

    // The recorded case really did diverge (its id is an eval case, always runnable).
    assert!(
        json["summary"]["mismatch"].as_u64().expect("a mismatch count") >= 1,
        "the fake runtime must diverge: {}",
        invocation.stdout
    );
    // The allowlist records the target id, yet NOTHING is allowed: a mismatch is never a gap.
    assert_eq!(
        json["ratchet"]["allowed"], 0,
        "an allowlisted mismatch must never be `allowed`: {}",
        invocation.stdout
    );
    // Its entry did not reproduce as a gap → `stale` (the allowlist must be tightened).
    assert_eq!(
        json["ratchet"]["stale"], 1,
        "the recorded mismatch id is the one stale entry: {}",
        invocation.stdout
    );
    // There are no gaps at all here (a differing runtime is never `unsupported`), but a host
    // without `erlc` would `skip` the module-run cases, so `unauthorized` is not pinned.

    // The recorded case is reported as a `mismatch`, not hidden.
    let cases = json["cases"].as_array().expect("a cases array");
    let recorded = cases
        .iter()
        .find(|case| case["case_id"] == target)
        .unwrap_or_else(|| panic!("the recorded case is in the report: {}", invocation.stdout));
    assert_eq!(
        recorded["verdict"], "mismatch",
        "the recorded case must be reported as a mismatch: {}",
        invocation.stdout
    );
    // A mismatch is never available-but-agreeing: the two sides really differ.
    assert_ne!(
        recorded["rust"]["stdout"], recorded["oracle"]["stdout"],
        "the reported mismatch must be backed by differing stdout: {}",
        invocation.stdout
    );
}

/// A missing/unreadable allowlist is a loud env error (exit 2) that fails **before** the
/// corpus runs — no report is written.
#[test]
fn a_missing_allowlist_is_a_loud_env_error_with_no_corpus_run() {
    let scratch = Scratch::new("ratchet-missing-allowlist");
    let missing = scratch.0.join("does-not-exist-allowlist.txt");
    let invocation = invoke(
        &[
            "suite",
            "--strict",
            "--allowlist",
            &missing.display().to_string(),
            "--out",
            &scratch.arg(),
        ],
        &[],
    );
    assert_eq!(invocation.code, Some(2), "{}", invocation.describe());
    assert!(
        invocation.stderr.contains("allowlist"),
        "the error names the allowlist: {}",
        invocation.describe()
    );
    assert!(
        invocation.stderr.contains("does-not-exist-allowlist.txt"),
        "{}",
        invocation.describe()
    );
    assert!(
        !scratch.report().exists(),
        "no report for an unreadable allowlist"
    );
}

/// `--allowlist` without `--strict` is a usage error (exit 2): the ratchet is meaningful only
/// under the strict gate.
#[test]
fn an_allowlist_without_strict_is_a_usage_error() {
    let scratch = Scratch::new("ratchet-no-strict");
    std::fs::create_dir_all(&scratch.0).expect("create the scratch dir");
    let list = scratch.0.join("known-gaps.txt");
    std::fs::write(&list, "# empty\n").expect("write the allowlist");
    let invocation = invoke(
        &[
            "suite",
            "--allowlist",
            &list.display().to_string(),
            "--out",
            &scratch.arg(),
        ],
        &[],
    );
    assert_eq!(invocation.code, Some(2), "{}", invocation.describe());
    assert!(
        invocation.stderr.contains("requires '--strict'"),
        "{}",
        invocation.describe()
    );
}