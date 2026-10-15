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
//! Tier-equivalence and structural pins for the **operand / Y-window axis** of the
//! baseline emitter (`super::emit::{operands, r#move, aggregate, arith, regalloc}`). A
//! `#[path]` child of `super::tests`, so it shares that module's harness
//! (`with_both_tiers`, `assert_same_outcome`, `body_module`, `build_module`).

use crate::machine::MAX_REGISTER;
use beam_loader::{Instr, Operand};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

/// Classify one `move`'s refusal class — a *diagnostic*, mirroring
/// [`super::emit::move_shape`]'s decision tree so the deopt census' `Move` row can be
/// attributed to the reason it was not lowered. Returns `"inlined"` when the shape is
/// lowered, else a `"<phase>:<kind>"` label.
fn classify_move(src: &Operand, dst: &Operand) -> &'static str {
    if super::emit::move_shape(src, dst).is_some() {
        return "inlined";
    }
    if move_dst_ok(dst) {
        return classify_move_src(src);
    }
    classify_move_dst(dst)
}

/// Whether [`super::emit::move_shape`] accepts the destination. Mirrors its two arms:
/// an in-bound `X(n)`, or an addressable `Y(k)`.
fn move_dst_ok(dst: &Operand) -> bool {
    super::emit::x_register(dst).map_or(false, |x| x < MAX_REGISTER)
        || super::emit::y_register(dst).map_or(false, super::emit::y_addressable)
}

/// The class of a refused `move` **destination**.
fn classify_move_dst(dst: &Operand) -> &'static str {
    if let Some(x) = super::emit::x_register(dst) {
        return if x >= MAX_REGISTER {
            "dst:x-out-of-bound"
        } else {
            "dst:??"
        };
    }
    if let Some(k) = super::emit::y_register(dst) {
        return if super::emit::y_addressable(k) {
            "dst:??"
        } else {
            "dst:y-unaddressable"
        };
    }
    match dst {
        Operand::FloatReg(_) => "dst:float-reg",
        Operand::Literal(_) => "dst:literal",
        Operand::Label(_) => "dst:label",
        Operand::List(_) => "dst:list",
        Operand::Allocation(_) => "dst:allocation",
        Operand::Integer(_) | Operand::Character(_) => "dst:immediate",
        Operand::Atom(_) | Operand::Nil => "dst:immediate",
        _ => "dst:other",
    }
}

/// Classify one `move`'s source when the destination was accepted.
fn classify_move_src(src: &Operand) -> &'static str {
    if let Some(x) = super::emit::x_register(src) {
        return if x >= MAX_REGISTER {
            "src:x-out-of-bound"
        } else {
            "src:??"
        };
    }
    if let Some(k) = super::emit::y_register(src) {
        return if super::emit::y_addressable(k) {
            "src:??"
        } else {
            "src:y-unaddressable"
        };
    }
    if super::emit::constant_word(src).is_some() {
        return "src:??";
    }
    match src {
        Operand::FloatReg(_) => "src:float-reg",
        Operand::Literal(_) => "src:literal",
        Operand::Label(_) => "src:label",
        Operand::List(_) => "src:list",
        Operand::Allocation(_) => "src:allocation",
        Operand::Integer(_) | Operand::Character(_) => "src:large-integer",
        _ => "src:other",
    }
}

/// The committed `*.beam` fixtures (no hardcoded names).
fn fixture_paths() -> Vec<PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
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

/// A bounded sample of the installed OTP `.beam`s (generically discovered), or empty
/// when no usable `erl` is on `PATH`.
fn otp_paths(cap: usize) -> Vec<PathBuf> {
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let Ok(output) = Command::new(&erl)
        .args(["-noshell", "-eval", "io:format(\"~s\",[code:lib_dir()]),halt()."])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
    if !root.is_dir() {
        return Vec::new();
    }
    let mut ebins: Vec<PathBuf> = Vec::new();
    if let Ok(apps) = std::fs::read_dir(&root) {
        for app in apps.flatten() {
            let ebin = app.path().join("ebin");
            if ebin.is_dir() {
                ebins.push(ebin);
            }
        }
    }
    ebins.sort();
    let mut paths: Vec<PathBuf> = Vec::new();
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

/// Attribute every `Instr::Move` in `paths` to its refusal class.
fn move_breakdown(paths: &[PathBuf]) -> BTreeMap<String, u64> {
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    for path in paths {
        let Ok(module) = beam_loader::load_file(path) else {
            continue;
        };
        for function in &module.functions {
            for instr in &function.code {
                let Instr::Move { src, dst } = instr else {
                    continue;
                };
                let class = classify_move(src, dst);
                *counts.entry(class.to_string()).or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Print the `Move` deopt breakdown over the corpus (fixtures + a bounded OTP sample),
/// and assert the classifier is **total**: every instance either lowers or lands in a
/// named refusal class.
#[test]
fn the_move_deopt_breakdown_over_the_corpus_is_printed() {
    let fixtures = fixture_paths();
    let otp = otp_paths(4096);
    let mut all = fixtures.clone();
    all.extend(otp.iter().cloned());

    let fixture_counts = move_breakdown(&fixtures);
    let all_counts = move_breakdown(&all);

    println!("=== Move deopt breakdown ===");
    println!("fixtures={} otp_modules={}", fixtures.len(), otp.len());
    println!("-- fixtures --");
    for (class, count) in &fixture_counts {
        println!("  {class:<24} {count}");
    }
    println!("-- fixtures + OTP --");
    for (class, count) in &all_counts {
        println!("  {class:<24} {count}");
    }

    let total: u64 = all_counts.values().sum();
    let inlined: u64 = all_counts.get("inlined").copied().unwrap_or(0);
    println!("total instances = {total}, inlined = {inlined}");

    assert!(total > 0, "the corpus must contain moves");
    for (class, count) in &all_counts {
        assert!(!class.ends_with("??"), "unclassifiable move: {class} x{count}");
    }
}
