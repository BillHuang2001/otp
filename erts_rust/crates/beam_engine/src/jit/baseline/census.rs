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

use beam_loader::{Instr, Label, Module, Operand, TestKind};

use super::counters;
use super::emit::{compile_function, compile_module};
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
