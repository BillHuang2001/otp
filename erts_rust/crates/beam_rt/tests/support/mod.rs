//! Shared helpers for the `beam_rt` integration suites that compare this runtime
//! against the **system Erlang oracle**.
//!
//! Every helper that needs the toolchain returns an `Option` (or is paired with
//! [`tools_available`]); the calling test prints a **loud skip banner** when the
//! toolchain is absent. A missing `erl`/`erlc` is never a silent pass.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Whether `tool` can be spawned at all (a missing binary ⇒ `false`).
pub fn tool_exists(tool: &str) -> bool {
    Command::new(tool).arg("-noshell").output().is_ok()
}

/// Whether both `erl` and `erlc` are present.
pub fn tools_available() -> bool {
    tool_exists("erl") && tool_exists("erlc")
}

/// Print the standard loud-skip banner and return `false` when the toolchain is
/// missing, so a test can `if !tools_available() { return; }`.
pub fn require_tools(what: &str) -> bool {
    if tools_available() {
        true
    } else {
        eprintln!("SKIP {what}: the system `erl`/`erlc` toolchain is not on PATH");
        false
    }
}

/// A fresh scratch directory under `$TMPDIR`, unique per call.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("beam_rt_{tag}_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the scratch dir");
    dir
}

/// Compile `source` (an Erlang module's text, named `name`) into `dir` with the
/// system `erlc`, returning the `.beam` path. Panics with the compiler's stderr.
pub fn compile(dir: &Path, name: &str, source: &str) -> PathBuf {
    let src = dir.join(format!("{name}.erl"));
    std::fs::write(&src, source).expect("write the fixture source");
    let out = Command::new("erlc")
        .arg("-o")
        .arg(dir)
        .arg(&src)
        .output()
        .expect("run erlc");
    assert!(
        out.status.success(),
        "erlc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir.join(format!("{name}.beam"))
}

/// Run `erl -noshell [-pa <pa>] -eval <expr>` and return its trimmed stdout.
///
/// The expression MUST terminate (`halt(0).`); a non-zero exit yields `None`.
pub fn oracle_eval(pa: Option<&Path>, expr: &str) -> Option<String> {
    let mut cmd = Command::new("erl");
    cmd.arg("-noshell");
    if let Some(dir) = pa {
        cmd.arg("-pa").arg(dir);
    }
    let out = cmd.arg("-eval").arg(expr).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
