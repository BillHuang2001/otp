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
//! `beam_loader::Instr` opcodes the native baseline tier still fails to lower — plus the
//! **dynamic per-variant execution census** (how often each variant actually executes)
//! and the **register-cache traffic census** that measures the spill/fill store+load
//! traffic the emitted blocks carry.
//!
//! Widening the JIT's inline set is only worth doing for the opcodes that actually
//! deopt on real code, so the choice needs *evidence*, not intuition. This module
//! produces it, in four independent views:
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
//! 3. **Register-cache traffic census**
//!    ([`the_register_cache_traffic_census_over_the_corpus_is_printed`]) — *deterministic
//!    and offline*, like the static view: compile the same corpus and print, per fixture
//!    module and in total, the spill **store** counts of all three models
//!    ([`spill_stores`](super::counters::Counters::spill_stores) — what `regs::spill`
//!    emits today — the written set, and the full cache), the fill **load** counts
//!    (liveness vs full cache), their per-site splits, and the block cache/write-set
//!    histograms. This is the evidence for whether a finer spill/fill rule is worth
//!    adding, independent of any run.
//! 4. **Dynamic per-variant execution census** ([`crate::exec_census`], printed by the
//!    same [`the_runtime_deopt_census_over_the_hot_shapes_is_printed`] test, immediately
//!    after its deopt ranking) — *dynamic*, over the same in-crate tier workloads but run
//!    on the **interpreter** tier: how many instructions of each `Instr` variant actually
//!    **executed**, and which of them returned a typed error. The deopt view above is the
//!    native tiers' own record of what they refused to lower; this is the executed
//!    population itself, so the two read together as "what ran" versus "what went back to
//!    the interpreter".
//!
//! All four views assert only **liveness/consistency** — never a deopt *threshold* —
//! so they keep passing as the inline set changes; the printed rankings are the evidence.
//!
//! This module is above the ~1000-line guidance because it hosts five self-contained
//! diagnostics (the static and runtime deopt censuses, the dynamic execution census, the
//! per-kind emitted-code-size table and the register-cache traffic census), each with its
//! own helpers and printed tables, that deliberately share **one** corpus-discovery and
//! compilation scaffold (`fixture_corpus`, `otp_lib_root`, `otp_corpus`,
//! `compile_corpus`, `write_dump`) rather than duplicating it.
//! Every number is attributed by a `variant_name` — [`super::counters::variant_name`] for
//! the deopt/code-size/reg-cache views, [`crate::exec_census::variant_name`] for the
//! execution view — each of which keys a `Test` by its kind. A deopt here is a
//! *compile-time* refusal: the run-time guard fallbacks emitted **inside** a
//! `try_inline_*` computation (an inlined shape whose operand guard rejects) are the
//! runtime view's business, not this one's.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::{Instr, Label, Module, Operand, TestKind};

use super::counters::{self, Counters};
use super::emit::{compile_function, compile_module, FillSite, SpillSite};
use super::tests::build_module;

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

/// What compiling one corpus cost: how many modules loaded, how many functions the
/// emitter lowered, and how many it refused.
#[derive(Default)]
struct CorpusTally {
    modules: u64,
    compiled: u64,
    skipped: u64,
}

impl CorpusTally {
    fn add(&mut self, other: &CorpusTally) {
        self.modules += other.modules;
        self.compiled += other.compiled;
        self.skipped += other.skipped;
    }
}

/// Compile every function of every loadable `.beam` in `paths` through the baseline
/// emitter, tallying modules/functions and skipping (never failing on) a module that
/// will not load or a function that will not compile.
fn compile_corpus(paths: &[PathBuf]) -> CorpusTally {
    let mut tally = CorpusTally::default();
    for path in paths {
        let Ok(module) = beam_loader::load_file(path) else {
            // A module that will not load is not this diagnostic's concern.
            continue;
        };
        tally.modules += 1;
        let labels = crate::interpreter::build_label_map(&module);
        for func in 0..module.functions.len() as u32 {
            match compile_function(&module, &labels, func) {
                Ok(_) => tally.compiled += 1,
                Err(_) => tally.skipped += 1,
            }
        }
    }
    tally
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
    // The fixture part is the always-present half — a run without `erl` still censuses
    // it — so it is compiled first and tallied on its own.
    let fixture_tally = compile_corpus(&fixtures);

    let otp = otp_lib_root();
    let (otp_count, otp_tally) = match &otp {
        Some(root) => {
            let beams = otp_corpus(root, OTP_MODULE_CAP);
            (beams.len(), compile_corpus(&beams))
        }
        None => {
            eprintln!(
                "deopt census: no usable `erl` on PATH — the OTP corpus is SKIPPED; \
                 only the committed fixtures are censused"
            );
            (0, CorpusTally::default())
        }
    };

    let fixture_modules = fixture_tally.modules;
    let fixture_functions = fixture_tally.compiled;
    let mut tally = fixture_tally;
    tally.add(&otp_tally);
    let CorpusTally {
        modules,
        compiled,
        skipped,
    } = tally;
    counters::set_enabled(false);
    let snapshot = counters::snapshot();

    let rows = census_rows(&snapshot.instr_instances, &snapshot.instr_deopts);
    let total_instances: u64 = snapshot.instr_instances.values().sum();
    let total_deopts: u64 = snapshot.instr_deopts.values().sum();
    let by_deopts = rank_by_deopts(&rows);
    let by_score = rank_by_score(&rows);

    // --- liveness / consistency (never a threshold) -------------------------
    assert!(
        fixture_functions > 0,
        "the fixture corpus must compile at least one function \
         (fixtures={fixture_count} modules={fixture_modules})"
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
        "corpus: fixtures={fixture_count} (modules={fixture_modules} functions={fixture_functions}) \
         otp_modules={otp_count} (erl_found={})",
        otp.is_some(),
    );
    println!(
        "totals: modules_loaded={modules} functions_compiled={compiled} \
         functions_skipped={skipped}"
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

    // -----------------------------------------------------------------------
    // Dynamic per-variant execution census (the same workloads, interpreter tier)
    // -----------------------------------------------------------------------
    //
    // The honest caveat this section must not let a reader forget lives on
    // `dynamic_execution_census`'s docs: these are the **in-crate tier workloads**, run
    // on the **interpreter** tier, not the OTP corpus — so no natively-inlined
    // instruction is silently missing from the counts.
    let exec = dynamic_execution_census();

    let mut exec_ranked: Vec<(&str, u64)> = exec
        .executed
        .iter()
        .map(|(variant, count)| (variant.as_str(), *count))
        .collect();
    exec_ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

    // --- liveness / consistency (never a threshold) -------------------------
    assert!(
        !exec.executed.is_empty(),
        "the hot shapes must execute at least one instruction through the funnel"
    );
    assert_eq!(
        exec.total_executed,
        exec.executed.values().sum::<u64>(),
        "total_executed must be the sum of the per-variant executed counts"
    );
    assert_eq!(
        exec.total_errors,
        exec.errors.values().sum::<u64>(),
        "total_errors must be the sum of the per-variant error counts"
    );
    for variant in exec.errors.keys() {
        let first = exec.first_error.get(variant).map(String::as_str);
        assert!(
            first.is_some_and(|text| !text.is_empty()),
            "{variant} errored, so it must keep a non-empty first error text: {:?}",
            exec.first_error
        );
    }
    for variant in exec.executed.keys().chain(exec.errors.keys()) {
        assert!(
            !variant.is_empty(),
            "a variant key must never be empty: {:?}",
            exec
        );
    }
    for variant in exec.first_error.keys() {
        assert!(
            !variant.is_empty(),
            "a variant key must never be empty: {:?}",
            exec
        );
    }
    // The ranking is a faithful re-ordering of the map: one row per executed variant, by
    // descending count with ties broken by ascending name, summing to the same total.
    assert_eq!(
        exec_ranked.len(),
        exec.executed.len(),
        "every executed variant must appear exactly once in the ranking"
    );
    assert!(
        exec_ranked
            .windows(2)
            .all(|w| (w[0].1 > w[1].1) || (w[0].1 == w[1].1 && w[0].0 < w[1].0)),
        "the ranking must be by descending count, ties by ascending variant name: {exec_ranked:?}"
    );
    assert_eq!(
        exec_ranked.iter().map(|(_, count)| *count).sum::<u64>(),
        exec.total_executed,
        "the ranking must account for every executed instruction"
    );
    assert!(
        exec_ranked
            .iter()
            .all(|(variant, count)| exec.executed.get(*variant) == Some(count)),
        "every ranked row must agree with the executed map"
    );

    // --- the tables ---------------------------------------------------------
    println!(
        "=== dynamic per-variant execution census (interpreter tier, hot in-crate shapes) ==="
    );
    println!(
        "total_executed={} distinct_variants={} total_errors={}",
        exec.total_executed,
        exec_ranked.len(),
        exec.total_errors,
    );
    println!("-- ranked by executed instructions (variant | executed) --");
    for (rank, (variant, count)) in exec_ranked.iter().take(PRINT_ROWS).enumerate() {
        println!("{:>3}  {:<24} executed={count}", rank + 1, variant);
    }
    if exec_ranked.len() > PRINT_ROWS {
        println!("     ... ({} more variants)", exec_ranked.len() - PRINT_ROWS);
    }
    println!("-- typed errors per variant (variant | errors | first error text) --");
    for (variant, count) in &exec.errors {
        println!(
            "     {:<24} errors={:<6} first={}",
            variant,
            count,
            exec.first_error
                .get(variant)
                .map(String::as_str)
                .unwrap_or("-"),
        );
    }

    // The machine-readable half, in the same spirit as the deopt dump above: a write
    // failure is reported on stderr by `exec_census::write_dump` and must not fail this
    // test.
    if let Some(path) = crate::exec_census::write_dump("dynamic") {
        println!("machine-readable dump: {}", path.display());
    }
}

/// Run the in-crate tier workloads on the **interpreter** tier with
/// [`crate::exec_census`] recording, and return its snapshot — the dynamic *execution*
/// view of the same workload set the deopt ranking above covers.
///
/// **What these counts do and do not cover** (read before quoting them):
///
/// * The workload set is the **in-crate tier workloads** ([`crate::jit::bench`]'s
///   thirteen programs), **not** the whole OTP corpus. They exercise the hot shapes the
///   baseline tier's inline set was judged against, so the ranking is evidence about
///   *those* shapes; the corpus-scale dynamic view needs a driver over real modules (a
///   `tests/` corpus sweep's business, not this file's).
/// * The tier is pinned to **interpreter** on purpose. On a native tier every lowered
///   instruction is emitted as native code and never enters the funnel, so a native run
///   would report only the steps it deopted — which is exactly the population the deopt
///   ranking above already describes. The interpreter executes every instruction through
///   the funnel, so no executed instruction is missing from the counts.
/// * The counts include each workload's warm-up run, since this is a **coverage** view
///   rather than a timing one; the run count is the tier harness' agreement count, so
///   the sample is small and fast.
/// * [`crate::jit::bench::assert_tier_agreement`] is deliberately **not** the driver
///   here: it runs every available tier (twice), which would mix the tiers' instruction
///   mixes into one sample. The driver is
///   [`crate::jit::bench::run_workloads_on`].
fn dynamic_execution_census() -> crate::exec_census::Snapshot {
    crate::exec_census::reset();
    crate::exec_census::enable();
    crate::jit::bench::run_workloads_on(crate::jit::Tier::Interpreter);
    crate::exec_census::disable();
    crate::exec_census::snapshot()
}

// ---------------------------------------------------------------------------
// Per-kind emitted-code-size table
// ---------------------------------------------------------------------------
//
// `counters::record_block` records, once per emitted block, `buffer.len()` at the end
// of the block minus the buffer offset at its start — the block's **whole** emitted
// native size (the prologue's position/Y-window publish store, the cached-register
// loads, the body, the shared exit path and the epilogue's teardown + `ret`) — and
// `Counters::block_emitted_bytes` sums it. Compiling a module whose function body is a
// **single** IR block therefore makes that sum *be* one block's own size, which is the
// measurement this section takes.
//
// The tier's shared per-instruction deopt (`emit::emit_step_and_continue`: spill the
// block's written cache set, `call jit_step_one`, `cmp rax, encode_target(func, pc+1)`
// / `jne` to the epilogue, refill the continuation's live cache set) has a **fixed**
// byte length for any one non-inlined instruction, so the table measures it once with a
// block containing one always-deopting `func_info` (deliberately never inlined — it is
// the failed-clause raise), then reports each inlined shape's block against that
// reference.
//
// Caveats a reader must not over-read:
// * both figures are **whole-block** sizes, so both carry the shared prologue/exit/
//   epilogue. The delta is therefore the honest shape-vs-deopt comparison, but the
//   register cache a shape's operands induce (its fill/spill traffic) may differ by a
//   few bytes from the `func_info` reference's empty cache. The `2N − N` **marginal**
//   (printed where the shape leaves its block open to a second copy) cancels *every*
//   shared byte and is the confound-free per-occurrence figure.
// * a deopt is a **per-occurrence** cost: an artifact-wide "what a deopt-only artifact
//   would cost" figure multiplies each shape's delta by that shape's occurrence count in
//   the artifact, which this table does not do (it measures one occurrence of each
//   shape). The artifact `code_len` below is instead the real, counted value with the
//   lowerings ON.
// * the named-BIF lowerings a sibling task is adding (`hd/1`, `element/2`,
//   `tuple_size/1`, `byte_size/1`, a small∧small `band`/`bor`/`bsr`) are **absent** from
//   this worktree (the dispatch has only the small-int `+`/`-`/`*` `gc_bif` fast path
//   and the comparison BIFs), so no row is reported for them; only the small-int
//   `gc_bif` fast path is present to measure and is out of this table's scope.

/// The atoms of the reference `func_info`: its operands are never read (the emitter
/// deopts the whole instruction), so the index is irrelevant.
const REF_ATOM: beam_term::Atom = beam_term::Atom::MAX;

/// A body of `repeat` always-deopting `func_info`s plus a `return` — the fixed
/// deopt-sequence reference (its `2 − 1` marginal is the per-occurrence deopt cost).
fn body_deopt(repeat: usize) -> Vec<Instr> {
    let mut body = Vec::new();
    for _ in 0..repeat {
        body.push(Instr::FuncInfo {
            module: REF_ATOM,
            function: REF_ATOM,
            arity: 0,
        });
    }
    body.push(Instr::Return);
    body
}

/// `move Integer -> X(0)` — the plain in-bound x-register `move`.
fn body_move_x(repeat: usize) -> Vec<Instr> {
    let mut body = Vec::new();
    for _ in 0..repeat {
        body.push(Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(0),
        });
    }
    body.push(Instr::Return);
    body
}

/// `move Literal(0) -> X(0)` — the **literal-area-source** `move` (a run-time-guarded
/// literal load; the literal area need not exist to compile the shape).
fn body_move_literal(repeat: usize) -> Vec<Instr> {
    let mut body = Vec::new();
    for _ in 0..repeat {
        body.push(Instr::Move {
            src: Operand::Literal(0),
            dst: Operand::X(0),
        });
    }
    body.push(Instr::Return);
    body
}

/// `test is_nil Y(0) Fail=self` — the **Y-operand** `test` (a guarded frame-window
/// load). The fail label is the block's own entry, so the function stays one block.
fn body_test_y(repeat: usize) -> Vec<Instr> {
    let mut body = Vec::new();
    for _ in 0..repeat {
        body.push(Instr::Test {
            kind: TestKind::IsNil,
            fail: Label(2),
            args: vec![Operand::Y(0)],
        });
    }
    body.push(Instr::Return);
    body
}

/// `select_val Y(0) Fail=self {0 -> self}` — the **Y-operand** `select_val`. A switch
/// always transfers control, so it ends its block (a second copy is unreachable).
fn body_select_val_y(_repeat: usize) -> Vec<Instr> {
    vec![Instr::SelectVal {
        value: Operand::Y(0),
        fail: Label(2),
        pairs: vec![(Operand::Integer(0), Label(2))],
    }]
}

/// `select_tuple_arity Y(0) Fail=self {2 -> self}` — the **Y-operand**
/// `select_tuple_arity`. Like `select_val` it always transfers control.
fn body_select_tuple_arity_y(_repeat: usize) -> Vec<Instr> {
    vec![Instr::SelectTupleArity {
        tuple: Operand::Y(0),
        fail: Label(2),
        destinations: vec![(Operand::Integer(2), Label(2))],
    }]
}

/// `call 0 self` — the **frame-preserving local call** (the `jit_call_store` helper
/// plus the local-call seam probe). It transfers control, so it ends its block.
fn body_call(_repeat: usize) -> Vec<Instr> {
    vec![Instr::Call {
        arity: 0,
        target: Label(2),
    }]
}

/// `put_tuple2` — a **family-owned** instruction (routed to `families::dispatch`'s
/// owning family through `emit::family::emit_family`).
fn body_family(repeat: usize) -> Vec<Instr> {
    let mut body = Vec::new();
    for _ in 0..repeat {
        body.push(Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        });
    }
    body.push(Instr::Return);
    body
}

/// Compile `module`'s function `func` with the counters on and return
/// `(block_emitted_bytes, total_static_deopts)`.
fn measure_module(module: &Module, func: u32) -> (u64, u64) {
    crate::jit::cache::clear_baseline_cache();
    counters::reset();
    counters::set_enabled(true);
    let labels = crate::interpreter::build_label_map(module);
    compile_function(module, &labels, func).expect("the census shape must compile");
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    (
        snapshot.block_emitted_bytes,
        snapshot.instr_deopts.values().sum(),
    )
}

/// Compile a module whose function `f/0` is a **single** block carrying `body` (prefixed
/// by the one entry label `Label(2)`) and return `measure_module`'s pair. One label means
/// one IR block, so the summed `block_emitted_bytes` is that block's own size.
fn measure_one_block(name: &str, body: Vec<Instr>) -> (u64, u64) {
    let mut code = vec![Instr::Label(Label(2))];
    code.extend(body);
    let module = build_module(name, &[], vec![("f", 0, code)], &[("f", 0, Label(2))]);
    measure_module(&module, 0)
}

/// **The per-kind emitted-code-size table** — the byte cost of each in-block lowering
/// the native baseline tier carries, against the fixed per-occurrence deopt it replaces.
///
/// This is *evidence for the tier's design*, not an assertion: the test asserts only
/// liveness/consistency (the reference deopts, every named shape lowers, the artifact
/// compiles and emits bytes) and **prints** the table. Run it with `--nocapture`:
///
/// ```text
/// erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
///     -p beam_engine --lib the_per_kind_emitted_code_size_table_is_printed -- --nocapture
/// ```
#[test]
fn the_per_kind_emitted_code_size_table_is_printed() {
    // (1) The fixed deopt-sequence reference: a block with ONE always-deopting
    // `func_info`. Its `2 − 1` marginal is the confound-free per-occurrence deopt cost.
    let (deopt_block, deopt_deopts) = measure_one_block("size_deopt_ref", body_deopt(1));
    assert_eq!(
        deopt_deopts, 1,
        "the reference block must deopt exactly once (the one `func_info`)"
    );
    let (deopt_block_2x, _) = measure_one_block("size_deopt_ref_2x", body_deopt(2));
    let deopt_seq = deopt_block_2x.saturating_sub(deopt_block);
    assert!(
        deopt_seq > 0 && deopt_block > deopt_seq,
        "the deopt sequence must be a positive part of the reference block \
         (seq={deopt_seq} block={deopt_block})"
    );

    // (2) The shapes. `repeatable` is `true` only when a second copy fits in the same
    // block (a block-terminating shape cannot), which is what the marginal needs.
    let kinds: [(&str, fn(usize) -> Vec<Instr>, bool); 7] = [
        ("Move x<-int", body_move_x, true),
        ("Move x<-literal", body_move_literal, true),
        ("Test Y-src (is_nil)", body_test_y, true),
        ("SelectVal Y-src", body_select_val_y, false),
        ("SelectTupleArity Y-src", body_select_tuple_arity_y, false),
        ("Call (frame-preserving)", body_call, false),
        ("Family arm (put_tuple2)", body_family, true),
    ];

    println!("=== baseline-JIT per-kind emitted-code size ===");
    println!(
        "deopt reference: a block with ONE always-deopting `func_info` = {deopt_block} bytes \
         (the fixed per-occurrence deopt sequence, via the 2x-minus-1x marginal, is {deopt_seq} bytes)"
    );
    println!();
    println!(
        "{:<26} {:>10} {:>10} {:>7} {:>14} {:>8}",
        "kind", "inlined", "deopt", "delta", "per-occurrence", "lowered"
    );

    let mut summed_delta: i64 = 0;
    let mut rows: Vec<(String, u64, i64, Option<i64>)> = Vec::new();
    for (name, build, repeatable) in &kinds {
        let (bytes1, deopts1) = measure_one_block("size_kind", build(1));
        let lowered = deopts1 == 0;
        let per_occurrence = repeatable.then(|| {
            let (bytes2, _) = measure_one_block("size_kind_2x", build(2));
            bytes2 as i64 - bytes1 as i64
        });
        let delta = bytes1 as i64 - deopt_block as i64;
        summed_delta += delta;
        let per_occ_str = per_occurrence
            .map(|v| format!("{v:+}"))
            .unwrap_or_else(|| "n/a".to_string());
        println!(
            "{:<26} {:>10} {:>10} {:>+7} {:>14} {:>8}",
            name,
            bytes1,
            deopt_block,
            delta,
            per_occ_str,
            if lowered { "yes" } else { "no" },
        );
        assert!(
            lowered,
            "the shape `{name}` must lower (no static deopt) for the row to be its inlined size"
        );
        rows.push((name.to_string(), bytes1, delta, per_occurrence));
    }

    // (3) The resolved-but-**not-compiled** `call_last` tail exit needs a second
    // function: the target label resolves (module-wide) but no block is compiled for it
    // in this per-function artifact, so the exit is `emit_tail_exit_to_driver`.
    let call_last_module = build_module(
        "size_call_last_unlinked",
        &[],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(2)),
                    Instr::CallLast {
                        arity: 0,
                        target: Label(9),
                        deallocate: 0,
                    },
                ],
            ),
            ("g", 0, vec![Instr::Label(Label(9)), Instr::Return]),
        ],
        &[("f", 0, Label(2))],
    );
    let (cl_bytes, cl_deopts) = measure_module(&call_last_module, 0);
    let cl_delta = cl_bytes as i64 - deopt_block as i64;
    summed_delta += cl_delta;
    println!(
        "{:<26} {:>10} {:>10} {:>+7} {:>14} {:>8}",
        "CallLast (unlinked exit)",
        cl_bytes,
        deopt_block,
        cl_delta,
        "n/a",
        if cl_deopts == 0 { "yes" } else { "no" },
    );
    rows.push((
        "CallLast (unlinked exit)".to_string(),
        cl_bytes,
        cl_delta,
        None,
    ));
    assert_eq!(
        cl_deopts, 0,
        "the resolved-not-compiled `call_last` tail exit must lower (no static deopt)"
    );

    println!();
    println!(
        "summed per-kind deltas (inlined block − the {deopt_block}-byte deopt reference, \
         ONE occurrence of each shape): {summed_delta:+} bytes"
    );
    println!(
        "  (per-occurrence, this is NOT artifact-scaled: an artifact-wide deopt-only figure \
         multiplies each delta by that shape's occurrence count)"
    );
    println!();

    // --- (4) liveness / consistency (never a threshold) ---------------------
    assert!(
        rows.len() == 8,
        "the table must have one row per measured shape"
    );
    assert!(
        rows.iter().any(|(_, _, delta, _)| *delta < 0),
        "at least one lowered shape must be smaller than the deopt block it replaces"
    );
    assert!(
        rows.iter()
            .filter(|(_, _, _, per_occ)| per_occ.is_some())
            .count()
            >= 1,
        "at least one shape must have a confound-free per-occurrence marginal"
    );

    // --- (5) the whole-artifact size on the committed fixtures, lowerings ON --
    let fixtures = fixture_corpus();
    let mut artifact_total = 0usize;
    let mut artifact_rows: Vec<(String, usize, usize, u64)> = Vec::new();
    for path in &fixtures {
        let Ok(module) = beam_loader::load_file(path) else {
            // A module that will not load is not this diagnostic's concern.
            continue;
        };
        crate::jit::cache::clear_baseline_cache();
        counters::reset();
        counters::set_enabled(true);
        let compiled = compile_module(&module).expect("the fixture must compile");
        counters::set_enabled(false);
        let snapshot = counters::snapshot();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        artifact_total += compiled.code_len();
        artifact_rows.push((
            name,
            compiled.code_len(),
            compiled.block_count(),
            snapshot.instr_instances.values().sum(),
        ));
    }
    artifact_rows.sort_by(|a, b| b.1.cmp(&a.1));

    println!("-- whole-artifact `code_len` on the {} committed fixtures (lowerings ON) --", artifact_rows.len());
    for (name, code_len, blocks, instances) in artifact_rows.iter().take(12) {
        println!("  {name:<24} code_len={code_len:<8} blocks={blocks:<5} emitted_instances={instances}");
    }
    println!(
        "  total code_len over {} fixtures = {artifact_total} bytes",
        artifact_rows.len()
    );

    assert!(
        !artifact_rows.is_empty(),
        "the committed fixture corpus must yield at least one compiled artifact"
    );
    assert!(
        artifact_total > 0,
        "the whole-artifact `code_len` must be positive"
    );

    let body: String = rows
        .iter()
        .map(|(name, bytes, delta, per_occurrence)| {
            format!(
                "{name}\t{bytes}\t{}\t{delta}\t{}\n",
                deopt_block,
                per_occurrence
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".to_string()),
            )
        })
        .collect();
    let header = "# emitted-code size: kind\tinlined_bytes\tdeopt_block_bytes\tdelta\tper_occurrence\n";
    if let Some(path) = write_dump("code_size", header, body) {
        println!("machine-readable dump: {}", path.display());
    }
}

// ---------------------------------------------------------------------------
// Register-cache (spill/fill) traffic census
// ---------------------------------------------------------------------------
//
// The block register cache's cost is the store/load traffic it emits: a `spill` stores
// cached x-registers back into `machine.x` and a `fill` reloads them. The emitter has
// three candidate spill models — the **dirty** set `regs::spill` emits today
// (`Allocation::spill_slots_at(pc)`), the **written set** (`Allocation::written`) and the
// **full cache** (`Allocation::slots`), the model the partial spill replaced — and two
// fill models — the continuation's **liveness** (`Allocation::fill_slots`) and the
// **full cache**. `counters::record_block` derives the
// models only the enclosing block can see (see the counters module docs). This section
// compiles the corpus the static deopt census uses and prints the traffic each model
// would emit — the deterministic evidence for whether a finer spill/fill rule is worth
// adding, independent of any run.

/// One register-cache census row: the cache-traffic scalars, the per-site splits and the
/// block histograms of one module, or of an aggregate of modules.
///
/// Every traffic field is a copy of the matching
/// [`Counters`](super::counters::Counters) field, so [`RegCacheRow::add`] is a plain
/// field-by-field sum (the maps are keyed sums).
#[derive(Default)]
struct RegCacheRow {
    /// The row's label (a fixture file name, `OTP sample`, or a total).
    label: String,
    /// Modules the row covers that loaded.
    modules: u64,
    /// Functions the emitter lowered.
    functions: u64,
    /// Σ `compile_module(..).code_len()` over the row's modules (0 for the OTP sample and
    /// the aggregate totals, whose `code_len` the static census' per-kind table reports).
    code_len: usize,
    /// Spill stores under the **dirty-set** model `regs::spill` emits
    /// (Σ `spill_slots_at(pc).len()`).
    spill_stores: u64,
    /// Spill stores under the written-set model (Σ `written.len()`).
    spill_stores_written: u64,
    /// Spill stores under the full-cache model (Σ `slots.len()`).
    spill_stores_full: u64,
    /// Fill loads under the liveness model (Σ `fill_slots(pc).len()`).
    fill_registers: u64,
    /// Fill loads under the full-cache model (Σ `slots.len()`).
    fill_registers_full: u64,
    /// Emitted blocks (IR blocks **and** continuation sub-blocks).
    blocks: u64,
    /// Σ over blocks of the cache size.
    block_cache_slots: u64,
    /// Σ over blocks of the write-set size.
    block_written_slots: u64,
    /// Total native bytes the emitted blocks produced.
    block_emitted_bytes: u64,
    /// Total IR instructions the emitted blocks cover.
    block_ir_instructions: u64,
    /// `spill` calls by site.
    spill_calls_by_site: BTreeMap<SpillSite, u64>,
    /// Spill stores by site under the dirty-set model `regs::spill` emits.
    spill_stores_by_site: BTreeMap<SpillSite, u64>,
    /// Spill stores by site under the written-set model.
    spill_stores_written_by_site: BTreeMap<SpillSite, u64>,
    /// Spill stores by site under the full-cache model.
    spill_stores_by_site_full: BTreeMap<SpillSite, u64>,
    /// `fill` calls by site.
    fill_calls_by_site: BTreeMap<FillSite, u64>,
    /// Fill loads by site under the liveness model.
    fill_registers_by_site: BTreeMap<FillSite, u64>,
    /// Fill loads by site under the full-cache model.
    fill_registers_by_site_full: BTreeMap<FillSite, u64>,
    /// Blocks by cache size.
    blocks_by_cache_slots: BTreeMap<u64, u64>,
    /// Blocks by write-set size.
    blocks_by_written_slots: BTreeMap<u64, u64>,
}

impl RegCacheRow {
    /// Build a row from a `Counters` snapshot plus the module/function/`code_len` tallies
    /// the counters do not carry.
    fn from_snapshot(
        label: String,
        modules: u64,
        functions: u64,
        code_len: usize,
        counters: &Counters,
    ) -> RegCacheRow {
        RegCacheRow {
            label,
            modules,
            functions,
            code_len,
            spill_stores: counters.spill_stores,
            spill_stores_written: counters.spill_stores_written,
            spill_stores_full: counters.spill_stores_full,
            fill_registers: counters.fill_registers,
            fill_registers_full: counters.fill_registers_full,
            blocks: counters.blocks,
            block_cache_slots: counters.block_cache_slots,
            block_written_slots: counters.block_written_slots,
            block_emitted_bytes: counters.block_emitted_bytes,
            block_ir_instructions: counters.block_ir_instructions,
            spill_calls_by_site: counters.spill_calls_by_site.clone(),
            spill_stores_by_site: counters.spill_stores_by_site.clone(),
            spill_stores_written_by_site: counters.spill_stores_written_by_site.clone(),
            spill_stores_by_site_full: counters.spill_stores_by_site_full.clone(),
            fill_calls_by_site: counters.fill_calls_by_site.clone(),
            fill_registers_by_site: counters.fill_registers_by_site.clone(),
            fill_registers_by_site_full: counters.fill_registers_by_site_full.clone(),
            blocks_by_cache_slots: counters.blocks_by_cache_slots.clone(),
            blocks_by_written_slots: counters.blocks_by_written_slots.clone(),
        }
    }

    /// Fold `other` into this row (plain sums; the maps are keyed sums).
    fn add(&mut self, other: &RegCacheRow) {
        self.modules += other.modules;
        self.functions += other.functions;
        self.code_len += other.code_len;
        self.spill_stores += other.spill_stores;
        self.spill_stores_written += other.spill_stores_written;
        self.spill_stores_full += other.spill_stores_full;
        self.fill_registers += other.fill_registers;
        self.fill_registers_full += other.fill_registers_full;
        self.blocks += other.blocks;
        self.block_cache_slots += other.block_cache_slots;
        self.block_written_slots += other.block_written_slots;
        self.block_emitted_bytes += other.block_emitted_bytes;
        self.block_ir_instructions += other.block_ir_instructions;
        add_sites(&mut self.spill_calls_by_site, &other.spill_calls_by_site);
        add_sites(&mut self.spill_stores_by_site, &other.spill_stores_by_site);
        add_sites(
            &mut self.spill_stores_written_by_site,
            &other.spill_stores_written_by_site,
        );
        add_sites(
            &mut self.spill_stores_by_site_full,
            &other.spill_stores_by_site_full,
        );
        add_sites(&mut self.fill_calls_by_site, &other.fill_calls_by_site);
        add_sites(&mut self.fill_registers_by_site, &other.fill_registers_by_site);
        add_sites(
            &mut self.fill_registers_by_site_full,
            &other.fill_registers_by_site_full,
        );
        add_counts(&mut self.blocks_by_cache_slots, &other.blocks_by_cache_slots);
        add_counts(
            &mut self.blocks_by_written_slots,
            &other.blocks_by_written_slots,
        );
    }

    /// The redundant ("clean") stores the written-set model emits beyond the **dirty-set**
    /// model `regs::spill` emits today: `written − spill`. It is the exact store count the
    /// dirty-set change removes — a cached register the block wrote and then left holding
    /// its old `machine.x` value, which the dirty walk skips but the whole-block write set
    /// still stores.
    fn redundant_clean_stores(&self) -> u64 {
        self.spill_stores_written.saturating_sub(self.spill_stores)
    }

    /// The total number of `spill` calls (Σ over the sites).
    fn spill_calls(&self) -> u64 {
        self.spill_calls_by_site.values().sum()
    }

    /// The total number of `fill` calls (Σ over the sites).
    fn fill_calls(&self) -> u64 {
        self.fill_calls_by_site.values().sum()
    }
}

/// Keyed sum of one `SpillSite`/`FillSite` map into another.
fn add_sites<K: Ord + Copy>(into: &mut BTreeMap<K, u64>, from: &BTreeMap<K, u64>) {
    for (site, count) in from {
        *into.entry(*site).or_insert(0) += *count;
    }
}

/// Keyed sum of one `u64`-keyed histogram into another.
fn add_counts(into: &mut BTreeMap<u64, u64>, from: &BTreeMap<u64, u64>) {
    for (key, count) in from {
        *into.entry(*key).or_insert(0) += *count;
    }
}

/// The whole-artifact `code_len` of one committed fixture module — the same figure the
/// static census' per-kind table tabulates. Compiled with recording **off** (the row's
/// traffic session is already closed), so it never perturbs the counters.
fn fixture_code_len(path: &Path) -> usize {
    let module = beam_loader::load_file(path).expect("a committed fixture must load");
    compile_module(&module)
        .expect("a committed fixture must compile")
        .code_len()
}

/// Print one register-cache row's aggregates, per-site splits and histograms.
fn print_reg_cache_row(row: &RegCacheRow) {
    println!("-- {} --", row.label);
    println!(
        "  modules={} functions={} code_len={}",
        row.modules, row.functions, row.code_len
    );
    println!(
        "  spill stores: spill={} written-set={} full-cache={} (redundant clean stores = written − spill = {})",
        row.spill_stores,
        row.spill_stores_written,
        row.spill_stores_full,
        row.redundant_clean_stores(),
    );
    println!(
        "  fill registers: liveness={} full-cache={}",
        row.fill_registers, row.fill_registers_full
    );
    println!(
        "  spill calls: total={} by site {:?}",
        row.spill_calls(),
        row.spill_calls_by_site
    );
    println!("  spill stores by site (spill model): {:?}", row.spill_stores_by_site);
    println!(
        "  spill stores by site (written-set): {:?}",
        row.spill_stores_written_by_site
    );
    println!(
        "  spill stores by site (full-cache): {:?}",
        row.spill_stores_by_site_full
    );
    println!(
        "  fill calls: total={} by site {:?}",
        row.fill_calls(),
        row.fill_calls_by_site
    );
    println!(
        "  fill registers by site (liveness): {:?}",
        row.fill_registers_by_site
    );
    println!(
        "  fill registers by site (full-cache): {:?}",
        row.fill_registers_by_site_full
    );
    println!(
        "  blocks={} cache_slots={} written_slots={} emitted_bytes={} ir_instrs={}",
        row.blocks,
        row.block_cache_slots,
        row.block_written_slots,
        row.block_emitted_bytes,
        row.block_ir_instructions,
    );
    println!("  blocks by cache size: {:?}", row.blocks_by_cache_slots);
    println!("  blocks by write size: {:?}", row.blocks_by_written_slots);
    println!();
}

/// Assert the row's traffic obeys the model orderings (never a threshold):
/// `spill ≤ written-set ≤ full-cache` for the spill stores, `liveness ≤ full-cache` for
/// the fill loads, both in total and per site, and that the block histogram is total.
fn assert_reg_cache_row_consistent(row: &RegCacheRow) {
    assert!(
        row.spill_stores <= row.spill_stores_written,
        "{}: the spill model ({} stores) must not exceed the written-set model ({})",
        row.label,
        row.spill_stores,
        row.spill_stores_written,
    );
    assert!(
        row.spill_stores_written <= row.spill_stores_full,
        "{}: the written-set model ({} stores) must not exceed the full-cache model ({})",
        row.label,
        row.spill_stores_written,
        row.spill_stores_full,
    );
    assert!(
        row.fill_registers <= row.fill_registers_full,
        "{}: the liveness fill model ({} loads) must not exceed the full-cache model ({})",
        row.label,
        row.fill_registers,
        row.fill_registers_full,
    );
    for (site, stores) in &row.spill_stores_by_site {
        let written = row
            .spill_stores_written_by_site
            .get(site)
            .copied()
            .unwrap_or(0);
        let full = row.spill_stores_by_site_full.get(site).copied().unwrap_or(0);
        assert!(
            *stores <= written && written <= full,
            "{}: spill site {site:?} violates spill ({stores}) ≤ written-set ({written}) \
             ≤ full-cache ({full})",
            row.label,
        );
    }
    for (site, loads) in &row.fill_registers_by_site {
        let full = row
            .fill_registers_by_site_full
            .get(site)
            .copied()
            .unwrap_or(0);
        assert!(
            *loads <= full,
            "{}: fill site {site:?} violates liveness ({loads}) ≤ full-cache ({full})",
            row.label,
        );
    }
    assert_eq!(
        row.blocks_by_cache_slots.values().sum::<u64>(),
        row.blocks,
        "{}: the blocks-by-cache-size histogram must total `blocks`",
        row.label,
    );
    assert_eq!(
        row.blocks_by_written_slots.values().sum::<u64>(),
        row.blocks,
        "{}: the blocks-by-write-size histogram must total `blocks`",
        row.label,
    );
}

/// The **register-cache traffic census** — the deterministic, corpus-wide evidence for
/// the baseline JIT's block register cache: the spill **store** counts of all three
/// models, the fill **load** counts of both, their per-site splits, and the block
/// cache/write-set histograms, over the committed fixtures plus a bounded sample of the
/// installed OTP.
///
/// Run with `--nocapture` to read the tables:
/// ```text
/// erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
///     -p beam_engine --lib the_register_cache_traffic_census_over_the_corpus_is_printed -- --nocapture
/// ```
#[test]
fn the_register_cache_traffic_census_over_the_corpus_is_printed() {
    crate::jit::cache::clear_baseline_cache();

    let fixtures = fixture_corpus();
    let fixture_count = fixtures.len();

    // Per-fixture-module rows: one recording session per module (the counter block is
    // thread-local and `reset` zeroes it), so each row's traffic is its own.
    let mut rows: Vec<RegCacheRow> = Vec::new();
    for path in &fixtures {
        counters::set_enabled(true);
        counters::reset();
        let tally = compile_corpus(std::slice::from_ref(path));
        counters::set_enabled(false);
        let snapshot = counters::snapshot();
        // `code_len` is a whole-artifact figure the counters do not carry; compile the
        // module a second time, recording OFF, exactly as the code-size table does.
        let code_len = fixture_code_len(path);
        let label = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        rows.push(RegCacheRow::from_snapshot(
            label,
            tally.modules,
            tally.compiled,
            code_len,
            &snapshot,
        ));
    }

    let fixture_modules: u64 = rows.iter().map(|r| r.modules).sum();
    let fixture_functions: u64 = rows.iter().map(|r| r.functions).sum();

    // The OTP sample: one aggregate row (the fixture breakdown is the readable one).
    // Skipped loudly without a usable `erl`.
    let otp = otp_lib_root();
    let otp_modules = match &otp {
        Some(root) => {
            let beams = otp_corpus(root, OTP_MODULE_CAP);
            counters::set_enabled(true);
            counters::reset();
            let tally = compile_corpus(&beams);
            counters::set_enabled(false);
            let snapshot = counters::snapshot();
            Some(RegCacheRow::from_snapshot(
                "OTP sample".to_string(),
                tally.modules,
                tally.compiled,
                0,
                &snapshot,
            ))
        }
        None => {
            eprintln!(
                "register-cache census: no usable `erl` on PATH — the OTP sample is \
                 SKIPPED; only the committed fixtures are censused"
            );
            None
        }
    };
    let otp_module_count = otp_modules.as_ref().map_or(0, |row| row.modules);

    // The totals: the rows fold to a `fixtures total`, and, when the OTP sample ran, a
    // `grand total` folds that in too.
    let mut fixtures_total = RegCacheRow {
        label: "fixtures total".to_string(),
        ..Default::default()
    };
    for row in &rows {
        fixtures_total.add(row);
    }
    let mut grand_total = RegCacheRow::default();
    grand_total.add(&fixtures_total);
    match &otp_modules {
        Some(row) => {
            grand_total.add(row);
            grand_total.label = "grand total (fixtures + OTP sample)".to_string();
        }
        None => grand_total.label = "grand total (fixtures only)".to_string(),
    }

    // --- liveness / consistency (never a threshold) -------------------------
    assert!(
        !rows.is_empty(),
        "the committed fixture corpus must yield at least one module row"
    );
    assert!(
        fixture_modules >= 1,
        "the fixture corpus must compile at least one module (fixtures={fixture_count})"
    );
    assert!(
        fixture_functions > 0,
        "the fixture corpus must lower at least one function"
    );
    for row in rows
        .iter()
        .chain(std::iter::once(&fixtures_total))
        .chain(std::iter::once(&grand_total))
        .chain(otp_modules.iter())
    {
        assert_reg_cache_row_consistent(row);
    }
    assert!(
        grand_total.blocks >= 1,
        "the corpus must emit at least one block"
    );
    assert!(
        grand_total.block_emitted_bytes > 0,
        "the emitted blocks must produce native bytes"
    );
    let fixture_code_len_total: usize = rows.iter().map(|r| r.code_len).sum();
    assert!(
        fixture_code_len_total > 0,
        "the fixture corpus must produce a positive whole-artifact `code_len`"
    );

    // --- the tables ---------------------------------------------------------
    println!("=== baseline-JIT register-cache traffic census ===");
    println!(
        "corpus: fixtures={fixture_count} (modules={fixture_modules} functions={fixture_functions}) \
         otp_modules={otp_module_count} (erl_found={})",
        otp.is_some(),
    );
    println!(
        "three spill models: `spill` (the dirty set it emits today, Σ spill_slots_at(pc)), \
         written-set (Σ written.len()), full-cache (Σ slots.len()); two fill models: liveness \
         vs full-cache"
    );
    println!();
    println!(
        "-- per fixture module: spill(spill/written/full) fill(liveness/full) blocks cache written bytes ir code_len --"
    );
    for row in &rows {
        println!(
            "  {:<24} spill={:>5}/{:<5}/{:<5} fill={:>5}/{:<5} blocks={:<4} cache={:<4} \
             written={:<4} bytes={:<7} ir={:<5} code_len={}",
            row.label,
            row.spill_stores,
            row.spill_stores_written,
            row.spill_stores_full,
            row.fill_registers,
            row.fill_registers_full,
            row.blocks,
            row.block_cache_slots,
            row.block_written_slots,
            row.block_emitted_bytes,
            row.block_ir_instructions,
            row.code_len,
        );
    }
    println!();
    print_reg_cache_row(&fixtures_total);
    if let Some(row) = &otp_modules {
        print_reg_cache_row(row);
    }
    print_reg_cache_row(&grand_total);
    println!(
        "fixture `code_len` total = {fixture_code_len_total} bytes over {} modules",
        rows.len(),
    );

    // --- the machine-readable dump ------------------------------------------
    let mut body = String::new();
    for row in rows
        .iter()
        .chain(std::iter::once(&fixtures_total))
        .chain(otp_modules.iter())
        .chain(std::iter::once(&grand_total))
    {
        body.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.label,
            row.modules,
            row.functions,
            row.code_len,
            row.spill_stores,
            row.spill_stores_written,
            row.spill_stores_full,
            row.fill_registers,
            row.fill_registers_full,
            row.spill_calls(),
            row.fill_calls(),
            row.blocks,
            row.block_cache_slots,
            row.block_written_slots,
            row.block_emitted_bytes,
            row.block_ir_instructions,
        ));
    }
    let header = "# reg-cache: label\tmodules\tfunctions\tcode_len\tspill_stores\t\
                  spill_stores_written\tspill_stores_full\tfill_registers\tfill_registers_full\t\
                  spill_calls\tfill_calls\tblocks\tblock_cache_slots\tblock_written_slots\t\
                  block_emitted_bytes\tblock_ir_instructions\n";
    if let Some(path) = write_dump("reg_cache", header, body) {
        println!("machine-readable dump: {}", path.display());
    }
}
