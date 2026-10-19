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
//! The **cross-function in-block transfer** counters: how often a transfer has to
//! ask the compiled artifact for a function's block-entry tables
//! ([`super::runtime::cross_function_entry`] → `CompiledModule::native_entry_tables`)
//! and how often a per-run memo answers it instead
//! ([`super::runtime::CrossEntry`]).
//!
//! The transfer itself is not new: a **frame-preserving** local `call` into another
//! function of the module, and the `return` that lands back on the caller's
//! continuation, are both resolved in-block by
//! [`super::runtime::enter_in_block`] (its `resolve_resume_entry` first tries the
//! **published** `EntryTables` — the function the driver last entered — and only a
//! *different* function falls through to the artifact). This suite pins that split, so
//! the cost of the artifact lookup is a measured quantity rather than an assumption:
//!
//! * [`the_cross_function_transfers_of_a_loop_are_exactly_two_per_iteration`] is the
//!   **synthetic closed-form** pin — a BIF-free list walk whose every element makes one
//!   cross-function call and takes its return back, so the expected count is arithmetic;
//! * [`to_lower_cross_function_transfers_per_call`] is the **real workload** figure —
//!   Cowboy's `cowboy_bstr:to_lower/1`, a binary comprehension that calls
//!   `char_to_lower/1` once per byte, i.e. every transfer there is cross-function.
//!
//! Both are `#[test]`s (not `#[ignore]`d measurements): the structural counts are
//! deterministic, and only the real workload's *staging* is allowed to skip (a missing
//! checkout or `erlc` is a loud `SKIP:`, never a failure — the crate's convention).

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_loader::{HeapNeed, Instr, Operand, TestKind};
use beam_term::make_small;

use super::counters;
use super::tests::{build_module, export_atom};
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::{binary_bytes, make_heap_binary, make_list};
use crate::MinimalContext;

// ---------------------------------------------------------------------------
// the synthetic closed-form pin
// ---------------------------------------------------------------------------

/// The walk length the synthetic pin uses: small enough to read, long enough that the
/// cold part of the count cannot be confused with the steady state.
const ELEMENTS: usize = 8;

/// `loop/1` — a BIF-free list walk whose **every** element makes one frame-preserving
/// call into **another function** of the module and takes that callee's `return` back:
///
/// ```text
///   loop/1:   is_nonempty_list fail=base [x0]
///             get_list x0 x1 x0            ; head -> x1 (unused), tail -> x0
///             allocate 0 2
///             call 1 helper/1              ; NON-tail AND cross-function: the shape measured
///             deallocate 0
///             jump entry                   ; the walk continues in the same block
///   base:     x0 = 0
///             return
///   helper/1: return                       ; a leaf: hands its argument straight back
/// ```
///
/// No `gc_bif` appears, so a bare [`MinimalContext`] (which rejects every BIF) is enough
/// and the run cannot be perturbed by a host seam. `helper/1` allocates nothing, so its
/// `return` reads the caller's live frame — the continuation the `call` stored — which is
/// exactly the second cross-function transfer of each iteration.
fn loop_call_case() -> (beam_loader::Module, beam_loader::Atom) {
    let module = build_module(
        "jit_cross_memo",
        &[],
        vec![
            (
                "loop",
                1,
                vec![
                    Instr::Label(beam_loader::Label(1)),
                    Instr::Label(beam_loader::Label(2)),
                    Instr::Test {
                        kind: TestKind::IsNonemptyList,
                        fail: beam_loader::Label(5),
                        args: vec![Operand::X(0)],
                    },
                    Instr::GetList {
                        source: Operand::X(0),
                        head: Operand::X(1),
                        tail: Operand::X(0),
                    },
                    Instr::Allocate {
                        stack_need: HeapNeed::Words(0),
                        live: 2,
                    },
                    Instr::Call {
                        arity: 1,
                        target: beam_loader::Label(3),
                    },
                    Instr::Deallocate(0),
                    Instr::Jump {
                        target: beam_loader::Label(2),
                    },
                    Instr::Label(beam_loader::Label(5)),
                    Instr::Move {
                        src: Operand::Integer(0),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
            (
                "helper",
                1,
                vec![
                    Instr::Label(beam_loader::Label(3)),
                    Instr::Return,
                ],
            ),
        ],
        &[("loop", 1, beam_loader::Label(2))],
    );
    let name = export_atom(&module, "loop");
    (module, name)
}

/// The `[1, 1, …]` argument the walk consumes — one cell per iteration.
fn walk_list(ctx: &mut MinimalContext) -> beam_term::Eterm {
    let elements = vec![make_small(1); ELEMENTS];
    make_list(ctx, &elements).expect("the walk list builds")
}

/// Run [`loop_call_case`] once on the baseline tier with the structural counters
/// recording, and return the snapshot.
fn run_loop_case() -> counters::Counters {
    let (module, name) = loop_call_case();
    let mut ctx = MinimalContext::new();
    let list = walk_list(&mut ctx);
    counters::reset();
    counters::set_enabled(true);
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(
        &module,
        name,
        1,
        &[list],
        &mut ctx,
        Tier::BaselineJit,
    );
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    match outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value,
            make_small(0),
            "`loop/1` returns the `base` clause's constant"
        ),
        other => panic!("`loop/1` must complete on the baseline tier, got {other:?}"),
    }
    snapshot
}

/// **The synthetic closed-form pin.** Every iteration of the walk makes exactly two
/// cross-function transfers — the `call` into `helper/1` and the callee's `return` onto
/// the caller's continuation — and each of them names a function the *published*
/// [`EntryTables`](super::runtime::EntryTables) do not describe (the driver entered the
/// other one), so each one has to be resolved through the compiled artifact.
///
/// Hence the exact total `2 * ELEMENTS` = **16**: [`ELEMENTS`] calls and [`ELEMENTS`]
/// returns. Nothing else in the module transfers in-block (the `jump` is an
/// intra-function branch the linker resolves, the `test`/`get_list`/`move` are inlined
/// readers/writers and the `allocate`/`deallocate` are frame helpers), so the count is
/// arithmetic rather than empirical — a change in it means the transfer shape itself
/// changed.
///
/// The memo splits that total into exactly **2 resolutions + 14 hits**. The two are the
/// cold misses of an empty two-slot memo: the first transfer of the walk (the `call` into
/// `helper/1`) and the first transfer back into `loop/1` (the `return`, which names a
/// function the published tables — `helper/1`'s — do not describe). After those two the
/// memo holds `{helper, loop}`, which is the whole alternation, so all `16 - 2 = 14`
/// remaining transfers are answered by the memo. The cold count is a function of the
/// *number of distinct functions in the cycle*, not of [`ELEMENTS`]: grow the walk and the
/// hits grow with it while the resolutions stay at 2.
#[test]
fn the_cross_function_transfers_of_a_loop_are_exactly_two_per_iteration() {
    let snapshot = run_loop_case();
    println!("---");
    println!("workload = synthetic loop/1 over {ELEMENTS} elements");
    println!("cross_function_resolutions = {}", snapshot.cross_function_resolutions);
    println!("cross_function_memo_hits = {}", snapshot.cross_function_memo_hits);
    println!("in_block_calls = {}", snapshot.in_block_calls);
    println!("in_block_call_declines = {}", snapshot.in_block_call_declines);
    println!("driver_round_trips = {}", snapshot.driver_round_trips);
    assert_eq!(
        snapshot.in_block_calls, ELEMENTS as u64,
        "every iteration's `call` must transfer in-block (one per element)"
    );
    assert_eq!(
        snapshot.in_block_call_declines, 0,
        "no transfer may be declined: the artifact compiles both functions"
    );
    assert_eq!(
        snapshot.cross_function_resolutions + snapshot.cross_function_memo_hits,
        2 * ELEMENTS as u64,
        "every iteration transfers twice (call + return); resolutions: {}, hits: {}",
        snapshot.cross_function_resolutions,
        snapshot.cross_function_memo_hits,
    );
    // The memo changes WHERE a transfer is answered, never WHETHER one happens: the two
    // counters above still sum to the total, and the split is now the memo's cold misses
    // against its hits. This run is one `drive()`, i.e. one memo lifetime, so the two cold
    // misses are this walk's only artifact lookups.
    assert!(
        snapshot.cross_function_memo_hits > 0,
        "the per-run memo must answer the repeat transfers; hits: {}",
        snapshot.cross_function_memo_hits
    );
    assert_eq!(
        snapshot.cross_function_resolutions,
        2,
        "exactly the two cold misses of an empty two-slot memo reach the artifact; hits: {}",
        snapshot.cross_function_memo_hits
    );
    assert_eq!(
        snapshot.cross_function_memo_hits,
        2 * ELEMENTS as u64 - 2,
        "every transfer after the two cold misses is answered by the memo"
    );
    assert!(
        snapshot.cross_function_resolutions <= 2 * ELEMENTS as u64,
        "the memo can only lower the resolution count, never raise it"
    );
}
// ---------------------------------------------------------------------------
// the real workload: Cowboy's `cowboy_bstr:to_lower/1`
// ---------------------------------------------------------------------------

/// The number of measured calls in the real-workload probe.
const TO_LOWER_CALLS: usize = 200;

/// A fresh [`Machine`] (and so a fresh `refc_binaries` reservoir) is started this often:
/// `to_lower/1` builds one writable binary per call, and this probe never prunes the
/// reservoir, so one 200-call machine would leak instead of measuring anything.
const TO_LOWER_BATCH: usize = 100;

/// The probe input — mixed case, so `char_to_lower/1` really runs its `select_val` per
/// byte rather than falling through on the identity clause.
const TO_LOWER_INPUT: &[u8] = b"The Quick Brown Fox Jumps Ovr!";

/// Cowboy's `cowboy_bstr:to_lower/1`, staged from the read-only checkout by the system
/// `erlc` and run [`TO_LOWER_CALLS`] times on the baseline tier, reporting the
/// deterministic per-call cross-function transfer counts.
///
/// `to_lower/1` is a binary comprehension: its body calls `char_to_lower/1` **once per
/// byte** and each of those calls is a frame-preserving cross-function `call` whose
/// `return` lands back in the comprehension, so the per-call transfer count is
/// `2 * input_len` (call + return per byte) — the shape a per-run memo exists for.
///
/// Skips loudly when the checkout or `erlc` is absent (`erlc` is an external tool, never
/// a gate).
#[test]
fn to_lower_cross_function_transfers_per_call() {
    let repo = std::env::var("BENCH_COWBOY_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/home/bill/Source/cowboy"));
    let source = repo.join("src/cowboy_bstr.erl");
    if !source.is_file() {
        println!(
            "SKIP: {} is not a file (set BENCH_COWBOY_REPO to the checkout root) -- the \
             to_lower transfer probe did not run",
            source.display()
        );
        return;
    }
    let beam = match stage(&source, "cowboy_bstr") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the to_lower transfer probe did not run");
            return;
        }
    };
    let module = match beam_loader::load_file(&beam) {
        Ok(module) => module,
        Err(error) => {
            println!(
                "SKIP: load {}: {error:?} -- the to_lower transfer probe did not run",
                beam.display()
            );
            return;
        }
    };
    let Some(entry) = module.atom_table.lookup("to_lower") else {
        println!("SKIP: `cowboy_bstr` exports no `to_lower/1` -- the probe did not run");
        return;
    };
    let mut ctx = MinimalContext::new();
    let input = make_heap_binary(&mut ctx, TO_LOWER_INPUT).expect("the input binary builds");

    counters::reset();
    counters::set_enabled(true);
    let mut done = 0usize;
    let mut measured = 0usize;
    while done < TO_LOWER_CALLS {
        let mut machine = Machine::new();
        for _ in 0..TO_LOWER_BATCH.min(TO_LOWER_CALLS - done) {
            let outcome = machine.run_export_outcome_with_tier(
                &module,
                entry,
                1,
                &[input],
                &mut ctx,
                Tier::BaselineJit,
            );
            match outcome {
                Ok(RunOutcome::Returned(value)) => {
                    let bytes = binary_bytes(value).expect("`to_lower/1` returns a binary");
                    assert_eq!(
                        bytes,
                        TO_LOWER_INPUT.to_ascii_lowercase().as_slice(),
                        "the lowered binary must be the input's lower case"
                    );
                }
                other => panic!("`to_lower/1` must complete on the baseline tier, got {other:?}"),
            }
            done += 1;
            measured += 1;
        }
    }
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    let calls = measured as f64;
    let per_call = |count: u64| count as f64 / calls;
    println!("---");
    println!("workload = cowboy_bstr:to_lower/1 over {} bytes", TO_LOWER_INPUT.len());
    println!("calls = {measured} (fresh Machine every {TO_LOWER_BATCH})");
    println!("cross_function_resolutions_per_call = {:.4}", per_call(snapshot.cross_function_resolutions));
    println!("cross_function_memo_hits_per_call = {:.4}", per_call(snapshot.cross_function_memo_hits));
    println!("in_block_calls_per_call = {:.4}", per_call(snapshot.in_block_calls));
    println!("in_block_call_declines_per_call = {:.4}", per_call(snapshot.in_block_call_declines));
    println!("driver_round_trips_per_call = {:.4}", per_call(snapshot.driver_round_trips));
    // One cross-function call per input byte, plus one `return` per call.
    let transfers_per_call = 2 * TO_LOWER_INPUT.len() as u64;
    let total = snapshot.cross_function_resolutions + snapshot.cross_function_memo_hits;
    println!("transfers_per_call = {transfers_per_call}");
    println!("total_transfers = {total}");
    assert_eq!(
        total,
        transfers_per_call * measured as u64,
        "every byte transfers twice (call + return); resolutions: {}, hits: {}",
        snapshot.cross_function_resolutions,
        snapshot.cross_function_memo_hits,
    );
    // The memo split. Each measured call is its own `run_export_outcome_with_tier`, hence
    // its own `drive()` and its own memo lifetime, so each one pays exactly the two cold
    // misses of an empty two-slot memo — `to_lower/1` → `char_to_lower/1` and the return
    // back — and the memo answers the remaining `2 * len - 2` transfers of that call. The
    // cold count is therefore a property of the caller/callee cycle, and the saving grows
    // with the input: `2` artifact lookups instead of `2 * len`.
    assert!(
        snapshot.cross_function_memo_hits > 0,
        "the per-run memo must answer the repeat per-byte transfers; hits: {}",
        snapshot.cross_function_memo_hits
    );
    assert_eq!(
        snapshot.cross_function_resolutions,
        2 * measured as u64,
        "one cold caller/callee cycle per call (each call is one memo lifetime); \
         hits: {}",
        snapshot.cross_function_memo_hits
    );
}
/// Stage `source` into a per-process temp dir and compile it with the system `erlc
/// +deterministic`, returning the produced `.beam` path.
///
/// The same invocation the crate's measurement harness uses (`erlc` is an external
/// tool, so a missing/unusable one is a `String` reason the caller skips loudly on).
fn stage(source: &Path, module: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("beam_engine_cross_{}", std::process::id()));
    let path = dir.join(format!("{module}.erl"));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    std::fs::copy(source, &path).map_err(|error| {
        format!(
            "copy {} -> {}: {error}",
            source.display(),
            path.display()
        )
    })?;
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&path)
        .output()
        .map_err(|error| format!("no usable `erlc` on PATH ({error})"))?;
    if !output.status.success() {
        return Err(format!(
            "`erlc` exited {} on {}: {}",
            output.status,
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(dir.join(format!("{module}.beam")))
}