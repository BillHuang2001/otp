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

//! A committed, dependency-free **measurement** of `.beam` load cost.
//!
//! Startup latency is an explicit objective of the Rust ERTS rewrite
//! (`../../ERTS_RUST_REWRITE_EVALUATION.md` Part I: compile/load latency is a
//! first-class constraint), so the loader — the first thing a booting runtime does —
//! must be *measurable* before it is optimised. This file is that measurement: it
//! loads a deterministic, representative sample of the installed OTP modules through
//! the crate's public entry point ([`beam_loader::load`]) and prints a stable,
//! machine-readable cost report.
//!
//! # What it measures
//!
//! The headline numbers are **pure loader work**, not file I/O: every selected
//! `.beam` is read *once, up front*, and each timed iteration calls
//! [`beam_loader::load`] on the already-in-memory bytes. The measurement therefore
//! covers the whole pipeline (`container::parse` → `chunk::parse` →
//! `decode::decode_stream` → `lower::build_module`), including `LitT` literal
//! materialisation and fun-descriptor lowering — everything except the `fs::read`.
//!
//! * **headline passes** — one warm-up pass over the whole sample, then
//!   `HEADLINE_PASSES` timed passes over it; the **fastest** pass is the headline
//!   (every pass's wall clock is printed too, so the machine's noise is visible). That
//!   pass's wall clock, total input bytes and total lowered instructions give
//!   `ns_per_instruction` and `bytes_per_second`. A pass is a *warm* load sequence:
//!   the module bytes have already been parsed by the warm-up and the passes before it.
//! * **per-stage pass** — each module loaded `PER_MODULE_RUNS` more times, timing each
//!   pipeline stage separately (container → chunks → atom interning → decode →
//!   lower); the fastest run of each module is the one that counts, and the report
//!   prints the per-stage mean of those best runs. This is the evidence for *where* a
//!   load's time goes, so an optimisation is aimed at a measured stage and a measured
//!   call, never at a guessed one.
//!
//! Every headline report is a best-of-`HEADLINE_PASSES` (minimum) measurement: the mean
//! and the maximum are dominated by scheduler, allocator and co-tenant-build noise,
//! while the minimum is a stable lower bound for the same code on the same machine.
//!
//! # Deterministic selection (what is loaded)
//!
//! The installed `lib` root is resolved exactly as `tests/real_modules.rs` does:
//! `$ERTS_LIB_DIR` when set, else `erl -noshell -eval 'io:format("~s",[code:lib_dir()])'`
//! (OTP's own `code:lib_dir/0`). Every `.beam` under it (34 applications / ~1366
//! modules on a full OTP install) is discovered recursively, sorted by path, and the
//! sample is picked deterministically:
//!
//! 1. the `TARGET / 8` **largest** modules by file size (ties broken by path), so the
//!    expensive end of the distribution is always represented — this is where a
//!    per-instruction or per-literal cost shows up; then
//! 2. evenly spaced modules from the full sorted list (`index = i * len / need`) to
//!    fill the sample up to `TARGET`, spreading across every application.
//!
//! The final sample is sorted by path, so two runs on the same install load exactly
//! the same modules in the same order. `$ERTS_LOAD_COST_MODULES` overrides `TARGET`
//! (used to trade fidelity for speed on a slow machine).
//!
//! # Skip-vs-assert policy
//!
//! A missing OTP install **skips loudly** (prints `SKIP: <reason>` and returns); it is
//! never a failure and never a silent pass. When OTP *is* present the test asserts the
//! sample is non-empty and reportable (`modules`, `instructions`, `bytes` all > 0) —
//! a report of zero work would be a silent measurement failure.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader \
//!     --test load_cost -- --nocapture
//! ```
//!
//! The report is emitted on stdout; every line starts with `LOAD_COST` and is
//! `key=value` pairs, so it can be diffed between revisions (before/after) with a
//! plain text diff.
//!
//! # Recorded reference numbers (BEFORE, pre-optimisation)
//!
//! Machine: 14 logical CPUs, AMD64 Nix sandbox; OTP 29.1.1 (`erts-17.1`) install;
//! `dev` profile = the workspace's `opt-level=1` dev profile, `release` = `--release`.
//! Sample: 200 modules (the 25 largest plus 175 evenly sampled), whole installed
//! `lib` root. `cargo test -p beam_loader --test load_cost -- --nocapture`:
//!
//! | profile | modules | bytes | instructions | ns/instruction | bytes/s |
//! |---------|---------|-------|--------------|----------------|---------|
//! | dev     | 200     | 15 943 132 | 854 423 | 204 | 91 600 000 |
//! | release | 200     | 15 943 132 | 854 423 | 294 | 63 600 000 |
//!
//! These are the `ns_per_instruction`/`bytes_per_second` figures the after-numbers in
//! this crate's optimisation work are compared against; re-measure, never quote.

#![allow(clippy::cast_precision_loss)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// The default number of modules to load — representative, and fast enough to run on
/// every change (`$ERTS_LOAD_COST_MODULES` overrides it).
const TARGET_MODULES: usize = 200;
/// The fraction of the sample reserved for the largest modules (`TARGET_MODULES / 8`).
const LARGEST_DIVISOR: usize = 8;
/// How many timed headline passes; the fastest pass is the headline (the minimum is
/// the least-noise estimator on a machine that may be running other builds).
const HEADLINE_PASSES: usize = 5;
/// How many timed iterations each module gets in the per-stage pass (again the fastest
/// run of each module wins).
const PER_MODULE_RUNS: usize = 7;

// ---------------------------------------------------------------------------
// Locating the installed OTP `lib` root (the same generic resolution as
// `tests/real_modules.rs`: `$ERTS_LIB_DIR`, else OTP's own `code:lib_dir/0`)
// ---------------------------------------------------------------------------

/// The running Erlang's `lib` root — the directory holding the versioned application
/// directories — or `None` when it cannot be located. A caller that gets `None` must
/// **skip loudly**, never fail.
fn otp_lib_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ERTS_LIB_DIR") {
        return Some(PathBuf::from(dir));
    }

    let output = Command::new("erl")
        .args([
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir()]),halt().",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    let root = root.trim();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Recursively collects every `*.beam` file beneath `dir` into `out`.
fn collect_beams(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_beams(&path, out);
        } else if path.extension().map(|ext| ext == "beam").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Every `.beam` under the installed `lib` root, sorted by path.
fn all_beams(root: &Path) -> Option<Vec<PathBuf>> {
    let mut beams = Vec::new();
    for entry in std::fs::read_dir(root).ok()?.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_beams(&path, &mut beams);
        }
    }
    beams.sort();
    (!beams.is_empty()).then_some(beams)
}

/// Why the measurement must skip (and return), or `None` when the OTP install is
/// usable. `stdlib` and `kernel` must both be present, exactly as
/// `tests/real_modules.rs` requires.
fn skip_reason(root: Option<&Path>, beams: Option<&[PathBuf]>) -> Option<String> {
    if root.is_none() {
        return Some(
            "SKIP: could not resolve the OTP `lib` root (need `erl` on PATH or $ERTS_LIB_DIR) — \
             the load-cost measurement requires the installed OTP `.beam` files"
                .to_string(),
        );
    }
    if beams.is_none() {
        return Some(format!(
            "SKIP: OTP `lib` root {} holds no readable `.beam` files — \
             the load-cost measurement requires the installed OTP `.beam` files",
            root.expect("checked above").display()
        ));
    }
    let beams = beams.expect("checked above");
    for app in ["stdlib", "kernel"] {
        let found = beams.iter().any(|path| {
            path.parent()
                .and_then(|dir| dir.parent())
                .and_then(|dir| dir.file_name())
                .is_some_and(|name| name.to_string_lossy().starts_with(app))
        });
        if !found {
            return Some(format!(
                "SKIP: system Erlang/{app} not found — the load-cost measurement requires the \
                 installed OTP `.beam` files"
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Deterministic sample selection
// ---------------------------------------------------------------------------

/// A selected module: its path, relative label and on-disk size.
#[derive(Clone, Debug)]
struct Sample {
    path: PathBuf,
    label: String,
    bytes: usize,
}

/// Picks `target` modules deterministically from the sorted `beams`:
/// the `target / LARGEST_DIVISOR` largest (ties broken by path) plus evenly spaced
/// entries from the whole sorted list. The result is sorted by path.
fn select_sample(root: &Path, beams: &[PathBuf], target: usize) -> Vec<Sample> {
    assert!(target > 0, "the sample must be non-empty");

    // The largest modules, by size (a stable sort on `(size desc, path)`, so ties are
    // broken deterministically by path).
    let largest_count = (target / LARGEST_DIVISOR).min(target);
    let mut by_size: Vec<&PathBuf> = beams.iter().collect();
    by_size.sort_by(|a, b| {
        let a_size = std::fs::metadata(a).map(|meta| meta.len()).unwrap_or(0);
        let b_size = std::fs::metadata(b).map(|meta| meta.len()).unwrap_or(0);
        b_size.cmp(&a_size).then_with(|| a.cmp(b))
    });

    // A boolean mask over the sorted list keeps the selection order-independent.
    let mut picked = vec![false; beams.len()];
    for path in by_size.iter().take(largest_count) {
        let index = beams
            .iter()
            .position(|candidate| candidate == *path)
            .expect("a selected path comes from `beams`");
        picked[index] = true;
    }

    // Fill the rest with evenly spaced entries from the full sorted list.
    let mut picked_count = picked.iter().filter(|flag| **flag).count();
    if picked_count < target {
        let need = target - picked_count;
        for i in 0..need {
            let index = i * beams.len() / need;
            if !picked[index] {
                picked[index] = true;
                picked_count += 1;
            }
        }
    }
    // If the even spread collided with the largest modules, walk forward for the rest
    // (still deterministic).
    let mut index = 0;
    while picked_count < target && index < beams.len() {
        if !picked[index] {
            picked[index] = true;
            picked_count += 1;
        }
        index += 1;
    }

    let mut sample: Vec<Sample> = beams
        .iter()
        .zip(picked)
        .filter(|(_, flag)| *flag)
        .map(|(path, _)| Sample {
            path: path.clone(),
            label: relative_label(root, path),
            bytes: std::fs::metadata(path)
                .map(|meta| meta.len() as usize)
                .unwrap_or(0),
        })
        .collect();
    sample.sort_by(|a, b| a.label.cmp(&b.label));
    sample
}

/// A short, stable label for reporting: `path` relative to `root` when possible.
fn relative_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// The instructions a loaded module contributes (the headline denominator).
fn instruction_count(module: &beam_loader::Module) -> usize {
    module.functions.iter().map(|f| f.code.len()).sum()
}

/// Accumulated per-stage load cost (nanoseconds) over the **fastest run of each
/// module** (see [`phase_breakdown`]).
#[derive(Default)]
struct Phases {
    modules: usize,
    container_ns: u128,
    chunks_ns: u128,
    intern_ns: u128,
    decode_ns: u128,
    lower_ns: u128,
    total_ns: u128,
    other_ns: u128,
    /// The summed `Code` **instruction-stream** bytes of the sampled modules (not the
    /// whole files) — the input the decoder walks.
    code_bytes: usize,
}

/// Times each stage of the pipeline separately, by driving the same public stages
/// [`beam_loader::load`] drives (`container::parse` → `chunk::parse` → atom interning →
/// `decode::decode_stream` → `lower::build_module`).
///
/// The interning step is the three lines `load` runs between the chunk parse and the
/// decode; `other_ns` is the total minus the measured stages (call overhead).
///
/// Each module is timed `runs` times and only its **fastest** run is accumulated
/// (again: the minimum is the least-noise estimator), so the report's per-stage values
/// are per-module means over a best-of-`runs` sample.
fn phase_breakdown(images: &[Vec<u8>], runs: usize) -> Phases {
    let mut phases = Phases::default();
    for bytes in images {
        // `(container, chunks, intern, decode, lower, total, code_stream_len)` of the
        // fastest run of this module.
        let mut best: Option<(u128, u128, u128, u128, u128, u128, usize)> = None;
        for _ in 0..runs {
            let start = Instant::now();
            let container = beam_loader::container::parse(bytes).expect("a real OTP module parses");
            let after_container = Instant::now();
            let chunks = beam_loader::chunk::parse(&container).expect("a real OTP module parses");
            let after_chunks = Instant::now();

            // Exactly `load`'s atom-map construction (`src/load.rs`).
            let mut table = beam_term::AtomTable::new();
            let mut atoms: Vec<beam_loader::ir::Atom> = Vec::with_capacity(chunks.atoms.len());
            atoms.push(beam_loader::ir::Atom::MAX);
            for name in &chunks.atoms[1..] {
                atoms.push(table.intern(name));
            }
            let after_intern = Instant::now();

            let raw = beam_loader::decode::decode_stream(
                chunks.code_stream,
                &atoms,
                &chunks.code_version,
                &chunks.types,
            )
            .expect("a real OTP module decodes");
            let after_decode = Instant::now();

            let module = beam_loader::lower::build_module(&chunks, &raw, table)
                .expect("a real OTP module lowers");
            let end = Instant::now();
            std::hint::black_box(&module);
            drop(module);

            let sample = (
                (after_container - start).as_nanos(),
                (after_chunks - after_container).as_nanos(),
                (after_intern - after_chunks).as_nanos(),
                (after_decode - after_intern).as_nanos(),
                (end - after_decode).as_nanos(),
                (end - start).as_nanos(),
                chunks.code_stream.len(),
            );
            if best.is_none_or(|current| sample.5 < current.5) {
                best = Some(sample);
            }
        }
        let (container_ns, chunks_ns, intern_ns, decode_ns, lower_ns, total_ns, code_stream) =
            best.expect("`runs` is non-zero");
        phases.container_ns += container_ns;
        phases.chunks_ns += chunks_ns;
        phases.intern_ns += intern_ns;
        phases.decode_ns += decode_ns;
        phases.lower_ns += lower_ns;
        phases.total_ns += total_ns;
        phases.code_bytes += code_stream;
        phases.modules += 1;
    }
    let measured = phases.container_ns
        + phases.chunks_ns
        + phases.intern_ns
        + phases.decode_ns
        + phases.lower_ns;
    phases.other_ns = phases.total_ns.saturating_sub(measured);
    phases
}

/// Prints the report, then asserts the sample genuinely did work (a zero-work report
/// would be a silent measurement failure).
#[test]
fn load_cost_report() {
    let target: usize = std::env::var("ERTS_LOAD_COST_MODULES")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(TARGET_MODULES);

    let root = otp_lib_dir();
    let beams = root.as_deref().and_then(all_beams);
    if let Some(reason) = skip_reason(root.as_deref(), beams.as_deref()) {
        eprintln!("{reason}");
        return;
    }
    let root = root.expect("checked by `skip_reason`");
    let beams = beams.expect("checked by `skip_reason`");

    let sample = select_sample(&root, &beams, target.min(beams.len()));
    assert!(
        !sample.is_empty(),
        "the measurement selected no modules under {}",
        root.display()
    );

    // Read every selected image once, up front: the timed work is the loader, not I/O.
    let images: Vec<Vec<u8>> = sample
        .iter()
        .map(|module| {
            std::fs::read(&module.path).unwrap_or_else(|error| {
                panic!("reading {} failed: {error}", module.path.display())
            })
        })
        .collect();

    let profile = if cfg!(debug_assertions) {
        "dev"
    } else {
        "release"
    };
    let cpus = std::thread::available_parallelism().map_or(0, |n| n.get());

    // -- warm-up pass: every code path (including the literal-arena retry) runs once.
    let mut warm_instructions = 0usize;
    for bytes in &images {
        let module = beam_loader::load(bytes).expect("a real OTP module must load");
        warm_instructions += instruction_count(std::hint::black_box(&module));
    }

    // -- headline passes: one timed load per module, after the warm-up. The fastest
    // pass is the headline (the minimum is the least-noise estimator on a machine that
    // may be running other builds); every pass's wall clock is also reported, so noise
    // is visible rather than hidden.
    let mut total_instructions = 0usize;
    let mut total_bytes = 0usize;
    let mut pass_ns: Vec<u128> = Vec::with_capacity(HEADLINE_PASSES);
    let mut per_module_ns: Vec<u128> = vec![u128::MAX; images.len()];
    let mut per_module_instructions: Vec<usize> = vec![0; images.len()];
    for _pass in 0..HEADLINE_PASSES {
        let start = Instant::now();
        let mut instructions_this_pass = 0usize;
        let mut bytes_this_pass = 0usize;
        for (index, bytes) in images.iter().enumerate() {
            let begin = Instant::now();
            let module =
                beam_loader::load(std::hint::black_box(bytes)).expect("a real OTP module must load");
            let elapsed = begin.elapsed().as_nanos();
            let instructions = instruction_count(std::hint::black_box(&module));
            drop(module);
            instructions_this_pass += instructions;
            bytes_this_pass += bytes.len();
            per_module_ns[index] = per_module_ns[index].min(elapsed);
            per_module_instructions[index] = instructions;
        }
        pass_ns.push(start.elapsed().as_nanos());
        // The instruction/byte totals are load-order and timing invariant; any pass
        // yields the same numbers (and the warm-up agrees with them, asserted below).
        total_instructions = instructions_this_pass;
        total_bytes = bytes_this_pass;
    }
    let total_ns = pass_ns.iter().copied().min().unwrap_or(0);

    let mut sorted_ns = per_module_ns.clone();
    sorted_ns.sort_unstable();
    let min_ns = sorted_ns.first().copied().unwrap_or(0);
    let median_ns = sorted_ns.get(sorted_ns.len() / 2).copied().unwrap_or(0);
    let max_ns = sorted_ns.last().copied().unwrap_or(0);
    // The sum of each module's **best** load across the passes. Unlike a single pass's
    // wall clock, this is barely affected by one module being interrupted by the
    // scheduler, so it is the headline used to compare two revisions (and the pass
    // wall clock above is what a real boot actually sees).
    let sum_best_ns: u128 = per_module_ns.iter().sum();

    // -- phase pass: the same warm loads, split by pipeline stage. This is the
    // evidence for *where* a load's time goes (`LOAD_COST_PHASE`), so an optimisation
    // is aimed at a measured hot spot rather than a guessed one.
    let phases = phase_breakdown(&images, PER_MODULE_RUNS);

    let ns_per_instruction = if total_instructions == 0 {
        0.0
    } else {
        total_ns as f64 / total_instructions as f64
    };
    let bytes_per_second = if total_ns == 0 {
        0.0
    } else {
        total_bytes as f64 * 1_000_000_000.0 / total_ns as f64
    };
    // The same two rates from the low-variance per-module aggregate (see `sum_best_ns`).
    let ns_per_instruction_best = if total_instructions == 0 {
        0.0
    } else {
        sum_best_ns as f64 / total_instructions as f64
    };
    let bytes_per_second_best = if sum_best_ns == 0 {
        0.0
    } else {
        total_bytes as f64 * 1_000_000_000.0 / sum_best_ns as f64
    };

    println!(
        "LOAD_COST profile={profile} cpus={cpus} warm={} passes={HEADLINE_PASSES} \
         modules={} bytes={} instructions={} total_ns={total_ns} \
         ns_per_instruction={ns_per_instruction:.1} bytes_per_second={bytes_per_second:.0} \
         sum_module_best_ns={sum_best_ns} \
         ns_per_instruction_best={ns_per_instruction_best:.1} \
         bytes_per_second_best={bytes_per_second_best:.0} \
         per_module_min_ns={min_ns} per_module_median_ns={median_ns} per_module_max_ns={max_ns}",
        if warm_instructions > 0 { "yes" } else { "no" },
        sample.len(),
        total_bytes,
        total_instructions,
    );
    // Every headline pass's wall clock: the spread is the machine's noise, and the
    // minimum above is the headline.
    println!(
        "LOAD_COST_PASSES pass_ns={:?}",
        pass_ns.iter().map(|ns| *ns as u64).collect::<Vec<u64>>()
    );
    let mut sorted_bytes: Vec<usize> = images.iter().map(|bytes| bytes.len()).collect();
    sorted_bytes.sort_unstable();
    println!(
        "LOAD_COST_SAMPLE modules={} input_bytes={} code_bytes={} module_bytes_min={} \
         module_bytes_median={} module_bytes_max={} ns_per_module_mean={:.0}",
        sample.len(),
        total_bytes,
        phases.code_bytes,
        sorted_bytes.first().copied().unwrap_or(0),
        sorted_bytes.get(sorted_bytes.len() / 2).copied().unwrap_or(0),
        sorted_bytes.last().copied().unwrap_or(0),
        total_ns as f64 / sample.len() as f64,
    );

    println!(
        "LOAD_COST_PHASE modules={} runs={PER_MODULE_RUNS} container_parse_ns={:.0} \
         chunk_parse_ns={:.0} intern_atoms_ns={:.0} decode_stream_ns={:.0} \
         lower_build_module_ns={:.0} other_ns={:.0}",
        phases.modules,
        phases.container_ns as f64 / phases.modules as f64,
        phases.chunks_ns as f64 / phases.modules as f64,
        phases.intern_ns as f64 / phases.modules as f64,
        phases.decode_ns as f64 / phases.modules as f64,
        phases.lower_ns as f64 / phases.modules as f64,
        phases.other_ns as f64 / phases.modules as f64,
    );

    // Per-module detail: cheap to print, and the raw material for a before/after diff.
    // `ns` is that module's best load across the headline passes.
    for (index, module) in sample.iter().enumerate() {
        println!(
            "LOAD_COST_MODULE {} bytes={} instructions={} ns={}",
            module.label, module.bytes, per_module_instructions[index], per_module_ns[index],
        );
    }

    assert!(
        total_instructions > 0 && total_bytes > 0,
        "the measurement loaded {total_instructions} instruction(s) / {total_bytes} byte(s) — \
         the sample under {} is not usable",
        root.display()
    );
    // A regression guard on the *measurement* itself: the headline pass must agree with
    // the warm-up on how much code the sample contains.
    assert_eq!(
        total_instructions, warm_instructions,
        "the timed pass lowered a different instruction count than the warm-up run"
    );
}
