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
fn suite_reports_the_stub_side_unsupported_and_strict_escalates_it() {
    if !oracle_available() {
        eprintln!("SKIP: oracle erl not available");
        return;
    }
    let scratch = Scratch::new("suite");

    let lenient = invoke(&["suite", "--json", "--out", &scratch.arg()], &[]);
    assert_eq!(lenient.code, Some(0), "{}", lenient.describe());
    let json: serde_json::Value =
        serde_json::from_str(&lenient.stdout).expect("the suite report parses");
    assert_eq!(json["schema"], "differential/1");
    assert_eq!(json["mode"], "suite");
    assert_eq!(json["oracle"]["available"], true);
    for case in json["cases"].as_array().expect("cases") {
        assert_eq!(
            case["oracle"]["matched_golden"], true,
            "the oracle drifted from a golden: {case}"
        );
        assert_ne!(
            case["verdict"], "agree",
            "a stub Rust runtime must never agree: {case}"
        );
    }

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
}
