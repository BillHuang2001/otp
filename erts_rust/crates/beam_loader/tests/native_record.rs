//! The `is_any_native_record/2` guard: a **real-module structural** check that the
//! opcode no longer lowers to the residual [`Instr::Generic`] fallback.
//!
//! `is_any_native_record Fail Src` (generic opcode 186; `emu/ops.tab:1634`) is the
//! `erlang:is_record/1` guard — branch to `Fail` iff `Src` is not a native record.
//! It is the only member of the OTP-29 native-record family the standard compiler
//! emits, with sites in `erl_eval`/`io_lib`/`erl_types`/`dialyzer_codeserver`/
//! `dbg_ieval`. The loader now lowers it to the typed
//! [`Instr::IsAnyNativeRecord`] variant, so the engine's residual-`Generic` path has
//! nothing the compiler emits left to interpret for this opcode.
//!
//! # What this proves
//!
//! * **`erl_eval` (stdlib, the canonical site) actually contains the opcode**, and it
//!   arrives as the typed variant — a positive control, so a sweep that found nothing
//!   could not pass vacuously.
//! * **No module in the stdlib sweep still carries the `Generic` form** — the
//!   regression this task removes.
//!
//! # Skip-vs-assert policy
//!
//! A missing OTP install (or a missing `erl_eval.beam`) **skips LOUDLY** with a
//! `SKIP:` line and returns — it never fails in that case, matching
//! `tests/real_modules.rs`.
//!
//! Ground truth is the system OTP install; the `ebin` directory is located at test
//! time (see [`app_ebin`]).

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::{Instr, Opcode};

// ---------------------------------------------------------------------------
// Locating the installed OTP `ebin` directory
// ---------------------------------------------------------------------------

/// The `ebin` directory of the running Erlang's `app` application, or `None` when it
/// cannot be located. Mirrors the resolution order of `tests/real_modules.rs`:
/// `$ERTS_<APP>_DIR` (app dir or its `ebin` subdir), then `code:lib_dir/1`.
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

/// The `(typed, generic)` occurrence counts of `is_any_native_record` in a loaded
/// module: `typed` counts [`Instr::IsAnyNativeRecord`] and `generic` counts the
/// residual [`Instr::Generic`] carrying the opcode.
fn count_sites(module: &beam_loader::Module) -> (usize, usize) {
    let mut typed = 0usize;
    let mut generic = 0usize;
    for function in &module.functions {
        for instr in &function.code {
            match instr {
                Instr::IsAnyNativeRecord { .. } => typed += 1,
                Instr::Generic { op, .. } if *op == Opcode::is_any_native_record => generic += 1,
                _ => {}
            }
        }
    }
    (typed, generic)
}

/// Proves the opcode lowers to its typed variant in the canonical real module and
/// nowhere in the stdlib sweep still hits the `Generic` fallback.
#[test]
fn is_any_native_record_lowers_typed_in_real_modules() {
    let Some(ebin) = app_ebin("stdlib") else {
        eprintln!(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the native-record lowering check requires the installed OTP `.beam` files"
        );
        return;
    };

    // Positive control: `erl_eval` is the canonical stdlib site of the opcode.
    let erl_eval_path = ebin.join("erl_eval.beam");
    if !erl_eval_path.is_file() {
        eprintln!(
            "SKIP: {} not found — the native-record lowering check needs the installed \
             stdlib `erl_eval.beam`",
            erl_eval_path.display()
        );
        return;
    }
    let erl_eval = beam_loader::load_file(&erl_eval_path).expect("erl_eval.beam must load");
    let (typed, generic) = count_sites(&erl_eval);
    assert!(
        typed > 0,
        "erl_eval must contain the `is_any_native_record` opcode, lowered to \
         Instr::IsAnyNativeRecord (found none)"
    );
    assert_eq!(
        generic, 0,
        "erl_eval must not leave any `is_any_native_record` on the Instr::Generic fallback"
    );

    // Regression sweep: no stdlib module may still carry the `Generic` form.
    let Ok(entries) = std::fs::read_dir(&ebin) else {
        eprintln!("SKIP: cannot read {}", ebin.display());
        return;
    };
    let mut modules: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
        .collect();
    modules.sort();

    let mut typed_total = 0usize;
    let mut generic_total = 0usize;
    let mut generic_modules: Vec<String> = Vec::new();
    for path in &modules {
        let module = beam_loader::load_file(path)
            .unwrap_or_else(|error| panic!("{} must load: {error:?}", path.display()));
        let (typed, generic) = count_sites(&module);
        typed_total += typed;
        generic_total += generic;
        if generic > 0 {
            generic_modules.push(path.display().to_string());
        }
    }

    println!(
        "SWEEP: is_any_native_record — {} typed site(s) over {} module(s), {} on the Generic fallback",
        typed_total,
        modules.len(),
        generic_total
    );
    assert_eq!(
        generic_total, 0,
        "is_any_native_record must never lower to Instr::Generic; still generic in: {generic_modules:?}"
    );
    assert!(
        typed_total >= typed,
        "the sweep must see at least the erl_eval site it counted"
    );
}
