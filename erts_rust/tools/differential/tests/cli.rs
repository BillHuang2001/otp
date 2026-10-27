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

use std::path::{Path, PathBuf};
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
    run_binary(None, args, envs)
}

/// [`invoke`] with the child binary's **working directory** set explicitly.
///
/// This is how the crate's cwd-independence is pinned: the child must behave identically no
/// matter where it was invoked from, so a corpus case may never depend on the caller's cwd.
fn invoke_in(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Invocation {
    run_binary(Some(dir), args, envs)
}

/// Spawn the built `differential` binary, optionally in an explicit working directory.
fn run_binary(cwd: Option<&Path>, args: &[&str], envs: &[(&str, &str)]) -> Invocation {
    let mut command = Command::new(env!("CARGO_BIN_EXE_differential"));
    command.args(args).envs(envs.iter().copied());
    // Point the child at a **worktree-local** temp dir instead of the ambient `$TMPDIR`: the
    // child's own scratch (e.g. the `.beam` files `module_build` compiles for module-run cases)
    // would otherwise land in the shared/volatile `$TMPDIR` this task exists to keep test
    // fixtures out of (`common::scratch_root`). This makes the tests independent of `$TMPDIR`
    // entirely — they pass even when it is missing or read-only.
    command.env("TMPDIR", common::scratch_root());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
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
        vec!["merge", "--help"],
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
        (vec!["merge"], "'merge' requires a --from <dir>"),
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
///
/// A host-oracle drift is **not** a mismatch and has its own allowlist (`known-drift.txt`) — see
/// [`classify_suite_json`] — so a case can never sneak into this list to escape the drift rules.
const KNOWN_RUNTIME_GAPS: &[&str] = &[];

/// Load the committed **host-oracle drift** allowlist (`known-drift.txt`).
///
/// The corpus goldens are frozen against the tree's pinned `erts` release; the installed oracle
/// may be a *different* release, in which case a few goldens (the three `system_info(version)`
/// cases) legitimately miss on the oracle side while our runtime still reproduces them. That is
/// [`Verdict::OracleDrift`], ratcheted by this committed list — exactly as `gate.sh` does with
/// `--drift-allowlist`. A missing/unparsable file is a panic: the file is committed next to this
/// crate and without it drift cannot be told from divergence.
fn known_drift_allowlist() -> differential::allowlist::Allowlist {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("known-drift.txt");
    differential::allowlist::Allowlist::load(&path).unwrap_or_else(|error| {
        panic!(
            "cannot load the committed drift allowlist {}: {error}",
            path.display()
        )
    })
}

/// The verdict buckets of a `differential/1` suite report, plus every **per-case evidence
/// violation** the report's own fields expose.
///
/// Kept separate from the (multi-hour, serial) corpus sweep it is used by, so a cheap test can
/// pin the classification rules against hand-written report JSON — see
/// `the_suite_classifier_requires_per_case_drift_evidence`.
#[derive(Debug, Default)]
struct Buckets {
    agree: u64,
    /// `oracle_drift` cases: the oracle missed a golden our own side reproduced.
    drift: u64,
    unsupported: u64,
    skipped: u64,
    /// The ids of `mismatch` cases (never a drift, never a gap).
    mismatched: Vec<String>,
    /// One entry per case whose verdict is not supported by its own evidence. Empty is the only
    /// acceptable value.
    violations: Vec<String>,
}

/// Classify a suite report's cases into verdict buckets **and** check each verdict against the
/// case's own evidence.
///
/// The evidence rules (each violation is recorded, never tolerated):
/// * `agree` — both sides reproduced the golden and the two sides' stdout + exit status are
///   identical (otherwise the "agreement" is fabricated);
/// * `oracle_drift` — `oracle.matched_golden == false`, `rust.matched_golden == true`, and the
///   two sides' stdout/exit status really differ with at least one reported field diff (drift is
///   **positive evidence only**: our side proves the golden is still reproducible);
/// * `skipped` — the case never ran, so it must carry no golden check at all (a skip can never
///   hide an oracle miss);
/// * `mismatch`/`unsupported`/`skipped` are counted, never re-labelled;
/// * a case that **did** run and whose oracle side missed its golden must have its id recorded in
///   `recorded_drift`, i.e. in the committed `known-drift.txt` — an unrecorded oracle golden miss
///   is a failure, and a deliberate allowlist entry (with a reason) is the only way to accept it.
///
/// A recorded entry is **not** required to drift (on an oracle whose `erts` version matches the
/// tree's it reproduces its golden); staleness is the gate's ratchet job, not this test's.
fn classify_suite_json(
    json: &serde_json::Value,
    recorded_drift: &differential::allowlist::Allowlist,
) -> Buckets {
    let mut buckets = Buckets::default();
    for case in json["cases"].as_array().expect("a report carries a cases array") {
        let id = case["case_id"].as_str().expect("a case id");
        let verdict = case["verdict"].as_str().expect("a verdict string");
        let oracle_matched = case["oracle"]["matched_golden"].as_bool();
        let rust_matched = case["rust"]["matched_golden"].as_bool();
        let observables_agree = case["rust"]["stdout"] == case["oracle"]["stdout"]
            && case["rust"]["exit_code"] == case["oracle"]["exit_code"];
        match verdict {
            "agree" => {
                buckets.agree += 1;
                if case["rust"]["available"] != true {
                    buckets
                        .violations
                        .push(format!("agree `{id}` carries no Rust observation: {case}"));
                }
                if rust_matched != Some(true) {
                    buckets.violations.push(format!(
                        "agree `{id}`: our side did not reproduce its golden \
                         (rust.matched_golden={rust_matched:?}) — that is a mismatch, not an \
                         agreement: {case}"
                    ));
                }
                if oracle_matched != Some(true) {
                    buckets.violations.push(format!(
                        "agree `{id}`: the oracle did not reproduce the golden \
                         (oracle.matched_golden={oracle_matched:?}) — an agreement needs both \
                         sides to match: {case}"
                    ));
                }
                if !observables_agree {
                    buckets.violations.push(format!(
                        "agree `{id}`: the two sides' stdout/exit status differ: {case}"
                    ));
                }
            }
            "oracle_drift" => {
                buckets.drift += 1;
                // Positive evidence only, never a heuristic: the oracle missed the golden, our
                // side reproduced it, and the observables (and the reported diffs) really differ.
                if oracle_matched != Some(false) {
                    buckets.violations.push(format!(
                        "oracle_drift `{id}` requires oracle.matched_golden == false \
                         (got {oracle_matched:?}): {case}"
                    ));
                }
                if rust_matched != Some(true) {
                    buckets.violations.push(format!(
                        "oracle_drift `{id}` requires rust.matched_golden == true \
                         (got {rust_matched:?}) — a golden our side fails is a mismatch: {case}"
                    ));
                }
                if observables_agree {
                    buckets.violations.push(format!(
                        "oracle_drift `{id}`: the two sides' stdout and exit status are \
                         identical, so calling the case drift is a fabrication: {case}"
                    ));
                }
                if case["diffs"].as_array().map(Vec::is_empty) != Some(false) {
                    buckets.violations.push(format!(
                        "oracle_drift `{id}`: the report carries no field diff, so nothing \
                         observable differs: {case}"
                    ));
                }
            }
            "unsupported" => buckets.unsupported += 1,
            "skipped" => {
                buckets.skipped += 1;
                if oracle_matched.is_some() {
                    buckets.violations.push(format!(
                        "skipped `{id}`: a skipped case never ran, so it cannot carry a golden \
                         check (oracle.matched_golden={oracle_matched:?}): {case}"
                    ));
                }
            }
            "mismatch" => buckets.mismatched.push(id.to_string()),
            other => panic!("unexpected verdict {other}: {case}"),
        }
        // A case that really ran may only miss its oracle golden when the golden is a **recorded**
        // host-oracle drift. `oracle_drift` already required exactly that miss above; every other
        // executed verdict must have reproduced it. (A mismatch stays a mismatch either way — this
        // rule never converts one into drift.)
        if verdict != "skipped" && verdict != "oracle_drift" && oracle_matched != Some(true) {
            if recorded_drift.contains(id) {
                eprintln!(
                    "NOTE: `{id}` misses its oracle golden on this host and is recorded in \
                     known-drift.txt (verdict {verdict})"
                );
            } else {
                buckets.violations.push(format!(
                    "`{id}`: the oracle missed its committed golden \
                     (oracle.matched_golden={oracle_matched:?}) and the case is not recorded in \
                     known-drift.txt — an unrecorded oracle golden miss is a failure, and drift \
                     needs a deliberate `<case-id> <owning-crate> <reason…>` entry: {case}"
                ));
            }
        }
    }
    buckets
}

/// Assert `buckets.violations` is empty, reporting every violation with the invoking child's
/// output for context.
fn assert_no_violations(buckets: &Buckets, output: &str) {
    assert!(
        buckets.violations.is_empty(),
        "per-case verdict evidence violations ({}):\n{}\n--- report ---\n{output}",
        buckets.violations.len(),
        buckets.violations.join("\n")
    );
}

/// A minimal `differential/1` **suite-shaped** report around `cases`, for the classifier test.
///
/// Deliberately carries a `summary` and a `schema` so the synthetic JSON has the same shape the
/// binary writes; [`classify_suite_json`] reads only `cases`, so the summary values are inert.
fn synthetic_suite(cases: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({
        "schema": "differential/1",
        "mode": "suite",
        "summary": {
            "total": cases.len(),
            "agree": 0,
            "mismatch": 0,
            "unsupported": 0,
            "skipped": 0,
            "oracle_drift": 0
        },
        "cases": cases,
    })
}

/// One synthetic case entry for the classifier test.
///
/// `matched` is `None` when the side carries no golden check at all (a side that did not run, or
/// a case that never ran); a `sudo` side whose `stdout` is `None` did not run. Every case gets
/// exit status `0` on any side that ran, so a `stdout` difference alone is a differing observable.
fn synthetic_case(
    id: &str,
    verdict: &str,
    oracle_matched: Option<bool>,
    rust_matched: Option<bool>,
    oracle_stdout: Option<&str>,
    rust_stdout: Option<&str>,
    diffs: usize,
) -> serde_json::Value {
    fn side(
        matched: Option<bool>,
        stdout: Option<&str>,
    ) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "available".to_string(),
            serde_json::Value::Bool(stdout.is_some()),
        );
        map.insert(
            "stdout".to_string(),
            stdout.map_or(serde_json::Value::Null, |text| {
                serde_json::Value::String(text.to_string())
            }),
        );
        map.insert("stdout_hex".to_string(), serde_json::Value::Null);
        map.insert(
            "exit_code".to_string(),
            if stdout.is_some() {
                serde_json::Value::from(0)
            } else {
                serde_json::Value::Null
            },
        );
        if let Some(matched) = matched {
            map.insert(
                "matched_golden".to_string(),
                serde_json::Value::Bool(matched),
            );
        }
        serde_json::Value::Object(map)
    }
    let diffs: Vec<serde_json::Value> = (0..diffs)
        .map(|index| {
            serde_json::json!({
                "field": "stdout",
                "oracle": format!("oracle-{index}"),
                "rust": format!("rust-{index}"),
            })
        })
        .collect();
    serde_json::json!({
        "case_id": id,
        "description": "a synthetic case",
        "contracts": ["A.8"],
        "eval": "ok",
        "verdict": verdict,
        "oracle": side(oracle_matched, oracle_stdout),
        "rust": side(rust_matched, rust_stdout),
        "diffs": diffs,
    })
}

/// The suite classification rules, pinned on **synthetic report JSON**.
///
/// The two tests that exercise the real corpus take hours (a full serial sweep), so the facts
/// that make their verdict handling honest must be affordable on every change. This one needs no
/// oracle, no runtime and no corpus:
///
/// * (a) an `oracle_drift` case is accepted only on its **positive evidence** — the oracle missed
///   the golden, our side reproduced it, and the two sides' observables really differ;
/// * (b) the same case with `rust.matched_golden == false` is a violation (a golden our side
///   fails is a mismatch, never drift), and so is a "drift" whose two sides are identical or
///   which carries no reported diff;
/// * (c) a `mismatch` is counted as a mismatch — never as a drift, never as a gap;
/// * (d) an `agree` case whose oracle side missed its golden is a violation (agreement needs both
///   sides to match);
/// * (e) a case that ran and missed its oracle golden is a violation **unless** its id is recorded
///   in the drift allowlist (`known-drift.txt`), and a recorded entry is accepted without
///   requiring it to drift;
/// * (f) a `skipped` case may carry no golden check, so a skip can never hide an oracle miss.
#[test]
fn the_suite_classifier_requires_per_case_drift_evidence() {
    let none = differential::allowlist::Allowlist::parse("# no recorded drift\n", None)
        .expect("an empty drift allowlist parses");

    // ---- (a) a well-formed drift is accepted -------------------------------------------
    let drift = synthetic_case(
        "system_info/version",
        "oracle_drift",
        Some(false),
        Some(true),
        Some("ok oracle\n"),
        Some("ok ours\n"),
        1,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![drift.clone()]), &none);
    assert_no_violations(&buckets, "a well-formed drift");
    assert_eq!(buckets.drift, 1);
    assert_eq!(buckets.agree + buckets.unsupported + buckets.skipped, 0);
    assert!(buckets.mismatched.is_empty());
    // A recorded drift id is not *required*: this case is not in the allowlist at all and is
    // still accepted, because the drift allowlist only tolerates a miss our side did not have.

    // ---- (b) a fabricated / unsupported drift is a violation ----------------------------
    // (b1) our side failed the golden: a mismatch by construction, never drift.
    let our_side_failed = synthetic_case(
        "system_info/version",
        "oracle_drift",
        Some(false),
        Some(false),
        Some("ok oracle\n"),
        Some("ok ours\n"),
        1,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![our_side_failed]), &none);
    assert_eq!(buckets.drift, 1, "the bucket follows the verdict string");
    assert!(
        buckets
            .violations
            .iter()
            .any(|violation| violation.contains("rust.matched_golden == true")),
        "our side failing the golden must be a violation: {:?}",
        buckets.violations
    );
    // (b2) both sides agree byte-for-byte: nothing differs, so there is nothing to drift.
    let identical = synthetic_case(
        "system_info/version",
        "oracle_drift",
        Some(false),
        Some(true),
        Some("ok same\n"),
        Some("ok same\n"),
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![identical]), &none);
    assert!(
        buckets
            .violations
            .iter()
            .any(|violation| violation.contains("identical")),
        "an identical-observables drift is a fabrication: {:?}",
        buckets.violations
    );
    // (b3) differing stdout but no reported diff: the reported evidence is incomplete.
    let no_diff = synthetic_case(
        "system_info/version",
        "oracle_drift",
        Some(false),
        Some(true),
        Some("ok oracle\n"),
        Some("ok ours\n"),
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![no_diff]), &none);
    assert!(
        buckets
            .violations
            .iter()
            .any(|violation| violation.contains("no field diff")),
        "a drift must carry the field diff it was derived from: {:?}",
        buckets.violations
    );

    // ---- (c) a mismatch stays a mismatch -----------------------------------------------
    let mismatch = synthetic_case(
        "etf/term_to_binary_tuple",
        "mismatch",
        Some(true),
        Some(false),
        Some("ok oracle\n"),
        Some("ok ours\n"),
        1,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![mismatch]), &none);
    assert_eq!(buckets.mismatched, vec!["etf/term_to_binary_tuple".to_string()]);
    assert_eq!(buckets.drift, 0, "a mismatch is never counted as drift");
    assert_eq!(
        buckets.unsupported + buckets.skipped,
        0,
        "a mismatch is never counted as a gap"
    );
    assert_no_violations(&buckets, "a plain mismatch");

    // ---- (d) an `agree` needs both sides to match the golden ---------------------------
    let agree_with_oracle_miss = synthetic_case(
        "flags/custom_argument",
        "agree",
        Some(false),
        Some(true),
        Some("ok same\n"),
        Some("ok same\n"),
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![agree_with_oracle_miss]), &none);
    assert_eq!(buckets.agree, 1);
    assert!(
        buckets
            .violations
            .iter()
            .any(|violation| violation.contains("an agreement needs both sides")),
        "an agree whose oracle missed the golden is a violation: {:?}",
        buckets.violations
    );
    // A well-formed agreement (both sides matched, observables identical) is clean.
    let agree = synthetic_case(
        "flags/custom_argument",
        "agree",
        Some(true),
        Some(true),
        Some("ok same\n"),
        Some("ok same\n"),
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![agree]), &none);
    assert_no_violations(&buckets, "a well-formed agreement");
    assert_eq!(buckets.agree, 1);

    // ---- (e) an oracle golden miss must be recorded ------------------------------------
    // Our side is unobserved (a capability gap), so the verdict is `unsupported` — but the
    // committed golden is host-version-dependent and the case is not recorded: a failure.
    let unrecorded = synthetic_case(
        "boot_extra/system_info_version_is_frozen_17_1",
        "unsupported",
        Some(false),
        None,
        Some("ok false\n"),
        None,
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![unrecorded.clone()]), &none);
    assert_eq!(buckets.unsupported, 1);
    assert_eq!(buckets.violations.len(), 1, "{:?}", buckets.violations);
    assert!(
        buckets.violations[0].contains("known-drift.txt"),
        "the violation must name the drift allowlist: {:?}",
        buckets.violations
    );
    // The same case, recorded with a reason: accepted, and **not** required to drift.
    let recorded = differential::allowlist::Allowlist::parse(
        "boot_extra/system_info_version_is_frozen_17_1  differential  the golden pins erts-17.1; \
         this host oracle is a different erts release\n",
        None,
    )
    .expect("a one-entry drift allowlist parses");
    let buckets = classify_suite_json(&synthetic_suite(vec![unrecorded]), &recorded);
    assert_eq!(buckets.unsupported, 1);
    assert_no_violations(&buckets, "a recorded oracle golden miss");
    // A recorded id that reproduces its golden is **not** a violation here (staleness is the
    // gate's ratchet job, not this test's).
    let recorded_but_matching = synthetic_case(
        "boot_extra/system_info_version_is_frozen_17_1",
        "agree",
        Some(true),
        Some(true),
        Some("ok true\n"),
        Some("ok true\n"),
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![recorded_but_matching]), &recorded);
    assert_no_violations(&buckets, "a recorded drift that does not drift");

    // ---- (f) a skip can never hide an oracle miss --------------------------------------
    let skipped = synthetic_case("dist/x", "skipped", None, None, None, None, 0);
    let buckets = classify_suite_json(&synthetic_suite(vec![skipped]), &none);
    assert_eq!(buckets.skipped, 1);
    assert_no_violations(&buckets, "a plain skip");
    let skipped_with_check = synthetic_case(
        "dist/y",
        "skipped",
        Some(false),
        None,
        Some("ok false\n"),
        None,
        0,
    );
    let buckets = classify_suite_json(&synthetic_suite(vec![skipped_with_check]), &none);
    assert_eq!(buckets.skipped, 1);
    assert!(
        buckets
            .violations
            .iter()
            .any(|violation| violation.contains("never ran")),
        "a skipped case carrying a golden check is a violation: {:?}",
        buckets.violations
    );

    // ---- every case lands in exactly one bucket ----------------------------------------
    let mixed = classify_suite_json(
        &synthetic_suite(vec![
            synthetic_case(
                "a/agree",
                "agree",
                Some(true),
                Some(true),
                Some("ok\n"),
                Some("ok\n"),
                0,
            ),
            synthetic_case(
                "a/drift",
                "oracle_drift",
                Some(false),
                Some(true),
                Some("ok oracle\n"),
                Some("ok ours\n"),
                1,
            ),
            synthetic_case(
                "a/unsupported",
                "unsupported",
                Some(true),
                None,
                Some("ok\n"),
                None,
                0,
            ),
            synthetic_case("a/skipped", "skipped", None, None, None, None, 0),
            synthetic_case(
                "a/mismatch",
                "mismatch",
                Some(true),
                Some(false),
                Some("ok\n"),
                Some("no\n"),
                1,
            ),
        ]),
        &none,
    );
    assert_no_violations(&mixed, "a mixed report");
    assert_eq!(
        mixed.agree
            + mixed.drift
            + mixed.unsupported
            + mixed.skipped
            + mixed.mismatched.len() as u64,
        5,
        "every case must fall into exactly one bucket"
    );
}


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

    // Classify every case into exactly one verdict bucket, checking each verdict against the
    // case's own evidence (see `classify_suite_json`): an `agree` needs *both* sides to reproduce
    // the golden and the observables to match, `oracle_drift` is positive-evidence-only, and an
    // oracle golden miss that is not recorded in the committed `known-drift.txt` is a failure.
    let recorded_drift = known_drift_allowlist();
    let buckets = classify_suite_json(&json, &recorded_drift);
    assert_no_violations(&buckets, &lenient.stdout);
    for id in &buckets.mismatched {
        assert!(
            KNOWN_RUNTIME_GAPS.contains(&id.as_str()),
            "unexpected mismatch for `{id}` — not in KNOWN_RUNTIME_GAPS: {}",
            lenient.stdout
        );
    }

    // The summary must count exactly the cases we saw (a drift or a mismatch is never fabricated
    // or hidden — zero mismatches is the target, and an unexpected one still fails above).
    assert_eq!(json["summary"]["agree"], buckets.agree);
    assert_eq!(
        json["summary"]["oracle_drift"], buckets.drift,
        "summary.oracle_drift must count exactly the drifting cases: {}",
        lenient.stdout
    );
    assert_eq!(json["summary"]["unsupported"], buckets.unsupported);
    assert_eq!(json["summary"]["skipped"], buckets.skipped);
    assert_eq!(
        json["summary"]["mismatch"].as_u64().expect("a mismatch count"),
        buckets.mismatched.len() as u64,
        "summary.mismatch must count exactly the mismatching cases: {}",
        lenient.stdout
    );

    // The suite exit code is `1` iff a mismatch remains: a host-oracle drift and a capability gap
    // are both non-fatal without `--strict` (the binary's `suite_exit_code` documents that).
    let expected_exit = if buckets.mismatched.is_empty() { 0 } else { 1 };
    assert_eq!(lenient.code, Some(expected_exit), "{}", lenient.describe());

    if agreement_possible {
        assert!(
            buckets.agree >= 1,
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
        assert_eq!(
            buckets.agree, 0,
            "nothing may agree without a usable Rust runtime"
        );
    }
    // Every case is accounted for by exactly one verdict bucket, so this holds both today
    // (with capability gaps and host-oracle drift) and once the gate is fully green — unlike a
    // hard-coded `unsupported + skipped >= 1`, which would pin the current gap count.
    assert_eq!(
        buckets.agree + buckets.drift + buckets.unsupported + buckets.skipped
            + buckets.mismatched.len() as u64,
        json["summary"]["total"].as_u64().expect("a total count"),
        "every case must fall into exactly one verdict bucket: {}",
        lenient.stdout
    );

    // `--strict` escalates **every** non-agreeing class to exit 1: a mismatch (always), and a
    // capability gap or a host-oracle drift (under `--strict`). This invocation passes no
    // `--drift-allowlist`, so the drift is unrecorded and must be exactly as fatal as a gap.
    let strict = invoke(&["suite", "--strict", "--out", &scratch.arg()], &env);
    let strict_expected = if !buckets.mismatched.is_empty()
        || buckets.drift > 0
        || buckets.unsupported + buckets.skipped > 0
    {
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

/// A **tree-relative** contract-A.9 case must agree no matter where the harness is invoked
/// from.
///
/// The subject is `epp:parse_file("lib/stdlib/src/ordsets.erl", [])`: the expression addresses
/// the OTP tree **repo-relatively**, so the case can only agree if the *child runtimes* run in
/// the tree root. `gate.sh` happens to be run from `$ERL_TOP`, but `cargo test` runs from the
/// crate directory and a user may run the binary from anywhere — so the harness resolves the
/// root itself (`runtime::otp_root`) and hands it to every child. Without that, **both** sides
/// get `{badmatch,{error,enoent}}` and — because the oracle then no longer reproduces its
/// golden — `compare::verdict_with` reports a (spurious) `mismatch`, i.e. exit `1`.
///
/// The child is deliberately started in a scratch directory that contains no `lib/stdlib/src`
/// at all, so agreement can only come from the harness's own cwd control.
#[test]
fn a_tree_relative_selfhost_case_agrees_from_a_scratch_working_directory() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    if !rust_runtime_available() {
        eprintln!("SKIP: no fresh Rust runtime (target/debug/beam_rt) — nothing to compare");
        return;
    }
    let fresh = common::fresh_rust_runtime();
    let helper = common::fresh_helper();
    let env: Vec<(&str, &str)> = fresh_beam_rt_env(&fresh)
        .into_iter()
        .chain(fresh_helper_env(&helper))
        .collect();

    // The scratch dir the harness process runs in: a fresh directory outside the OTP tree,
    // holding neither the repository nor any stdlib source.
    let scratch = Scratch::new("cwd-scratch");
    std::fs::create_dir_all(&scratch.0).expect("create the scratch working directory");
    assert!(
        !scratch.0.join("lib/stdlib/src/ordsets.erl").exists(),
        "the scratch working directory must not contain the case's tree-relative subject"
    );

    let invocation = invoke_in(
        &scratch.0,
        &[
            "run",
            "selfhost/epp_parse_file_reads_a_real_stdlib_source_into_forms",
            "--json",
            "--out",
            &scratch.arg(),
        ],
        &env,
    );
    assert_eq!(
        invocation.code,
        Some(0),
        "a tree-relative selfhost case must agree from any working directory (the harness \
         resolves the OTP tree root itself): {}",
        invocation.describe()
    );
    let json: serde_json::Value = serde_json::from_str(&invocation.stdout).expect("parses");
    let case = &json["cases"][0];
    assert_eq!(
        case["case_id"], "selfhost/epp_parse_file_reads_a_real_stdlib_source_into_forms",
        "{}",
        invocation.describe()
    );
    assert_eq!(
        case["rust"]["available"], true,
        "the injected fresh runtime must be observed: {}",
        invocation.describe()
    );
    assert_eq!(
        case["verdict"], "agree",
        "the tree-relative case must agree, not drift into a mismatch: {}",
        invocation.describe()
    );
    assert_eq!(
        case["oracle"]["stdout"], "ok true\n",
        "the oracle runs in the tree root too, so it sees the real stdlib source: {}",
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

/// Write a synthetic **empty** drift allowlist (a `#`-comment only, no entries) into `path`.
///
/// Passing an *empty* drift allowlist is what exercises the drift ratchet without pinning the
/// corpus: any `oracle_drift` case would be `drift_unauthorized`, and any entry would be
/// `drift_stale` — and the fake exit-3 runtime below makes every case a gap, so neither can
/// happen and a zero count is the honest expectation.
#[cfg(unix)]
fn write_empty_drift_allowlist(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create the drift allowlist dir");
    }
    std::fs::write(
        path,
        "# an empty host-oracle drift allowlist: the fake runtime never produces a drift\n",
    )
    .expect("write the drift allowlist");
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
///
/// Both runs also pass `--drift-allowlist` pointing at a freshly written **empty** allowlist, so
/// the drift ratchet is exercised end to end too. No case can be an `oracle_drift` here: our side
/// is *unobserved* (the fake exits 3), and `Verdict::OracleDrift` requires our side to have
/// reproduced the golden — so `drift_unauthorized` and `drift_stale` must both be 0, and a
/// non-zero value would mean the drift classification had invented evidence.
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

    // The drift ratchet: an empty allowlist, so every case must stay out of both drift counts.
    let drift_list = scratch.0.join("known-drift.txt");
    write_empty_drift_allowlist(&drift_list);
    let drift_list = drift_list.display().to_string();

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
            "--drift-allowlist",
            &drift_list,
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
    // The drift ratchet ran (a drift allowlist was given, so the keys are present) and found
    // nothing to tolerate — and an unobserved runtime cannot drift.
    assert_eq!(json["summary"]["oracle_drift"], 0, "{}", clean.stdout);
    assert_eq!(
        json["ratchet"]["drift_unauthorized"],
        0,
        "an unobserved runtime can never produce a drift: {}",
        clean.stdout
    );
    assert_eq!(
        json["ratchet"]["drift_stale"],
        0,
        "the empty drift allowlist carries no entry that could go stale: {}",
        clean.stdout
    );
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
            "--drift-allowlist",
            &drift_list,
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
    // The drift ratchet is unaffected by the *gaps* ratchet being dirty: an unobserved runtime
    // produces no drift, and the empty drift allowlist has nothing to go stale.
    assert_eq!(json["summary"]["oracle_drift"], 0, "{}", dirty.stdout);
    assert_eq!(json["ratchet"]["drift_unauthorized"], 0, "{}", dirty.stdout);
    assert_eq!(json["ratchet"]["drift_stale"], 0, "{}", dirty.stdout);

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

/// `run --out-file <path>` writes a single-case report to **exactly** that path, and
/// `merge --from <dir>` folds a directory of such reports back into one suite-shaped report
/// and regenerates the same `GAPS.md` the (slow) serial `suite --gaps` sweep would have
/// written — the cheap regeneration route.
#[cfg(unix)]
#[test]
fn merge_folds_run_out_file_reports_into_a_generated_gaps_document() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    if !rust_runtime_available() {
        eprintln!("SKIP: no fresh Rust runtime (target/debug/beam_rt) — nothing to compare");
        return;
    }
    let scratch = Scratch::new("merge");
    let reports = scratch.0.join("reports");
    std::fs::create_dir_all(&reports).expect("create the reports dir");

    // (1) an AGREEING case under the real (fresh) runtime.
    let fresh = common::fresh_rust_runtime();
    let env = fresh_beam_rt_env(&fresh);
    let agree_path = reports.join("agree.json");
    let agree = invoke(
        &[
            "run",
            "etf/term_to_binary_tuple",
            "--out-file",
            &agree_path.display().to_string(),
        ],
        &env,
    );
    assert_eq!(agree.code, Some(0), "{}", agree.describe());
    let agree_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&agree_path).expect("read the agree report"))
            .expect("the --out-file report parses");
    assert_eq!(
        agree_json["cases"][0]["verdict"], "agree",
        "{}",
        agree.describe()
    );
    assert!(
        !reports.join("report.json").exists(),
        "--out-file must not also write <dir>/report.json"
    );

    // (2) a CAPABILITY GAP: a fake exit-3 runtime signals `unsupported` honestly.
    let fake = fake_runtime(
        &scratch,
        "beam_rt-fake",
        "echo 'beam_rt: unsupported: fixture' 1>&2\nexit 3",
    );
    let fake = fake.to_str().expect("utf-8 path");
    let gap_path = reports.join("gap.json");
    let gap = invoke(
        &[
            "run",
            "etf/term_to_binary_atom",
            "--out-file",
            &gap_path.display().to_string(),
        ],
        &[("BEAM_RT", fake)],
    );
    assert_eq!(
        gap.code,
        Some(0),
        "a lone unsupported case is not fatal: {}",
        gap.describe()
    );
    let gap_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&gap_path).expect("read the gap report"))
            .expect("the gap report parses");
    assert_eq!(
        gap_json["cases"][0]["verdict"], "unsupported",
        "{}",
        gap.describe()
    );

    // Fold the two reports: the allowlist records the one gap, so the ratchet is clean
    // (exit 0). A bogus `$BEAM_DIST_PROBE` proves `merge` never resolves/spawns the helper.
    let list = scratch.0.join("known-gaps.txt");
    write_allowlist(&list, &["etf/term_to_binary_atom"]);
    let gaps_path = scratch.0.join("GAPS.md");
    let merged_path = scratch.0.join("merged.json");
    let mut merge_env = env.clone();
    merge_env.push(("BEAM_DIST_PROBE", "/nonexistent/dist_probe"));
    let merged = invoke(
        &[
            "merge",
            "--from",
            &reports.display().to_string(),
            "--allowlist",
            &list.display().to_string(),
            "--gaps",
            &gaps_path.display().to_string(),
            "--json",
            "--out-file",
            &merged_path.display().to_string(),
        ],
        &merge_env,
    );
    assert_eq!(merged.code, Some(0), "{}", merged.describe());

    let json: serde_json::Value =
        serde_json::from_str(&merged.stdout).expect("the merged report parses");
    assert_eq!(json["mode"], "merge");
    assert_eq!(json["schema"], "differential/1");
    assert_eq!(json["summary"]["total"], 2);
    assert_eq!(json["summary"]["agree"], 1);
    assert_eq!(json["summary"]["unsupported"], 1);
    assert_eq!(json["summary"]["mismatch"], 0);
    assert_eq!(json["ratchet"]["allowed"], 1);
    assert_eq!(json["ratchet"]["unauthorized"], 0);
    assert_eq!(json["ratchet"]["stale"], 0);
    // Cases are ordered by id, deterministic regardless of file order.
    assert_eq!(json["cases"][0]["case_id"], "etf/term_to_binary_atom");
    assert_eq!(json["cases"][1]["case_id"], "etf/term_to_binary_tuple");

    // The merged report was written to EXACTLY the requested `--out-file` path.
    assert!(merged_path.is_file(), "the merged report is written");
    assert!(
        merged.stderr.contains("merged.json"),
        "the writer names the path: {}",
        merged.describe()
    );

    // The generated document is the same shape `suite --gaps` writes: the run line and the
    // gap table row, with the reason from the runtime and the owning crate from the allowlist.
    let markdown = std::fs::read_to_string(&gaps_path).expect("the generated GAPS.md");
    assert!(
        markdown.contains("Run: total=2 agree=1 mismatch=0 unsupported=1 skipped=0"),
        "{markdown}"
    );
    assert!(
        markdown.contains(
            "| etf/term_to_binary_atom | unsupported | beam_bifs | allowed | beam_rt: unsupported: fixture |"
        ),
        "{markdown}"
    );
    assert!(
        !markdown.contains("| etf/term_to_binary_tuple |"),
        "only gaps are listed: {markdown}"
    );
}

/// `merge` is a pure report-folding command: a missing/unknown `--from`, an empty directory
/// and a malformed `*.json` are all loud usage errors (exit 2) that name the offender.
#[cfg(unix)]
#[test]
fn merge_reports_loud_usage_errors_for_bad_input() {
    // A missing `--from` names the requirement and shows the merge help.
    let missing_from = invoke(&["merge", "--json"], &[]);
    assert_eq!(missing_from.code, Some(2), "{}", missing_from.describe());
    assert!(
        missing_from.stderr.contains("--from <dir>"),
        "{}",
        missing_from.describe()
    );
    assert!(
        missing_from.stderr.contains("differential merge"),
        "{}",
        missing_from.describe()
    );

    let scratch = Scratch::new("merge-bad-from");
    std::fs::create_dir_all(&scratch.0).expect("create scratch");

    // A `--from` that is not a directory.
    let not_a_dir = scratch.0.join("file.txt");
    std::fs::write(&not_a_dir, "x").expect("write the file");
    let bad_dir = invoke(&["merge", "--from", &not_a_dir.display().to_string()], &[]);
    assert_eq!(bad_dir.code, Some(2), "{}", bad_dir.describe());
    assert!(
        bad_dir.stderr.contains("not a readable directory"),
        "{}",
        bad_dir.describe()
    );

    // An empty directory (no `*.json` reports).
    let empty = scratch.0.join("empty");
    std::fs::create_dir_all(&empty).expect("create the empty dir");
    let none = invoke(&["merge", "--from", &empty.display().to_string()], &[]);
    assert_eq!(none.code, Some(2), "{}", none.describe());
    assert!(
        none.stderr.contains("no *.json reports"),
        "{}",
        none.describe()
    );

    // A malformed `*.json` is a loud error naming the file (never silently skipped).
    let dir = scratch.0.join("bogus");
    std::fs::create_dir_all(&dir).expect("create the bogus dir");
    let bogus = dir.join("bogus.json");
    std::fs::write(&bogus, "{ not json").expect("write the bogus report");
    let bad_json = invoke(&["merge", "--from", &dir.display().to_string()], &[]);
    assert_eq!(bad_json.code, Some(2), "{}", bad_json.describe());
    assert!(
        bad_json.stderr.contains("bogus.json"),
        "{}",
        bad_json.describe()
    );

    // `merge --help` succeeds without touching a runtime.
    let help = invoke(&["merge", "--help"], &[]);
    assert_eq!(help.code, Some(0), "{}", help.describe());
    assert!(help.stdout.contains("--from <dir>"), "{}", help.describe());
}

/// `gate.sh` resolves its allowlist from its arguments: an explicit `--allowlist <file>` is
/// honoured (a missing one is a loud env error, exit `2`, naming the path), `--help` prints the
/// usage and succeeds, and any other argument is a usage error. All of these are handled before
/// anything is built, so this pins the gate's argument handling without running the corpus.
#[cfg(unix)]
#[test]
fn gate_sh_resolves_the_allowlist_from_its_arguments() {
    let gate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("gate.sh");
    assert!(gate.is_file(), "the gate script is present: {}", gate.display());
    let run = |args: &[&str]| {
        let output = Command::new("sh")
            .arg(&gate)
            .args(args)
            .output()
            .expect("spawn gate.sh");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    // An explicit, non-existent allowlist is a loud env error naming the path, and it fails
    // before the (slow) build/corpus run.
    let scratch = Scratch::new("gate-allowlist");
    let missing = scratch.0.join("no-such-allowlist.txt");
    let (code, _stdout, stderr) = run(&["--allowlist", &missing.display().to_string()]);
    assert_eq!(code, Some(2), "stderr: {stderr}");
    assert!(stderr.contains("allowlist"), "stderr: {stderr}");
    assert!(stderr.contains("no-such-allowlist.txt"), "stderr: {stderr}");

    // `--help` prints the usage (which names `--allowlist`) and succeeds without building.
    let (code, stdout, _stderr) = run(&["--help"]);
    assert_eq!(code, Some(0), "stdout: {stdout}");
    assert!(stdout.contains("--allowlist"), "stdout: {stdout}");

    // An unknown argument is a usage error.
    let (code, _stdout, stderr) = run(&["--bogus"]);
    assert_eq!(code, Some(2), "stderr: {stderr}");
    assert!(stderr.contains("unknown argument"), "stderr: {stderr}");
}