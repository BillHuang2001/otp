//! End-to-end CLI tests for the `differential` binary.
//!
//! These drive the real binary as a child process, so the documented exit-code contract
//! (0 agreed / 1 mismatch / 2 usage-env-IO) and the `--help` surface are locked in, and
//! environment overrides (`$ERTS_ORACLE`, `$BEAM_RT`) can be injected without touching
//! this test process. Tests that need the oracle **skip loudly** when `erl` is missing.

use std::path::PathBuf;
use std::process::Command;

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
        let dir = std::env::temp_dir().join(format!(
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

/// Whether a Rust runtime is resolvable from the test process (the child binary resolves
/// the same one, since both embed the same `CARGO_MANIFEST_DIR`).
fn rust_runtime_available() -> bool {
    matches!(differential::runtime::resolve_rust(), Ok(Some(_)))
}

/// Whether the module-run cases can be exercised (they need the system `erlc`).
fn erlc_available() -> bool {
    matches!(differential::module_build::resolve_erlc(), Ok(Some(_)))
}

/// Write an executable shell script into `scratch` (a fake `$BEAM_RT`) and return its path.
#[cfg(unix)]
fn fake_runtime(scratch: &Scratch, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = scratch.0.join(name);
    std::fs::create_dir_all(&scratch.0).expect("create scratch dir");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake runtime");
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

#[test]
fn the_suite_reports_module_agreement_and_expression_capability_gaps() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    // Agreement is only observable when both a Rust runtime and `erlc` exist. When they do
    // not, the module-run cases degrade to `unsupported`/`skipped` (never a fabricated pass).
    let agreement_possible = rust_runtime_available() && erlc_available();
    if !agreement_possible {
        eprintln!("NOTE: no usable ($BEAM_RT + erlc) pair — module agreement not exercised");
    }
    let scratch = Scratch::new("suite");

    let lenient = invoke(&["suite", "--json", "--out", &scratch.arg()], &[]);
    assert_eq!(lenient.code, Some(0), "{}", lenient.describe());
    let json: serde_json::Value =
        serde_json::from_str(&lenient.stdout).expect("the suite report parses");
    assert_eq!(json["schema"], "differential/1");
    assert_eq!(json["mode"], "suite");
    assert_eq!(json["oracle"]["available"], true);

    let mut agree = 0;
    let mut unsupported = 0;
    let mut skipped = 0;
    for case in json["cases"].as_array().expect("cases") {
        assert_eq!(
            case["oracle"]["matched_golden"], true,
            "the oracle drifted from a golden: {case}"
        );
        assert_ne!(
            case["verdict"], "mismatch",
            "the corpus must never mismatch here: {case}"
        );
        match case["verdict"].as_str().expect("verdict string") {
            "agree" => agree += 1,
            "unsupported" => unsupported += 1,
            "skipped" => skipped += 1,
            other => panic!("unexpected verdict {other}: {case}"),
        }
    }
    assert_eq!(json["summary"]["mismatch"], 0);
    assert_eq!(json["summary"]["agree"], agree);
    if agreement_possible {
        assert!(
            agree >= 1,
            "the freshly-compiled module-run cases must agree: {}",
            lenient.stdout
        );
    } else {
        assert_eq!(agree, 0, "nothing may agree without a usable Rust runtime");
    }
    assert!(
        unsupported + skipped >= 1,
        "the expression corpus is a capability gap: {}",
        lenient.stdout
    );

    let strict = invoke(&["suite", "--strict", "--out", &scratch.arg()], &[]);
    assert_eq!(strict.code, Some(1), "{}", strict.describe());

    // A single case can be re-run by id without touching anything else.
    let one = invoke(
        &["run", "etf/term_to_binary_tuple", "--json", "--out", &scratch.arg()],
        &[],
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
    let invocation = invoke(
        &["compare", "length([a,b,c])", "--json", "--out", &Scratch::new("compare").arg()],
        &[],
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
    // `length/1` is outside the Rust runtime's current subset: it signals a capability gap
    // (exit 3), which is an honest `unsupported` — never a `mismatch`.
    assert_eq!(
        json["cases"][0]["verdict"], "unsupported",
        "{}",
        invocation.describe()
    );
    assert_eq!(json["cases"][0]["rust"]["available"], false);
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

/// A fake `$BEAM_RT` that signals `unsupported` (exit 3) is honest: `unsupported` lenient,
/// a failure under `--strict` — and never a `mismatch`.
#[cfg(unix)]
#[test]
fn a_rust_runtime_signalling_unsupported_is_honest_not_a_mismatch() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
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