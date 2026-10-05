//! The **executable** end-to-end: spawn `beam_rt` and run the vertical slice.
//!
//! `tests/slice.rs` proves the library path; this test proves the *binary* — the CLI
//! parser, the driver, the exit codes and the stdout of a real process. The `.beam`
//! fixture is passed as an absolute path resolved from `CARGO_MANIFEST_DIR` (the test
//! process's working directory is the crate directory, so a relative path would not
//! resolve), and it is **reused** from `beam_loader`, never copied.
use std::path::Path;
use std::process::{Command, Output};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// The stdout of a finished run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The stderr of a finished run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn run_add_prints_five() {
    let output = beam_rt(&["run", SLICE_BEAM, "slice:add/2", "2", "3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "5\n");
}

#[test]
fn run_fib_prints_fifty_five() {
    let output = beam_rt(&["run", SLICE_BEAM, "slice:fib/1", "10"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "55\n");
}

#[test]
fn run_accepts_a_negative_integer_argument() {
    let output = beam_rt(&["run", SLICE_BEAM, "slice:add/2", "2", "-3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "-1\n");
}

#[test]
fn version_prints_the_banner() {
    for flag in ["--version", "-version"] {
        let output = beam_rt(&[flag]);
        assert!(output.status.success(), "stderr: {}", stderr(&output));
        let banner = stdout(&output);
        assert!(banner.contains("30.0-rc0"), "{banner}");
        assert!(banner.contains("erts-17.1"), "{banner}");
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn eval_prints_the_result() {
    let output = beam_rt(&["-eval", "2+3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "5\n");

    let output = beam_rt(&["-noshell", "-eval", "(1+2)*3"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "9\n");
}

#[test]
fn an_unknown_switch_is_unsupported_and_names_itself() {
    let output = beam_rt(&["--nope"]);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("beam_rt: unsupported:"), "{message}");
    assert!(message.contains("--nope"), "{message}");
    assert!(
        !message.contains("Usage:"),
        "no usage text on this path: {message}"
    );
}

#[test]
fn a_bad_eval_expression_is_unsupported() {
    // An `-eval` expression outside the supported integer grammar is a capability
    // gap (exit 3), not a genuine runtime error (exit 1).
    let output = beam_rt(&["-eval", "1+"]);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("beam_rt: unsupported:"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_non_integer_eval_expression_is_unsupported() {
    for expr in ["length([a,b,c])", "try(1)end."] {
        let output = beam_rt(&["-eval", expr]);
        assert_eq!(
            output.status.code(),
            Some(3),
            "{expr}: stderr {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("beam_rt: unsupported:"),
            "{expr}: {}",
            stderr(&output)
        );
        assert!(!stderr(&output).contains("Usage:"), "{expr}");
    }
}

#[test]
fn an_unmodeled_emulator_switch_is_unsupported_and_names_itself() {
    // Real `erl` accepts unmodeled switches as init args, so this is a capability
    // gap (exit 3) — reported once, naming the switch, with no usage text.
    let output = beam_rt(&[
        "-noshell",
        "-differential_test_flag",
        "hello",
        "-eval",
        "halt(0).",
    ]);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    let message = stderr(&output);
    assert!(message.contains("beam_rt: unsupported:"), "{message}");
    assert!(message.contains("-differential_test_flag"), "{message}");
    assert!(!message.contains("Usage:"), "{message}");
}

#[test]
fn a_missing_switch_operand_is_a_usage_error() {
    let output = beam_rt(&["-eval"]);
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("Usage:"), "{}", stderr(&output));
}

#[test]
fn a_stray_positional_argument_is_a_usage_error() {
    let output = beam_rt(&["junk"]);
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("Usage:"), "{}", stderr(&output));
}
#[test]
fn a_missing_file_fails() {
    let output = beam_rt(&["run", "/nonexistent/nope.beam", "nope:f/0"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(!stderr(&output).is_empty());
}

#[test]
fn the_s_action_applies_with_atom_arguments_and_discards_the_result() {
    let fixtures = Path::new(SLICE_BEAM)
        .parent()
        .expect("the fixture has a parent directory")
        .to_str()
        .expect("the path is UTF-8")
        .to_string();
    let output = beam_rt(&["-pa", &fixtures, "-s", "slice", "pick", "a"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "the result is discarded");
}

#[test]
fn the_code_path_is_searched_for_the_module() {
    // Without `-pa`, the module cannot be found: a runtime error, not a panic.
    let output = beam_rt(&["-s", "slice", "pick", "a"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("slice.beam"), "{}", stderr(&output));
}
