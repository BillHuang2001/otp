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

//! A **test-only refusal census**: *why* the baseline tier's `test` / `select_val` /
//! `select_tuple_arity` inliners refuse the shapes they still refuse, and which BIF
//! names the `bif` / `gc_bif` / `bif0` deopt pool actually covers.
//!
//! The sibling [`super::super::census`] counts *how many* refusals each variant has;
//! this attributes every refusal to the exact predicate / operand shape that caused
//! it, which is the evidence a follow-up inline-set widening is chosen from. It is
//! declared behind `#[cfg(test)]` from [`super::test`], so it is absent from every
//! production build (zero cost by construction).
//!
//! Run with `--nocapture`:
//! `... cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine refusal -- --nocapture`

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::{Instr, Label, Module, Operand, Reg as RegRef, TestKind, NO_FAIL};
use beam_term::MAX_ARITYVAL;

use crate::interpreter::{build_label_map, LabelMap};
use crate::machine::MAX_REGISTER;

use super::super::arith::import_names;
use super::super::operands::{constant_word, x_register, Loc};
use super::{test_plan, test_src};

/// A safety bound on the installed-OTP sample (mirrors the sibling census).
const OTP_MODULE_CAP: usize = 4096;

/// The operand shape name of one operand — what the report prints.
fn shape(op: &Operand) -> String {
    match op {
        Operand::X(n) if *n < MAX_REGISTER => "X(in-bound)".to_string(),
        Operand::X(_) => "X(out-of-bound)".to_string(),
        Operand::Y(_) => "Y".to_string(),
        Operand::Typed {
            reg: RegRef::X(n),
            ..
        } if *n < MAX_REGISTER => "X(in-bound,tr)".to_string(),
        Operand::Typed {
            reg: RegRef::X(_), ..
        } => "X(out-of-bound,tr)".to_string(),
        Operand::Typed {
            reg: RegRef::Y(_), ..
        } => "Y(tr)".to_string(),
        Operand::FloatReg(_) => "FloatReg".to_string(),
        Operand::Literal(_) => "Literal".to_string(),
        Operand::Integer(_) => "Integer".to_string(),
        Operand::Character(_) => "Character".to_string(),
        Operand::Atom(_) => "Atom".to_string(),
        Operand::Nil => "Nil".to_string(),
        Operand::BigInteger(_) => "BigInteger".to_string(),
        Operand::Label(_) => "Label".to_string(),
        Operand::List(_) => "List".to_string(),
        Operand::Allocation(_) => "Allocation".to_string(),
    }
}

/// The `test` kinds the emitter serves through `test_helper.rs::jit_test` (the
/// comparison helpers) rather than by a *static* `jit_step_one` deopt: a comparison
/// that `test_plan` declines is still inlined as a helper call, so it is NOT a static
/// deopt and is excluded from this census (the sibling census agrees — no comparison
/// kind appears in its deopt table).
fn is_comparison(kind: TestKind) -> bool {
    matches!(
        kind,
        TestKind::IsEqExact
            | TestKind::IsNeExact
            | TestKind::IsEq
            | TestKind::IsNe
            | TestKind::IsLt
            | TestKind::IsGe
    )
}

/// Why `test_plan` refuses this `test` (or `None` when it inlines it): the exact
/// predicate / operand shape, classified after the plan declined.
fn test_refusal(
    module: &Module,
    labels: &LabelMap,
    kind: TestKind,
    fail: Label,
    args: &[Operand],
) -> Option<String> {
    if test_plan(module, kind, fail, args, labels).is_some() {
        return None;
    }
    if fail == NO_FAIL {
        return Some("fail=NO_FAIL".to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some("unresolved fail label".to_string());
    }
    match kind {
        TestKind::IsEqExact
        | TestKind::IsNeExact
        | TestKind::IsEq
        | TestKind::IsNe
        | TestKind::IsLt
        | TestKind::IsGe => match (args.first(), args.get(1)) {
            (Some(a), Some(b)) => Some(format!("comparison operands: {} + {}", shape(a), shape(b))),
            _ => Some("comparison operand count".to_string()),
        },
        TestKind::IsBoolean => match args.first() {
            Some(op) if test_src(op).is_none() => Some(format!("operand: {}", shape(op))),
            Some(_) => Some("no true/false atom interned".to_string()),
            None => Some("missing operand".to_string()),
        },
        // Every remaining kind reads `args[0]` through `test_src`.
        _ => {
            match args.first() {
                Some(op) if test_src(op).is_none() => {
                    return Some(format!("operand: {}", shape(op)));
                }
                Some(_) => {}
                None => return Some("missing operand".to_string()),
            }
            match kind {
                TestKind::TestArity | TestKind::IsFunction2 => arity_reason(kind, args),
                TestKind::IsTaggedTuple => arity_reason(kind, args),
                _ => Some("other".to_string()),
            }
        }
    }
}

/// The arity/tag-operand reason of an arity-shaped `test` whose `args[0]` is already
/// known loadable.
fn arity_reason(kind: TestKind, args: &[Operand]) -> Option<String> {
    let max = if kind == TestKind::IsFunction2 {
        0xFF
    } else {
        MAX_ARITYVAL as i64
    };
    match args.get(1) {
        Some(Operand::Integer(a)) => {
            if *a < 0 || *a > max {
                return Some(format!("arity operand out of range: {a}"));
            }
            if kind == TestKind::IsTaggedTuple {
                if *a == 0 {
                    return Some("is_tagged_tuple arity 0".to_string());
                }
                match args.get(2) {
                    Some(tag) if test_src(tag).is_none() => {
                        return Some(format!("tag operand: {}", shape(tag)));
                    }
                    Some(_) => return Some("other".to_string()),
                    None => return Some("missing tag operand".to_string()),
                }
            }
            Some("other".to_string())
        }
        Some(op) => Some(format!("arity operand: {}", shape(op))),
        None => Some("missing arity operand".to_string()),
    }
}

/// Why the `select_val` plan refuses this instruction, or `None` when it inlines it.
fn select_val_refusal(
    labels: &LabelMap,
    value: &Operand,
    fail: Label,
    pairs: &[(Operand, Label)],
) -> Option<String> {
    if fail == NO_FAIL {
        return Some("fail=NO_FAIL".to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some("unresolved fail label".to_string());
    }
    if test_src(value).is_none() {
        return Some(format!("scrutinee: {}", shape(value)));
    }
    for (candidate, label) in pairs {
        if constant_word(candidate).is_none() {
            return Some(format!("candidate: {}", shape(candidate)));
        }
        if labels.resolve(*label).is_err() {
            return Some("unresolved case label".to_string());
        }
    }
    None
}

/// Why the `select_tuple_arity` plan refuses this instruction, or `None` when it
/// inlines it.
fn select_tuple_arity_refusal(
    labels: &LabelMap,
    tuple: &Operand,
    fail: Label,
    destinations: &[(Operand, Label)],
) -> Option<String> {
    if fail == NO_FAIL {
        return Some("fail=NO_FAIL".to_string());
    }
    if labels.resolve(fail).is_err() {
        return Some("unresolved fail label".to_string());
    }
    // The scrutinee must be an addressable `Loc` (an in-bound x-register or an
    // addressable `Y` slot) — exactly what `select_tuple_arity_plan` accepts.
    if Loc::of(tuple).is_none() {
        return Some(match x_register(tuple) {
            Some(_) => "tuple X out-of-bound".to_string(),
            None => format!("tuple: {}", shape(tuple)),
        });
    }
    for (candidate, label) in destinations {
        match candidate {
            Operand::Integer(n) if *n >= 0 => {}
            Operand::Integer(_) => return Some("negative arity candidate".to_string()),
            other => return Some(format!("arity candidate: {}", shape(other))),
        }
        if labels.resolve(*label).is_err() {
            return Some("unresolved case label".to_string());
        }
    }
    None
}

/// The `(module,function,arity)` of an import, or a placeholder.
fn bif_name(module: &Module, bif: beam_loader::ImportRef) -> String {
    match import_names(module, bif) {
        Some((m, f, a)) => format!("{m}:{f}/{a}"),
        None => "<unresolved import>".to_string(),
    }
}

/// Whether `name` is one of the BIFs the tier already inlines (the eight comparison
/// ops via `Instr::Bif`/`GcBif`, and `erlang:'+'/2`/`'-'/2`/`'*'/2` via `GcBif`).
fn already_inlined(name: &str) -> bool {
    matches!(
        name,
        "erlang:+/2"
            | "erlang:-/2"
            | "erlang:*/2"
            | "erlang:=:=/2"
            | "erlang:=/=/2"
            | "erlang:==/2"
            | "erlang:/=/2"
            | "erlang:</2"
            | "erlang:=</2"
            | "erlang:>/2"
            | "erlang:>=/2"
    )
}

/// Every committed `*.beam` fixture (no hardcoded names).
fn fixture_corpus() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("beam") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
}

/// A bounded sample of the installed OTP `.beam`s (no hardcoded app names).
fn otp_corpus(cap: usize) -> Vec<PathBuf> {
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let Ok(output) = Command::new(&erl)
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return Vec::new();
    }
    let mut ebins: Vec<PathBuf> = Vec::new();
    let Ok(apps) = std::fs::read_dir(Path::new(&root)) else {
        return Vec::new();
    };
    for app in apps.flatten() {
        let ebin = app.path().join("ebin");
        if ebin.is_dir() {
            ebins.push(ebin);
        }
    }
    ebins.sort();
    let mut paths = Vec::new();
    'outer: for ebin in ebins {
        let mut beams: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&ebin) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("beam") {
                    beams.push(path);
                }
            }
        }
        beams.sort();
        for path in beams {
            if paths.len() >= cap {
                break 'outer;
            }
            paths.push(path);
        }
    }
    paths
}

/// The refusal census over the fixture + OTP corpus.
#[test]
fn the_refusal_attribution_census_is_printed() {
    let mut paths = fixture_corpus();
    paths.extend(otp_corpus(OTP_MODULE_CAP));

    // kind -> reason -> count
    let mut tests: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut selects: BTreeMap<String, u64> = BTreeMap::new();
    let mut tuple_arities: BTreeMap<String, u64> = BTreeMap::new();
    let mut bifs: BTreeMap<String, u64> = BTreeMap::new();
    let mut gcbifs: BTreeMap<String, u64> = BTreeMap::new();
    let mut bif0s: BTreeMap<String, u64> = BTreeMap::new();
    let mut modules_loaded = 0u64;

    for path in &paths {
        let Ok(module) = beam_loader::load_file(path) else {
            continue;
        };
        modules_loaded += 1;
        let labels = build_label_map(&module);
        for function in &module.functions {
            for instr in &function.code {
                match instr {
                    Instr::Test { kind, fail, args } => {
                        if !is_comparison(*kind) {
                        if let Some(reason) =
                            test_refusal(&module, &labels, *kind, *fail, args)
                        {
                            *tests
                                .entry(format!("Test({kind:?})"))
                                .or_default()
                                .entry(reason)
                                .or_insert(0) += 1;
                        }
                        }
                    }
                    Instr::SelectVal { value, fail, pairs } => {
                        if let Some(reason) = select_val_refusal(&labels, value, *fail, pairs) {
                            *selects.entry(reason).or_insert(0) += 1;
                        }
                    }
                    Instr::SelectTupleArity {
                        tuple,
                        fail,
                        destinations,
                    } => {
                        if let Some(reason) =
                            select_tuple_arity_refusal(&labels, tuple, *fail, destinations)
                        {
                            *tuple_arities.entry(reason).or_insert(0) += 1;
                        }
                    }
                    Instr::Bif { bif, .. } => {
                        let name = bif_name(&module, *bif);
                        if !already_inlined(&name) {
                            *bifs.entry(name).or_insert(0) += 1;
                        }
                    }
                    Instr::GcBif { bif, .. } => {
                        let name = bif_name(&module, *bif);
                        if !already_inlined(&name) {
                            *gcbifs.entry(name).or_insert(0) += 1;
                        }
                    }
                    Instr::Bif0 { bif, .. } => {
                        *bif0s.entry(bif_name(&module, *bif)).or_insert(0) += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    assert!(modules_loaded > 0, "the corpus must load at least one module");
    println!("=== baseline-JIT refusal-attribution census ===");
    println!("corpus: paths={} modules_loaded={modules_loaded}", paths.len());

    println!();
    println!("-- `test` refusals: kind | reason | count --");
    let mut test_rows: Vec<(String, String, u64)> = Vec::new();
    for (kind, reasons) in &tests {
        for (reason, count) in reasons {
            test_rows.push((kind.clone(), reason.clone(), *count));
        }
    }
    test_rows.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    let test_total: u64 = test_rows.iter().map(|r| r.2).sum();
    println!("total test refusals = {test_total}");
    for (kind, reason, count) in &test_rows {
        println!("{count:>7}  {kind:<24} {reason}");
    }

    println!();
    println!("-- `select_val` refusals: reason | count --");
    let mut sel_rows: Vec<(String, u64)> = selects.into_iter().collect();
    sel_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (reason, count) in &sel_rows {
        println!("{count:>7}  {reason}");
    }

    println!();
    println!("-- `select_tuple_arity` refusals: reason | count --");
    let mut sta_rows: Vec<(String, u64)> = tuple_arities.into_iter().collect();
    sta_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (reason, count) in &sta_rows {
        println!("{count:>7}  {reason}");
    }

    println!();
    println!("-- `bif` (non-gc) deopt pool by BIF name --");
    let mut bif_rows: Vec<(String, u64)> = bifs.into_iter().collect();
    bif_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let bif_total: u64 = bif_rows.iter().map(|r| r.1).sum();
    println!("total = {bif_total}");
    for (name, count) in bif_rows.iter().take(40) {
        println!("{count:>7}  {name}");
    }

    println!();
    println!("-- `gc_bif` deopt pool by BIF name --");
    let mut gc_rows: Vec<(String, u64)> = gcbifs.into_iter().collect();
    gc_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let gc_total: u64 = gc_rows.iter().map(|r| r.1).sum();
    println!("total = {gc_total}");
    for (name, count) in gc_rows.iter().take(40) {
        println!("{count:>7}  {name}");
    }

    println!();
    println!("-- `bif0` deopt pool by BIF name (no arm at all) --");
    let mut b0_rows: Vec<(String, u64)> = bif0s.into_iter().collect();
    b0_rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (name, count) in b0_rows.iter().take(40) {
        println!("{count:>7}  {name}");
    }
}
