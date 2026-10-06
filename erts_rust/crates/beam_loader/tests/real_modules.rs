//! A **broad real-module LOAD sweep** over the running system's OTP install.
//!
//! Where `tests/container.rs`, `tests/ir.rs` and `tests/widened_ir.rs` load a
//! couple of committed oracle fixtures, this file is the *wide* evidence: it walks
//! the real `ebin` directories of the installed OTP (`stdlib`, `kernel`, and
//! `compiler` when present) and loads **every** `*.beam` through the crate's public
//! entry point [`beam_loader::load_file`] — the whole pipeline
//! (`container::parse` → `chunk::parse` → `decode::decode_stream` →
//! `lower::build_module`).
//!
//! # What the sweep proves
//!
//! * **Load breadth.** The public loader pipeline parses most of a real OTP
//!   install; a broken walk (a missing application, a directory-resolution bug) is
//!   caught by a floor on the number of modules discovered.
//! * **Failures are surfaced, never swallowed.** Every module that fails to load
//!   is printed by name with its exact typed error.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! * **A missing OTP install skips LOUDLY.** When `erl` is absent (or the
//!   `stdlib`/`kernel` `ebin` directories cannot be located) the test prints
//!   `SKIP: <reason>` and returns — it never fails in that case.
//! * **Known load failures are an explicit allowlist.** [`KNOWN_LOAD_FAILURES`]
//!   records the modules that do **not** load at this commit (upstream
//!   `beam_loader`/`beam_codec` gaps — external-fun literals, an inline
//!   big-integer operand, `lower::build_module` rejecting the instruction model).
//!   The observed failure set must be a **subset** of it: a module failing that is
//!   *not* listed fails the test (a regression), while a listed module that starts
//!   loading is merely printed (never a failure) so the allowlist can shrink.
//! * **The allowlist is expected to go stale.** Sibling executors are concurrently
//!   closing the individual load gaps listed here, so a module dropping off the
//!   list is normal — it is *reported*, never a failure. This mirrors
//!   `beam_engine/tests/stdlib_sweep.rs` but is **load-only** (no execution) and
//!   drives `beam_loader` directly.
//!
//! Ground truth is the system OTP install itself; the `ebin` directories are
//! located at **test time** (see [`app_ebin`]).
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader
//! ```
//!
//! With `-- --nocapture` the per-module failures and the `loaded N/M` summary are
//! printed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// The allowlist (data, not a pass)
// ---------------------------------------------------------------------------

/// The modules of this OTP install that the loader genuinely cannot load today.
///
/// This is **data, not a pass**: the sweep prints every one by name with its typed
/// error, and fails if *any module outside this set* fails to load (a regression).
/// A listed module that starts loading is reported and simply drops off — sibling
/// executors are actively closing these gaps, so the list is expected to shrink.
///
/// The modules were recorded at this commit by running the sweep with an empty
/// list and copying the printed `LOAD-FAIL` names. At this commit the install had
/// 261 modules (stdlib 98, kernel 104, compiler 59) of which 220 load; the 41
/// below do not.
const KNOWN_LOAD_FAILURES: &[&str] = &[
    // stdlib — 19 modules.
    "dets_v9",
    "epp",
    "erl_pp",
    "erl_scan",
    "erl_stdlib_errors",
    "escript",
    "file_sorter",
    "gen_event",
    "gen_fsm",
    "gen_server",
    "gen_statem",
    "json",
    "maps",
    "proc_lib",
    "rand",
    "shell",
    "shell_docs",
    "supervisor",
    "supervisor_bridge",
    // kernel — 14 modules.
    "application_controller",
    "disk_log",
    "erl_kernel_errors",
    "error_logger",
    "group",
    "inet_epmd_dist",
    "inet_tcp_dist",
    "logger",
    "logger_formatter",
    "logger_h_common",
    "logger_proxy",
    "logger_server",
    "net",
    "net_kernel",
    // compiler — 8 modules.
    "beam_call_types",
    "beam_doc",
    "beam_jump",
    "beam_ssa_alias",
    "beam_ssa_pre_codegen",
    "beam_ssa_ss",
    "compile",
    "sys_core_fold",
];

// ---------------------------------------------------------------------------
// Locating the installed OTP `ebin` directories
// ---------------------------------------------------------------------------

/// One application `ebin` directory to sweep, with its application name and the
/// sorted `*.beam` module basenames it holds.
struct SweepDir {
    app: &'static str,
    dir: PathBuf,
    modules: Vec<String>,
}

/// The `ebin` directory of the running Erlang's `app` application (`stdlib`,
/// `kernel`, `compiler`, …), or `None` when it cannot be located.
///
/// Resolution order:
/// 1. `$ERTS_<APP>_DIR` (uppercased app name; accepts either the application dir
///    — `…/lib/stdlib-8.1` — or its `ebin` subdir);
/// 2. `erl -noshell -eval 'io:format("~s",[code:lib_dir(<app>)]),halt().'`
///    evaluated at test time (OTP's `code:lib_dir/1` returns the *application*
///    dir, whose `ebin` subdirectory holds the `.beam` files).
///
/// A caller that gets `None` must **skip loudly** (never fail) — see
/// [`skip_reason`].
fn app_ebin(app: &str) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(format!("ERTS_{}_DIR", app.to_uppercase())) {
        let dir = PathBuf::from(dir);
        if has_beams(&dir) {
            return Some(dir);
        }
        let ebin = dir.join("ebin");
        return has_beams(&ebin).then_some(ebin);
    }

    let eval = format!("io:format(\"~s\",[code:lib_dir({app})]),halt().");
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &eval])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let app_dir = PathBuf::from(stdout.trim());
    if has_beams(&app_dir) {
        return Some(app_dir);
    }
    let ebin = app_dir.join("ebin");
    has_beams(&ebin).then_some(ebin)
}

/// True when `dir` is a directory that contains at least one `*.beam` file.
fn has_beams(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        entry
            .path()
            .extension()
            .map(|extension| extension == "beam")
            .unwrap_or(false)
    })
}

/// The reason the test should skip (and return early), or `None` when the oracle
/// OTP install is available. `stdlib` and `kernel` are required; a caller that
/// gets `Some` must print it and return — never fail.
fn skip_reason() -> Option<String> {
    if app_ebin("stdlib").is_none() {
        return Some(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    if app_ebin("kernel").is_none() {
        return Some(
            "SKIP: system Erlang/kernel not found (need `erl` on PATH or $ERTS_KERNEL_DIR) — \
             the real-module sweep requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    None
}

/// The installed-OTP applications swept: `stdlib` and `kernel` are required
/// (guaranteed present by [`skip_reason`]); `compiler` is swept too when present
/// (extra evidence, never a failure).
fn sweep_dirs() -> Vec<SweepDir> {
    let mut dirs = Vec::new();
    for app in ["stdlib", "kernel", "compiler"] {
        let Some(dir) = app_ebin(app) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut modules: Vec<String> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
            .filter_map(|path| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .collect();
        modules.sort();
        dirs.push(SweepDir { app, dir, modules });
    }
    dirs
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// Loads every `*.beam` in the sweep directories through [`beam_loader::load_file`]
/// (the public pipeline) and proves each one that loads parses to a usable module,
/// while surfacing every load failure loudly.
#[test]
fn loads_every_installed_otp_beam() {
    if let Some(reason) = skip_reason() {
        eprintln!("{reason}");
        return;
    }

    let dirs = sweep_dirs();
    assert!(
        !dirs.is_empty(),
        "the sweep found no OTP `ebin` directories to load"
    );

    let mut total = 0usize;
    let mut loaded = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut unexpected: Vec<(String, String)> = Vec::new();
    let mut empty: Vec<String> = Vec::new();
    let mut now_loading: Vec<&str> = Vec::new();

    for dir in &dirs {
        for module in &dir.modules {
            total += 1;
            let path = dir.dir.join(format!("{module}.beam"));
            match beam_loader::load_file(&path) {
                Ok(loaded_module) => {
                    loaded += 1;
                    if let Some(name) = KNOWN_LOAD_FAILURES.iter().find(|name| **name == module) {
                        now_loading.push(name);
                    }
                    // A successful load must have produced at least one function:
                    // an empty function list means the pipeline parsed the chunks
                    // but silently dropped the code — still a defect.
                    if loaded_module.functions.is_empty() {
                        empty.push(format!("{}:{module}", dir.app));
                    }
                }
                Err(error) => {
                    // Surface every failure loudly, with the exact typed error.
                    let message = format!("{error:?}");
                    println!("LOAD-FAIL {module}: {message}");
                    if KNOWN_LOAD_FAILURES.contains(&module.as_str()) {
                        failures.push((module.clone(), message));
                    } else {
                        unexpected.push((module.clone(), message));
                    }
                }
            }
        }
    }

    let mut report = String::new();
    let _ = write!(report, "SWEEP: loaded {loaded}/{total} modules");
    for dir in &dirs {
        let _ = write!(report, " ({}={})", dir.app, dir.modules.len());
    }
    println!("{report}");
    println!(
        "SWEEP: {} known load failure(s) (documented upstream gaps):",
        failures.len()
    );
    for (module, error) in &failures {
        println!("  - {module}: {error}");
    }
    if !now_loading.is_empty() {
        println!(
            "SWEEP: these allowlisted modules now load (drop them from KNOWN_LOAD_FAILURES): \
             {now_loading:?}"
        );
    }

    // A broad real sweep must cover a large number of modules. The floor is well
    // below the real count (stdlib+kernel ≈ 200) so an OTP point release
    // adding/removing a few modules does not break it, but a broken walk (or a
    // missing application) does.
    assert!(
        total >= 150,
        "the sweep only found {total} `.beam` files — expected stdlib+kernel (≈200)"
    );
    assert!(
        empty.is_empty(),
        "{} module(s) loaded but yielded no function: {empty:?}",
        empty.len()
    );
    assert!(
        unexpected.is_empty(),
        "{} UNEXPECTED load failure(s) (a regression): {unexpected:?}",
        unexpected.len()
    );
}
