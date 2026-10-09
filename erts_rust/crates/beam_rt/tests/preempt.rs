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

//! **Reduction-budget pre-emption, proved by interleaving.**
//!
//! The runtime's scheduler hands its runner one slice's worth of reductions
//! ([`beam_process::DEFAULT_REDUCTIONS_PER_SLICE`] = 4000) and expects it to return
//! [`beam_process::RunOutcome::Yield`] once the budget is spent; the scheduler then requeues
//! the process at the **tail** of the run queue, so the next slice goes to whatever was queued
//! behind it ([`beam_process::Scheduler::run_slice`], `beam_rt/src/runner/`).
//!
//! Pre-emption is **cooperative**: a runner that ignored its budget would run the whole
//! CPU-bound process inside the one `run` call the scheduler makes and only then return, so a
//! short process that was spawned *second* would never get a turn until the first had finished
//! entirely. This suite pins the property that distinguishes the two behaviours, by observing
//! the order in which two processes' results become available:
//!
//! * spawn a CPU-bound process (an allocating loop) **first**, so it is at the head of the run
//!   queue, and a short non-allocating process **second**, so it sits behind it;
//! * after one slice, the CPU-bound process must **not** have finished (it yielded) and the
//!   short process must **not** have run yet;
//! * after the next slice, the short process must have **returned** while the CPU-bound one is
//!   still unfinished -- a budget-ignoring runner could not satisfy this, because it would have
//!   finished the first process in slice 1;
//! * driving the scheduler to completion then finishes the CPU-bound process with the
//!   **fully-built** list -- proving the yield suspended it and lost no work rather than
//!   dropping or corrupting it.
//!
//! # The fixture
//!
//! `tests/fixtures/preempt.beam` (source `preempt.erl`, both committed) is the image. Its
//! `alloc_loop/1` is a tail-recursive accumulator that builds one cons cell per iteration, so
//! the compiler emits a `test_heap` before each allocation -- the one per-instruction seam the
//! reduction budget is charged at. `quick/0` returns the atom `done` in a single slice.
//!
//! The committed image is used directly (no toolchain needed beyond the system installation
//! the boot sequence resolves); [`a_freshly_compiled_fixture_shows_the_same_interleaving`]
//! additionally recompiles the source with the system `erlc` and repeats the proof, skipping
//! **loudly** when `erlc` is unavailable.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{is_small, make_small, signed_value};

/// The committed fixture image, built with the system `erlc` (recipe in `preempt.erl`'s
/// header) and loaded by **relative path** so there is a single source of truth for it.
const PREEMPT_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/preempt.beam");

/// The committed fixture source, recompiled at test time by the second test.
const PREEMPT_ERL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/preempt.erl");

/// The number of allocations `preempt:alloc_loop/1` performs in one call.
///
/// Far above one slice's budget ([`beam_process::DEFAULT_REDUCTIONS_PER_SLICE`] = 4000), so a
/// single slice can never finish it, and small enough to complete in well under a second.
const ALLOCATIONS: i64 = 100_000;

/// A runtime with the fixture loaded and its boot sequence run.
///
/// `boot` (and therefore every `spawn_process`, which boots on demand) resolves the system
/// code path from the filesystem (the system `erl` only when no installation is discoverable),
/// so a tree without the system installation cannot run this suite at all -- reported as a
/// **loud skip** by [`support::require_oracle`], never a silent pass.
fn runtime_with(beam: &str) -> Option<Runtime> {
    support::require_oracle("preempt")?;
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(beam).expect("the fixture loads");
    Some(rt)
}

/// The correctness heart of the suite: run the two processes and assert the interleaving.
///
/// Shared by the committed-image test and the freshly-compiled test so both pin the **same**
/// property.
fn assert_pre_emption_interleaving(rt: &mut Runtime) {
    // The CPU-bound process is spawned FIRST, so it is at the head of the run queue (spawn
    // enqueues; it does not run until the scheduler is stepped).
    let cpu_bound = rt
        .spawn_process("preempt", "alloc_loop", vec![make_small(ALLOCATIONS)])
        .expect("spawn preempt:alloc_loop/1");
    // The short process is spawned SECOND, so it lands BEHIND the CPU-bound one.
    let short = rt
        .spawn_process("preempt", "quick", vec![])
        .expect("spawn preempt:quick/0");
    assert_eq!(rt.run_queue_len(), 2, "both are runnable, in spawn order");

    // ---- Slice 1: the CPU-bound process runs and is PRE-EMPTED. ----
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    assert!(
        rt.process_result(cpu_bound).is_none(),
        "the CPU-bound loop did NOT finish in slice 1: it spent its reduction budget and \
         yielded. A runner that ignored the budget would have finished it here, so its \
         result would already be present."
    );
    assert!(
        rt.process_result(short).is_none(),
        "the short process has not had its turn yet (the CPU-bound one is still ahead of it)"
    );
    assert_eq!(
        rt.run_queue_len(),
        2,
        "the yielded process was requeued at the tail; the short one is still queued"
    );

    // ---- Slice 2: the SHORT process gets its turn, because the CPU-bound one yielded. ----
    assert_eq!(rt.run_one_slice().expect("one slice"), 1);
    let short_result = rt
        .process_result(short)
        .expect("the short process must have run by now: the CPU-bound one yielded to it");
    assert_eq!(
        rt.atom_name_of(short_result),
        Some("done"),
        "quick/0 returned the atom `done`"
    );
    assert!(
        rt.process_result(cpu_bound).is_none(),
        "the CPU-bound process is STILL unfinished after two slices: this is exactly the \
         fairness a budget-ignoring runner cannot satisfy, because it would have finished the \
         first process inside slice 1 and left the short one nothing to overtake."
    );
    assert_eq!(
        rt.run_queue_len(),
        1,
        "only the CPU-bound process is left runnable"
    );

    // ---- Drive to completion: the yield SUSPENDED the process, losing no work. ----
    rt.run_scheduler().expect("the pass drains the queue");
    let built = rt
        .process_result(cpu_bound)
        .expect("the CPU-bound loop finished once the short process was out of the way");
    let elements = terms::list_elements(built).expect("alloc_loop/1 returned a proper list");
    assert_eq!(
        elements.len() as i64,
        ALLOCATIONS,
        "every iteration survived the pre-emption/yield/resume round trips"
    );
    for (index, element) in elements.iter().enumerate() {
        assert!(is_small(*element), "element {index} is a small integer");
        assert_eq!(
            signed_value(*element),
            index as i64 + 1,
            "element {index} is the value prepended at that step (the list is [1, 2, ...])"
        );
    }
    assert!(rt.process_result(short).is_some(), "the short process stayed finished");
    assert_eq!(rt.run_queue_len(), 0, "both processes terminated");
}

/// **The fairness proof.** A CPU-bound process spawned first is pre-empted after its slice,
/// so a short process spawned second runs and returns while the first is still unfinished.
///
/// This uses the committed image, so it runs whenever the system installation (for the boot
/// sequence) is present -- no `erlc` is needed.
#[test]
fn a_cpu_bound_process_is_pre_empted_so_a_short_process_gets_its_turn() {
    let Some(mut rt) = runtime_with(PREEMPT_BEAM) else {
        return;
    };
    assert_pre_emption_interleaving(&mut rt);
}

/// The same proof, against `preempt.erl` recompiled **at test time** with the system `erlc`.
///
/// It pins two things the committed-image test cannot: that the source still produces a module
/// with the property, and that the committed image is not hand-tweaked relative to its source.
/// Skipped **loudly** when the system `erl`/`erlc` is unavailable.
#[test]
fn a_freshly_compiled_fixture_shows_the_same_interleaving() {
    if support::require_oracle("preempt").is_none() {
        return;
    }
    let Some(erlc) = support::find_on_path("erlc") else {
        support::skip_loudly("preempt", "no `erlc` on $PATH");
        return;
    };
    let Some(fixture) = compile_fixture(&erlc) else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(&beam).expect("the freshly compiled fixture loads");

    assert_pre_emption_interleaving(&mut rt);

    fixture.cleanup();
}

/// A fixture compiled at test time in its own scratch directory.
struct Compiled {
    /// The scratch directory holding `preempt.beam`.
    dir: PathBuf,
    /// The freshly compiled `preempt.beam`.
    beam: PathBuf,
}

impl Compiled {
    /// Removes the scratch directory (`erlc` output is not source).
    fn cleanup(self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            println!("note: could not remove {}: {error}", self.dir.display());
        }
    }
}

/// Compiles the committed `preempt.erl` with `erlc` into a scratch directory, or reports a
/// loud skip and returns `None` when `erlc` cannot produce it.
fn compile_fixture(erlc: &Path) -> Option<Compiled> {
    let dir = std::env::temp_dir().join(format!("beam_rt_preempt_{}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly("preempt", &format!("cannot create {}: {error}", dir.display()));
        return None;
    }
    let output = match Command::new(erlc)
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(PREEMPT_ERL)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly("preempt", &format!("cannot run `{}`: {error}", erlc.display()));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            "preempt",
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join("preempt.beam");
    if !beam.is_file() {
        support::skip_loudly("preempt", &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Compiled { dir, beam })
}
