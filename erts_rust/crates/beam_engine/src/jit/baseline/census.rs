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

//! The **baseline-JIT deopt census** — a test-only diagnostic that ranks which
//! `beam_loader::Instr` opcodes the native baseline tier still fails to lower.
//!
//! Widening the JIT's inline set is only worth doing for the opcodes that actually
//! deopt on real code, so the choice needs *evidence*, not intuition. This module
//! produces it, in two independent but complementary views:
//!
//! 1. **Static census** ([`the_static_deopt_census_over_the_corpus_is_printed`]) —
//!    *deterministic and offline*: compile a corpus (the crate's committed `erlc`
//!    fixtures plus a bounded sample of the installed OTP `.beam`s) through the
//!    baseline emitter with [`super::counters`] recording, and count, per instruction
//!    variant, how many instruction *instances* the lowering dispatch visited versus
//!    how many it did **not** lower. This is the compile-time (shape) decision,
//!    independent of any run, and is therefore the primary evidence for which shapes a
//!    follow-up inline-set widening should target.
//! 2. **Runtime census** ([`the_runtime_deopt_census_over_the_hot_shapes_is_printed`]) —
//!    *dynamic cross-check*: run the in-crate tier workloads
//!    ([`crate::jit::bench::assert_tier_agreement`]) with recording on and dump the
//!    per-variant [`Counters::deopts_by_variant`](super::counters::Counters) the
//!    executed instructions produced. This shows which un-lowered opcodes dominate the
//!    **hot** shapes (where a deopt actually costs run time, e.g. once per loop
//!    iteration), which the static view cannot know.
//!
//! Both views assert only **liveness/consistency** — never a deopt *threshold* — so
//! they keep passing as the inline set changes; the printed rankings are the evidence.
//! Every number is attributed by [`super::counters::variant_name`] (a `Test` is keyed
//! by its kind), and a deopt here is a *compile-time* refusal: the run-time guard
//! fallbacks emitted **inside** a `try_inline_*` computation (an inlined shape whose
//! operand guard rejects) are the runtime view's business, not this one's.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::counters;
use super::emit::compile_function;

/// A safety bound on how many installed-OTP `.beam`s the static census samples.
///
/// The point is a *representative* corpus that stays fast. The default comfortably
/// covers the whole installed OTP tree (a few thousand modules compile in seconds), so
/// the census sees the whole real opcode mix; lower it (or set it to the fixture count)
/// if a run of the census needs to be cheaper. The fixture part always runs regardless.
const OTP_MODULE_CAP: usize = 4096;

/// How many rows each printed ranking shows (the tables themselves stay complete in
/// the machine-readable dump).
const PRINT_ROWS: usize = 60;

/// One census row: a variant's static deopt/instance counts.
struct Row {
    /// The variant name ([`super::counters::variant_name`]).
    variant: String,
    /// Instructions of this variant the dispatch did **not** lower.
    deopts: u64,
    /// Instructions of this variant the dispatch visited (the denominator).
    instances: u64,
}

impl Row {
    /// The fraction of this variant's visited instances the dispatch deopted
    /// (`0.0` when it was never visited).
    fn deopt_ratio(&self) -> f64 {
        if self.instances == 0 {
            0.0
        } else {
            self.deopts as f64 / self.instances as f64
        }
    }

    /// The **impact score** of this variant: its deopt count weighted by the cost of a
    /// deopt at that opcode (see [`deopt_weight`]).
    fn score(&self) -> u64 {
        self.deopts * deopt_weight(&self.variant)
    }
}

/// The per-deopt **cost weight** of an opcode, by variant name.
///
/// A deopt is not equally expensive everywhere. `Jump`, `SelectVal` and
/// `SelectTupleArity` are the **block-terminating dispatch** opcodes: their interpreter
/// arm unconditionally transfers control (a `Step::Goto` on every path), so a block that
/// cannot lower one leaves for the driver *and* forfeits the direct block-to-block link
/// the tier would otherwise emit for it — the tier loses the whole chain of successor
/// blocks behind that branch, not just the one instruction. Every other opcode deopts
/// mid-block and the block continues natively at `pc + 1` after a single interpreter
/// step, so its marginal cost is one step. A weight of **4** expresses that a
/// dispatch-structure deopt is worth several ordinary ones; the exact value is a
/// judgment call (the ranking is robust to anything above 1), and both the raw count
/// ranking and the score ranking are printed so the reader can see the effect.
fn deopt_weight(variant: &str) -> u64 {
    match variant {
        "Jump" | "SelectVal" | "SelectTupleArity" => 4,
        _ => 1,
    }
}

/// The union of the instance and deopt maps, one [`Row`] per variant that either map
/// names (a variant with instances but no deopts is a **fully inlined** shape).
fn census_rows(
    instances: &BTreeMap<String, u64>,
    deopts: &BTreeMap<String, u64>,
) -> Vec<Row> {
    let mut names: BTreeSet<&String> = BTreeSet::new();
    names.extend(instances.keys());
    names.extend(deopts.keys());
    names
        .into_iter()
        .map(|variant| Row {
            variant: variant.clone(),
            deopts: deopts.get(variant).copied().unwrap_or(0),
            instances: instances.get(variant).copied().unwrap_or(0),
        })
        .collect()
}

/// Rank rows by deopts descending, tie-break by variant name ascending.
fn rank_by_deopts(rows: &[Row]) -> Vec<&Row> {
    let mut ranked: Vec<&Row> = rows.iter().filter(|row| row.deopts > 0).collect();
    ranked.sort_by(|a, b| b.deopts.cmp(&a.deopts).then(a.variant.cmp(&b.variant)));
    ranked
}

/// Rank rows by [`Row::score`] descending, tie-break by variant name ascending.
fn rank_by_score(rows: &[Row]) -> Vec<&Row> {
    let mut ranked: Vec<&Row> = rows.iter().filter(|row| row.score() > 0).collect();
    ranked.sort_by(|a, b| {
        b.score()
            .cmp(&a.score())
            .then(a.variant.cmp(&b.variant))
    });
    ranked
}

/// Every committed `*.beam` fixture, discovered by scanning the crate's
/// `tests/fixtures` directory — **no hardcoded names**, so a newly added fixture joins
/// the census automatically.
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

/// The OTP lib root reported by the system `erl` (`code:lib_dir()`), or `None` when no
/// usable `erl` is on `PATH` (then the OTP part of the static census is skipped loudly).
///
/// Honours `ERTS_ORACLE` (the differential harness' oracle override) before falling
/// back to `erl`.
fn otp_lib_root() -> Option<PathBuf> {
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let output = Command::new(&erl)
        .args(["-noshell", "-eval", "io:format(\"~s\",[code:lib_dir()]),halt()."])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }
    let path = PathBuf::from(root);
    path.is_dir().then_some(path)
}

/// A bounded sample of the installed OTP `.beam`s: scan every `<lib>/<app>-<vsn>/ebin`
/// directory (discovered generically, **no hardcoded app names**) in sorted order and
/// take up to `cap` of the `.beam`s found.
fn otp_corpus(lib_root: &Path, cap: usize) -> Vec<PathBuf> {
    let mut ebins: Vec<PathBuf> = Vec::new();
    if let Ok(apps) = std::fs::read_dir(lib_root) {
        for app in apps.flatten() {
            let ebin = app.path().join("ebin");
            if ebin.is_dir() {
                ebins.push(ebin);
            }
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

/// Write one census section to a compact machine-readable TSV in the temp directory
/// (`$TMPDIR`).
///
/// A failure to write is reported (never silent) but does not fail the test — the
/// printed output is the evidence; this is a convenience copy. The two sections live in
/// two files because the two census tests are independent.
fn write_dump(section: &str, header: &str, body: String) -> Option<PathBuf> {
    let mut out = String::new();
    out.push_str("# baseline-JIT deopt census\n");
    out.push_str(header);
    out.push_str(&body);
    let path = std::env::temp_dir().join(format!("beam_engine_deopt_census_{section}.tsv"));
    match std::fs::write(&path, out) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("deopt census: could not write {}: {error}", path.display());
            None
        }
    }
}

/// The **static** deopt census: which instruction shapes the lowering dispatch refuses,
/// counted over a corpus of committed fixtures plus sampled OTP modules.
///
/// Run with `--nocapture` to read the tables:
/// `... cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine census -- --nocapture`
#[test]
fn the_static_deopt_census_over_the_corpus_is_printed() {
    crate::jit::cache::clear_baseline_cache();
    counters::reset();
    counters::set_enabled(true);

    let fixtures = fixture_corpus();
    let fixture_count = fixtures.len();
    let mut paths = fixtures;

    let otp = otp_lib_root();
    let otp_count = match &otp {
        Some(root) => {
            let beams = otp_corpus(root, OTP_MODULE_CAP);
            let count = beams.len();
            paths.extend(beams);
            count
        }
        None => {
            eprintln!(
                "deopt census: no usable `erl` on PATH — the OTP corpus is SKIPPED; \
                 only the committed fixtures are censused"
            );
            0
        }
    };

    let mut modules = 0u64;
    let mut compiled = 0u64;
    let mut skipped = 0u64;
    for path in &paths {
        let Ok(module) = beam_loader::load_file(path) else {
            // A module that will not load is not this diagnostic's concern.
            continue;
        };
        modules += 1;
        let labels = crate::interpreter::build_label_map(&module);
        for func in 0..module.functions.len() as u32 {
            match compile_function(&module, &labels, func) {
                Ok(_) => compiled += 1,
                Err(_) => skipped += 1,
            }
        }
    }

    counters::set_enabled(false);
    let snapshot = counters::snapshot();

    let rows = census_rows(&snapshot.instr_instances, &snapshot.instr_deopts);
    let total_instances: u64 = snapshot.instr_instances.values().sum();
    let total_deopts: u64 = snapshot.instr_deopts.values().sum();
    let by_deopts = rank_by_deopts(&rows);
    let by_score = rank_by_score(&rows);

    // --- liveness / consistency (never a threshold) -------------------------
    assert!(
        compiled > 0,
        "the fixture corpus must compile at least one function"
    );
    assert!(
        !by_deopts.is_empty(),
        "the corpus must exhibit at least one deopted instruction shape"
    );
    assert!(
        total_instances >= total_deopts,
        "every deopt is also a visited instance: {total_deopts} > {total_instances}"
    );
    for row in &rows {
        assert!(
            row.deopts <= row.instances,
            "variant {} deopted {} times out of {} instances",
            row.variant,
            row.deopts,
            row.instances,
        );
    }

    // --- the tables ---------------------------------------------------------
    println!("=== baseline-JIT static deopt census ===");
    println!(
        "corpus: fixtures={fixture_count} otp_modules={otp_count} \
         modules_loaded={modules} functions_compiled={compiled} functions_skipped={skipped}"
    );
    println!(
        "totals: instances={total_instances} deopts={total_deopts} \
         distinct_variants={} variants_with_deopts={}",
        rows.len(),
        by_deopts.len(),
    );
    println!(
        "deopt_weight: block-terminating dispatch (Jump/SelectVal/SelectTupleArity)=4, else 1"
    );
    println!();

    println!("-- ranked by deopt count (variant | deopts | instances | deopt_ratio) --");
    for (rank, row) in by_deopts.iter().take(PRINT_ROWS).enumerate() {
        println!(
            "{:>3}  {:<24} deopts={:<8} instances={:<9} ratio={:.4}",
            rank + 1,
            row.variant,
            row.deopts,
            row.instances,
            row.deopt_ratio(),
        );
    }
    if by_deopts.len() > PRINT_ROWS {
        println!("     ... ({} more variants)", by_deopts.len() - PRINT_ROWS);
    }
    println!();

    println!("-- ranked by score (deopts * weight) (variant | deopts | weight | score) --");
    for (rank, row) in by_score.iter().take(PRINT_ROWS).enumerate() {
        println!(
            "{:>3}  {:<24} deopts={:<8} weight={:<2} score={}",
            rank + 1,
            row.variant,
            row.deopts,
            deopt_weight(&row.variant),
            row.score(),
        );
    }
    if by_score.len() > PRINT_ROWS {
        println!("     ... ({} more variants)", by_score.len() - PRINT_ROWS);
    }

    let body: String = by_deopts
        .iter()
        .map(|row| {
            format!(
                "{}\t{}\t{}\t{:.4}\t{}\n",
                row.variant,
                row.deopts,
                row.instances,
                row.deopt_ratio(),
                row.score(),
            )
        })
        .collect();
    let header = "# static: variant\tdeopts\tinstances\tdeopt_ratio\tscore\n";
    if let Some(path) = write_dump("static", header, body) {
        println!("machine-readable dump: {}", path.display());
    }
}

/// The **runtime** deopt census: which un-lowered opcodes dominate the *hot* in-crate
/// shapes.
///
/// The baseline tier is the recording side (the interpreter records nothing), so this
/// dumps exactly the deopts the executed natively-compiled blocks produced — the
/// dynamic cross-check of the static table above.
#[test]
fn the_runtime_deopt_census_over_the_hot_shapes_is_printed() {
    crate::jit::cache::clear_baseline_cache();
    counters::reset();
    counters::set_enabled(true);
    crate::jit::bench::assert_tier_agreement();
    counters::set_enabled(false);
    let snapshot = counters::snapshot();

    let mut ranked: Vec<(String, u64)> = snapshot
        .deopts_by_variant
        .iter()
        .map(|(variant, count)| (variant.clone(), *count))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let total: u64 = ranked.iter().map(|(_, count)| *count).sum();
    assert!(
        !ranked.is_empty(),
        "the hot shapes must produce at least one run-time deopt"
    );

    println!("=== baseline-JIT runtime deopt census (hot in-crate shapes) ===");
    println!(
        "step_one={} step_ext={} total_deopts={} distinct_variants={}",
        snapshot.step_one,
        snapshot.step_ext,
        total,
        ranked.len(),
    );
    println!("-- ranked by run-time deopts (variant | deopts) --");
    for (rank, (variant, count)) in ranked.iter().take(PRINT_ROWS).enumerate() {
        println!("{:>3}  {:<24} deopts={count}", rank + 1, variant);
    }
    if ranked.len() > PRINT_ROWS {
        println!("     ... ({} more variants)", ranked.len() - PRINT_ROWS);
    }

    let body: String = ranked
        .iter()
        .map(|(variant, count)| format!("{variant}\t{count}\n"))
        .collect();
    let header = "# runtime: variant\tdeopts\n";
    if let Some(path) = write_dump("runtime", header, body) {
        println!("machine-readable dump: {}", path.display());
    }
}
