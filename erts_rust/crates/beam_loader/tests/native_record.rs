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
//! * **The committed oracle fixture pair `nr_def.beam`/`nr_use.beam`** (compiled by
//!   the system `erlc` from `tests/fixtures/nr_def.erl`/`nr_use.erl`) lowers
//!   **186-191** to their typed variants with **none** on the `Instr::Generic`
//!   fallback, and the module's `Recs` table resolves the native records
//!   (`#rec`, `#local`) through [`Module::records`]/[`Module::record_by_name`].
//! * **`erl_eval` (stdlib, the canonical site of 186) actually contains
//!   `is_any_native_record`**, and it arrives typed — a positive control, so a sweep
//!   that found nothing could not pass vacuously.
//! * **No module in the stdlib/kernel/compiler sweep still carries a `Generic` form
//!   of any family member.**
//!
//! # Honest coverage
//!
//! The installed **stdlib/kernel/compiler** `.beam` files only ever contain opcode
//! **186** — the `is_record/1` guard, emitted for `erlang:is_record/1` with sites in
//! `erl_eval`/`io_lib`/`io_lib_pretty`. The other seven are emitted only for *native
//! records*, which no shipped OTP module uses, so the sweep is a no-regression guard
//! for 187-193; the committed fixture pair is the positive control that actually
//! exercises their lowering. Opcodes **192/193 were added in OTP 30** and are absent
//! from the installed OTP-29 `beam_opcodes` table, so no system `erlc` can emit them —
//! their lowering is covered by the crate unit tests (`src/lower/instr.rs`) only.
//!
//! # Skip-vs-assert policy
//!
//! The installed-corpus test skips **loudly** when the system OTP `.beam` files cannot
//! be located; the committed-fixture test needs no OTP at all (the `.beam` files are
//! checked in).

use std::path::{Path, PathBuf};
use std::process::Command;

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

// ---------------------------------------------------------------------------
// The committed fixtures
// ---------------------------------------------------------------------------

/// Loads the committed `nr_def.beam` native-record-defining oracle fixture.
fn load_nr_def() -> Module {
    beam_loader::load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/nr_def.beam"
    ))
    .expect("the nr_def oracle fixture must load")
}

/// Loads the committed `nr_use.beam` native-record-consuming oracle fixture.
fn load_nr_use() -> Module {
    beam_loader::load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/nr_use.beam"
    ))
    .expect("the nr_use oracle fixture must load")
}

/// Every lowered instruction of `module`, in function/stream order.
fn instructions(module: &Module) -> Vec<&Instr> {
    module
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .collect()
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

/// The positive control for the whole OTP-29 subset (186-191): the committed, real
/// `erlc`-compiled fixture pair `nr_def.beam`/`nr_use.beam` must lower 186-191 to their
/// typed variants with none on the `Generic` fallback, and the native records they
/// define must resolve through [`Module::records`]/[`Module::record_by_name`].
#[test]
fn committed_native_records_lower_typed() {
    let def = load_nr_def();
    let use_ = load_nr_use();

    // No family member may leak through as `Instr::Generic`, in either module.
    for (name, module) in [("nr_def", &def), ("nr_use", &use_)] {
        let census = census(module);
        assert_eq!(
            census.generic, 0,
            "{name} must not leave any native-record instruction on Instr::Generic"
        );
    }

    let mut typed_variants = variant_names(&def);
    typed_variants.extend(variant_names(&use_));
    typed_variants.sort();
    typed_variants.dedup();
    println!("COMPILED: native-record family typed variants: {typed_variants:?}");

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
            "the committed fixtures must contain {expected} (found {typed_variants:?})"
        );
    }

    // Operand-level spot checks, so a variant that merely *exists* is not enough.
    let use_instrs = instructions(&use_);

    // `is_any_native_record` (the `#local{...} = R` match in `local_peek/1`).
    let any = use_instrs
        .iter()
        .find_map(|instr| match instr {
            Instr::IsAnyNativeRecord { src, .. } => Some(src.clone()),
            _ => None,
        })
        .expect("nr_use contains `is_any_native_record`");
    assert_eq!(any, beam_loader::Operand::X(0));

    // `is_record_accessible` with the `external` scope atom (`def_upd/1`, the
    // cross-module `#recdef:rec{...}` update).
    let scope = use_instrs
        .iter()
        .find_map(|instr| match instr {
            Instr::IsRecordAccessible { scope, .. } => Some(scope.clone()),
            _ => None,
        })
        .expect("nr_use contains `is_record_accessible`");
    match scope {
        beam_loader::Operand::Atom(atom) => {
            assert_eq!(use_.atom_name(atom), Some("external"));
        }
        other => panic!("`is_record_accessible` scope must be an atom, got {other:?}"),
    }

    // The native records themselves resolve through the `Recs` table.
    assert_record(&def, "nr_def", "rec", true);
    assert_record(&use_, "nr_use", "local", false);
}

/// Asserts `module` defines exactly one native record `record_name` (exported iff
/// `is_exported`), whose fields are `a`, `b` with no defaults — the declaration order
/// of the `#rec{a, b}` / `#local{a, b}` sources.
fn assert_record(module: &Module, module_name: &str, record_name: &str, is_exported: bool) {
    assert_eq!(
        module.records().len(),
        1,
        "{module_name} must define exactly one native record"
    );

    let record = module
        .record_by_name(record_name)
        .unwrap_or_else(|| panic!("{module_name} must define a native record `{record_name}`"));
    assert_eq!(record.is_exported, is_exported);

    let fields: Vec<Option<&str>> = record
        .fields
        .iter()
        .map(|field| module.atom_name(field.name))
        .collect();
    assert_eq!(fields, vec![Some("a"), Some("b")]);
    assert!(
        record.fields.iter().all(|field| field.default.is_none()),
        "`#rec{{a, b}}` / `#local{{a, b}}` declare no field defaults"
    );

    // `record/1` (by module-local atom) agrees with `record_by_name/1`.
    let atom = module
        .atom_table
        .lookup(record_name)
        .expect("the record name is interned");
    assert!(module.record(atom).is_some());
}
