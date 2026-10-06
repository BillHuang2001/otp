//! The OTP-29 native-record opcode family: a **real-module structural** check that no
//! member still lowers to the residual [`Instr::Generic`] fallback.
//!
//! The family is generic opcodes 186-193, each lowered to its own typed variant:
//!
//! | # | `genop.tab` name | typed variant |
//! |---|---|---|
//! | 186 | `is_any_native_record` | [`Instr::IsAnyNativeRecord`] |
//! | 187 | `is_native_record` | [`Instr::IsNativeRecord`] |
//! | 188 | `get_record_elements` | [`Instr::GetRecordElements`] |
//! | 189 | `put_record` | [`Instr::PutRecord`] |
//! | 190 | `is_record_accessible` | [`Instr::IsRecordAccessible`] |
//! | 191 | `get_record_field` | [`Instr::GetRecordField`] |
//! | 192 | `get_record_elements_id` | [`Instr::GetRecordElementsId`] |
//! | 193 | `update_record_id` | [`Instr::UpdateRecordId`] |
//!
//! # What this proves
//!
//! * **`erl_eval` (stdlib, the canonical site of 186) actually contains
//!   `is_any_native_record`**, and it arrives typed — a positive control, so a sweep
//!   that found nothing could not pass vacuously.
//! * **A record-using module compiled by the system `erlc` lowers 186-191 typed** —
//!   [`compiled_native_records_lower_typed`] compiles `-record #rec{…}` /
//!   `-import_record/2` source and asserts those six variants appear and none is
//!   `Generic`.
//! * **No module in the stdlib/kernel/compiler sweep still carries a `Generic` form
//!   of any family member.**
//!
//! # Honest coverage
//!
//! The installed **stdlib/kernel/compiler** `.beam` files only ever contain opcode
//! **186** — the `is_record/1` guard, emitted for `erlang:is_record/1` with sites in
//! `erl_eval`/`io_lib`/`io_lib_pretty`. The other seven are emitted only for *native
//! records*, which no shipped OTP module uses, so the sweep is a no-regression guard
//! for 187-193; the compiled-module test is the positive control that actually
//! exercises their lowering. Opcodes **192/193 were added in OTP 30** and are absent
//! from the installed OTP-29 `beam_opcodes` table, so no system `erlc` can emit them —
//! their lowering is covered by the crate unit tests (`src/lower/instr.rs`) only.
//!
//! # Skip-vs-assert policy
//!
//! A missing OTP install (or a missing `erlc`, or a compiler that rejects native-record
//! syntax) **skips LOUDLY** with a `SKIP:` line and returns — it never fails in that
//! case, matching `tests/real_modules.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use beam_loader::{Instr, Module, Opcode};

/// The eight generic opcodes of the OTP-29 native-record family (186-193).
const FAMILY: [Opcode; 8] = [
    Opcode::is_any_native_record,
    Opcode::is_native_record,
    Opcode::get_record_elements,
    Opcode::put_record,
    Opcode::is_record_accessible,
    Opcode::get_record_field,
    Opcode::get_record_elements_id,
    Opcode::update_record_id,
];

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

/// Every `*.beam` file directly inside `dir`, name-sorted for a deterministic order.
fn beam_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut modules: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|ext| ext == "beam").unwrap_or(false))
        .collect();
    modules.sort();
    modules
}

// ---------------------------------------------------------------------------
// Census
// ---------------------------------------------------------------------------

/// A census of the native-record family over one module: how many family instructions
/// lowered to a typed variant, and how many still hit the [`Instr::Generic`] fallback.
#[derive(Default)]
struct Census {
    typed: usize,
    generic: usize,
}

impl Census {
    fn add(&mut self, other: &Census) {
        self.typed += other.typed;
        self.generic += other.generic;
    }
}

/// Counts the family's typed and residual-`Generic` occurrences in a loaded module.
fn census(module: &Module) -> Census {
    let mut census = Census::default();
    for function in &module.functions {
        for instr in &function.code {
            match instr {
                Instr::IsAnyNativeRecord { .. }
                | Instr::IsNativeRecord { .. }
                | Instr::GetRecordElements { .. }
                | Instr::PutRecord { .. }
                | Instr::IsRecordAccessible { .. }
                | Instr::GetRecordField { .. }
                | Instr::GetRecordElementsId { .. }
                | Instr::UpdateRecordId { .. } => census.typed += 1,
                Instr::Generic { op, .. } if FAMILY.contains(op) => census.generic += 1,
                _ => {}
            }
        }
    }
    census
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The `is_any_native_record` positive control (`erl_eval`) plus the regression sweep:
/// no installed stdlib/kernel/compiler module may leave a family member on `Generic`.
#[test]
fn native_record_family_never_lowers_generic_in_installed_modules() {
    let Some(stdlib) = app_ebin("stdlib") else {
        eprintln!(
            "SKIP: system Erlang/stdlib not found (need `erl` on PATH or $ERTS_STDLIB_DIR) — \
             the native-record lowering check requires the installed OTP `.beam` files"
        );
        return;
    };

    // Positive control: `erl_eval` is the canonical stdlib site of the `is_record/1`
    // guard (opcode 186).
    let erl_eval_path = stdlib.join("erl_eval.beam");
    if !erl_eval_path.is_file() {
        eprintln!(
            "SKIP: {} not found — the native-record lowering check needs the installed \
             stdlib `erl_eval.beam`",
            erl_eval_path.display()
        );
        return;
    }
    let erl_eval = beam_loader::load_file(&erl_eval_path).expect("erl_eval.beam must load");
    let control = census(&erl_eval);
    assert!(
        control.typed > 0,
        "erl_eval must contain `is_any_native_record`, lowered to \
         Instr::IsAnyNativeRecord (found none)"
    );
    assert_eq!(
        control.generic, 0,
        "erl_eval must not leave any native-record instruction on the Instr::Generic fallback"
    );

    // Regression sweep: no stdlib/kernel/compiler module may still carry the `Generic`
    // form of any family member.
    let mut typed_total = 0usize;
    let mut generic_total = 0usize;
    let mut module_count = 0usize;
    let mut generic_modules: Vec<String> = Vec::new();
    for app in ["stdlib", "kernel", "compiler"] {
        let Some(ebin) = app_ebin(app) else {
            eprintln!("SKIP: {app} ebin not found — sweeping the remaining applications only");
            continue;
        };
        for path in beam_files(&ebin) {
            let module = beam_loader::load_file(&path)
                .unwrap_or_else(|error| panic!("{} must load: {error:?}", path.display()));
            let census = census(&module);
            module_count += 1;
            typed_total += census.typed;
            generic_total += census.generic;
            if census.generic > 0 {
                generic_modules.push(path.display().to_string());
            }
        }
    }

    println!(
        "SWEEP: native-record family — {typed_total} typed site(s) over {module_count} module(s), \
         {generic_total} on the Generic fallback"
    );
    assert_eq!(
        generic_total, 0,
        "no native-record family opcode may lower to Instr::Generic; still generic in: \
         {generic_modules:?}"
    );
    assert!(
        typed_total >= control.typed,
        "the sweep must see at least the erl_eval `is_any_native_record` site it counted"
    );
}

/// The real-`.beam` positive control for the family beyond opcode 186: a module pair
/// using the OTP-29 native-record syntax, compiled by the system `erlc`, must lower
/// 186-191 to their typed variants with none on the `Generic` fallback.
///
/// Compilation failure (no `erlc`, or a compiler without native-record syntax) skips
/// **loudly** — it never fails in that case.
#[test]
fn compiled_native_records_lower_typed() {
    // The definition module exports its native record; the consumer imports it, so the
    // consumer emits the cross-module forms (`is_record_accessible`, `get_record_field`
    // with a `{Module,Name}` literal `Id`).
    const DEF: &str = r#"-module(nr_def).
-export([mk/0]).
-export_record([rec]).
-record #rec{a, b}.
mk() -> #rec{a = 1, b = 2}.
"#;
    const USE: &str = r#"-module(nr_use).
-export([local_mk/0, local_peek/1, local_fld/1, def_mk/0, def_upd/1, def_fld/1]).
-import_record(nr_def, [rec]).
-record #local{a, b}.

local_mk() -> #local{a = 1, b = 2}.
local_peek(R) -> #local{a = A, b = B} = R, {A, B}.
local_fld(R) -> R#local.a.
def_mk() -> #recdef:rec{a = 1, b = 2}.
def_upd(R) -> R#recdef:rec{a = 7}.
def_fld(R) -> R#recdef:rec.a.
"#;

    let dir = unique_temp_dir("native_record");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!("SKIP: cannot create {}: {error}", dir.display());
        return;
    }
    let def = dir.join("nr_def.erl");
    let use_ = dir.join("nr_use.erl");
    for (path, source) in [(&def, DEF), (&use_, USE)] {
        if let Err(error) = std::fs::write(path, source) {
            eprintln!("SKIP: cannot write {}: {error}", path.display());
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    }

    let output = match Command::new("erlc")
        .arg("+debug_info")
        .arg("-o")
        .arg(&dir)
        .arg(&def)
        .arg(&use_)
        // Keep the compiler's cwd out of the source tree (it writes `.beam` next to
        // `-o`, but any stray output must land in the temp dir regardless).
        .current_dir(&dir)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            eprintln!("SKIP: cannot run `erlc`: {error} — need the system OTP compiler");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
    };
    if !output.status.success() {
        eprintln!(
            "SKIP: `erlc` rejected the native-record source ({}): {} — this compiler has no \
             native-record syntax",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    let mut total = Census::default();
    let mut typed_variants = Vec::new();
    for name in ["nr_def", "nr_use"] {
        let beam = dir.join(format!("{name}.beam"));
        let module = match beam_loader::load_file(&beam) {
            Ok(module) => module,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("{} must load: {error:?}", beam.display());
            }
        };
        total.add(&census(&module));
        typed_variants.extend(variant_names(&module));
    }
    let _ = std::fs::remove_dir_all(&dir);

    println!(
        "COMPILED: native-record family — {} typed site(s), {} on the Generic fallback; \
         variants: {typed_variants:?}",
        total.typed, total.generic
    );
    assert_eq!(
        total.generic, 0,
        "the compiled native-record module must not leave any family member on Instr::Generic"
    );
    // The source is chosen to emit the whole OTP-29 subset; each variant must appear.
    for expected in [
        "IsAnyNativeRecord",
        "IsNativeRecord",
        "GetRecordElements",
        "PutRecord",
        "IsRecordAccessible",
        "GetRecordField",
    ] {
        assert!(
            typed_variants.iter().any(|name| name == expected),
            "the compiled module must contain {expected} (found {typed_variants:?})"
        );
    }
}

/// The names of the family's typed variants that occur in `module`, name-sorted.
fn variant_names(module: &Module) -> Vec<String> {
    let mut names = Vec::new();
    for function in &module.functions {
        for instr in &function.code {
            let name = match instr {
                Instr::IsAnyNativeRecord { .. } => "IsAnyNativeRecord",
                Instr::IsNativeRecord { .. } => "IsNativeRecord",
                Instr::GetRecordElements { .. } => "GetRecordElements",
                Instr::PutRecord { .. } => "PutRecord",
                Instr::IsRecordAccessible { .. } => "IsRecordAccessible",
                Instr::GetRecordField { .. } => "GetRecordField",
                Instr::GetRecordElementsId { .. } => "GetRecordElementsId",
                Instr::UpdateRecordId { .. } => "UpdateRecordId",
                _ => continue,
            };
            names.push(name.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// A fresh, unique directory under the system temp dir for the compiled fixture.
fn unique_temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("beam_loader_{tag}_{}_{nanos}", std::process::id()))
}
