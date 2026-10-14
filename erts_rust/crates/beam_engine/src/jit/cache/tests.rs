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

//! In-module unit tests for the cross-run baseline-JIT [`CodeCache`](super::CodeCache).
//!
//! Split out of `mod.rs` when the cache plus its tests outgrew the ~1000-line budget:
//! the production cache stays in `mod.rs`, its tests live here. The test-only
//! [`fingerprint_calls`](super::fingerprint_calls) counter is the deterministic signal
//! these tests assert on — a memoized hit (cross-module or primed entry) must leave it
//! untouched, because the module's full decoded-IR re-hash is the cost the memo exists
//! to remove.

use super::*;
use beam_loader::{load_file, Block, Instr, Label, Operand};
use beam_term::make_atom;

use crate::context::MinimalContext;
use crate::jit::Tier;
use crate::machine::test_support::{EmptyMailbox, Fixture};
/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

#[test]
fn fingerprint_is_content_stable_across_separate_loads() {
    let a = slice_module();
    let b = slice_module();
    assert_eq!(module_fingerprint(&a), module_fingerprint(&b));
}

#[test]
fn fingerprint_distinguishes_a_mutated_copy_with_the_same_name() {
    let a = slice_module();
    let mut b = a.clone();
    // Different content, same module name.
    b.functions[0].code.push(Instr::NifStart);
    assert_eq!(a.atom_name(a.name), b.atom_name(b.name));
    assert_ne!(module_fingerprint(&a), module_fingerprint(&b));
}

#[test]
fn get_or_compile_reuses_the_artifact_for_reloaded_content() {
    let m1 = slice_module();
    let m2 = slice_module();
    let mut cache = CodeCache::new();
    assert!(cache.is_empty());

    let range1 = cache
        .get_or_compile(&m1)
        .expect("compile m1")
        .code_address_range();
    assert_eq!(cache.compiles(), 1);
    assert_eq!(cache.len(), 1);

    let range2 = cache
        .get_or_compile(&m2)
        .expect("compile m2")
        .code_address_range();
    // Same content (separately loaded) => same key => the *same* artifact.
    assert_eq!(cache.compiles(), 1, "a cache hit must not recompile");
    assert_eq!(cache.len(), 1);
    assert_eq!(range1, range2);
    assert!(cache.contains(&m1));
    assert!(cache.contains(&m2));
}

#[test]
fn different_content_with_the_same_name_does_not_collide() {
    let a = slice_module();
    let mut b = a.clone();
    b.functions[0].code.push(Instr::NifStart);

    let mut cache = CodeCache::new();
    cache.get_or_compile(&a).expect("compile a");
    assert!(cache.contains(&a));
    assert!(!cache.contains(&b), "a differently-bodied module must miss");

    cache.get_or_compile(&b).expect("compile b");
    assert_eq!(cache.compiles(), 2, "two distinct modules => two compiles");
    assert_eq!(cache.len(), 2, "distinct keys must not collide");
}

#[test]
fn clear_drops_entries_and_resets_the_counter() {
    let module = slice_module();
    let mut cache = CodeCache::new();
    cache.get_or_compile(&module).expect("compile");
    assert_eq!(cache.len(), 1);
    cache.clear();
    assert!(cache.is_empty());
    assert_eq!(cache.compiles(), 0);
}

#[test]
fn the_artifact_records_the_modules_content_fingerprint() {
    let module = slice_module();
    let mut cache = CodeCache::new();
    // `compiled_handle` stages a lazy artifact; it must carry the module's content
    // fingerprint so a memoized resolution can rebuild the key without re-hashing.
    let compiled = cache.compiled_handle(&module).expect("stage the artifact");
    assert_eq!(compiled.fingerprint(), module_fingerprint(&module));
}

#[test]
fn a_memoized_lookup_does_not_rehash_the_ir() {
    let module = Arc::new(slice_module());
    let mut cache = CodeCache::new();

    // First resolution: the key is derived from the content (one hash) and the memo
    // is filled under the module's address — exactly what `cached_module_blocks`
    // does on the cross-module path.
    let first = cache.compiled_handle(&module).expect("stage the artifact");
    cache.remember(&module, &first);
    assert_eq!(cache.compiles(), 1);
    assert_eq!(cache.len(), 1);

    // A second lookup is an O(1) memo hit: the same artifact, no re-hash, no compile.
    let before = fingerprint_calls();
    let second = cache.compiled_handle(&module).expect("memoized hit");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a memoized hit must not re-hash the module's IR"
    );
    assert!(Rc::ptr_eq(&first, &second), "a hit returns the same artifact");
    assert_eq!(cache.compiles(), 1, "a hit must not recompile");
    assert_eq!(cache.len(), 1);
}

#[test]
fn a_dead_memo_entry_is_ignored_and_falls_back_to_the_content_hash() {
    let mut cache = CodeCache::new();
    // Resolve a module and memoize it under its address, then **drop** it: the memo
    // entry now outlives its module, so it must never be trusted again.
    let first = {
        let module = Arc::new(slice_module());
        let handle = cache.compiled_handle(&module).expect("stage the artifact");
        cache.remember(&module, &handle);
        handle
    };

    // A separately loaded copy of the same content: its address may or may not reuse
    // the dead module's, but either way the lookup must fall back to the content hash
    // and land on the identical-content entry (never a stale pointer-keyed guess).
    let fresh = slice_module();
    let before = fingerprint_calls();
    let second = cache.compiled_handle(&fresh).expect("content lookup");
    assert_eq!(
        fingerprint_calls(),
        before + 1,
        "a dead memo entry must fall back to the honest content hash"
    );
    assert!(
        Rc::ptr_eq(&first, &second),
        "identical content resolves to the same artifact"
    );
    assert_eq!(cache.compiles(), 1, "the content hit must not recompile");
    assert_eq!(cache.len(), 1, "no stale pointer key may add a second entry");
}

#[test]
fn cached_module_blocks_reuses_the_memo_across_resolutions() {
    // The real cross-module path (the thread-local cache the native driver consults).
    clear_baseline_cache();
    let module = Arc::new(slice_module());

    // First resolution: one content hash, then the memo is filled.
    let first = cached_module_blocks(&module).expect("stage the callee's blocks");
    let hashes_after_first = fingerprint_calls();

    // Repeated resolutions are O(1): the same artifact, no re-hash.
    for _ in 0..3 {
        let again = cached_module_blocks(&module).expect("memoized resolution");
        assert!(Rc::ptr_eq(&first, &again), "a hit returns the same artifact");
    }
    assert_eq!(
        fingerprint_calls(),
        hashes_after_first,
        "repeated cross-module resolutions must not re-hash the IR"
    );
    assert_eq!(baseline_compile_count(), 1, "the module is staged once");

    clear_baseline_cache();
}

// ---------------------------------------------------------------------------
// The entry-module priming API (`remember_module`)
//
// `CodeCache::compiled_handle` takes a bare `&Module` on the tier-entry path, so its
// address memo can only be filled for the module a host is about to *enter* by a caller
// that holds that module's `Arc` — `remember_module`. Without it the ENTRY module (never
// a cross-module callee, the memo's other fill path) re-hashes its whole decoded IR on
// every slice entry/resume, which is the dominant baseline-tier cost on a message-passing
// shape. These tests pin the API with the test-only `fingerprint_calls` counter — a
// deterministic signal, not a wall-clock one — and cross-check every path's result
// against the interpreter.
// ---------------------------------------------------------------------------

/// A module whose export `park/0` **suspends**: the mailbox is always empty, so
/// `loop_rec` takes its fail label and the `wait` reports the loop head as the resume
/// address — the parked state [`run_baseline_cached_resume_outcome`] needs.
///
/// Built from the shared test fixture ([`Fixture`]); the fixture does not cut
/// `Function::blocks`, and the baseline tier lowers exactly those, so they are cut here
/// the way the loader cuts them — at every `Instr::Label`.
fn parker_module() -> (Arc<Module>, Atom) {
    let mut fixture = Fixture::new("parker");
    let entry = fixture.add_function(
        "park",
        0,
        vec![
            Instr::Label(Label(1)), // pc 0 — the entry AND the resume target
            Instr::LoopRec {
                fail: Label(2),
                dst: Operand::X(0),
            }, // pc 1
            Instr::RemoveMessage,   // pc 2
            Instr::Return,          // pc 3
            Instr::Label(Label(2)), // pc 4 — the empty-mailbox arm
            Instr::Wait {
                target: Label(1),
            }, // pc 5
        ],
    );
    fixture.add_export("park", 0, entry);
    let mut module = fixture.finish();
    module.functions[0].blocks = vec![
        Block {
            label: Label(1),
            start: 0,
            end: 4,
        },
        Block {
            label: Label(2),
            start: 4,
            end: 6,
        },
    ];
    let atom = module
        .atom_table
        .lookup("park")
        .expect("the fixture interns `park`");
    (Arc::new(module), atom)
}

/// A context with an always-empty mailbox: `loop_rec` fails at once and the run
/// suspends at the `wait`.
fn parking_context() -> MinimalContext {
    MinimalContext::with_mailbox(Box::new(EmptyMailbox))
}

/// **The negative control**: without priming, a tier entry pays `module_fingerprint`
/// on **every** slice — the honest content hash of the module's whole decoded IR.
///
/// This is the state `remember_module` removes: the entry path hands
/// `compiled_handle` a bare `&Module`, so it has no address memo to hit. The counter is
/// exact, which is what makes the primed assertions below meaningful:
///
/// * the **first** entry into a cold cache costs **two** hashes — one to derive the key
///   ([`ModuleKey::of`]) and one inside the artifact it then stages
///   ([`CompiledModule::lazy`] keeps its module's fingerprint);
/// * **every later** entry costs exactly **one** — the key, since the artifact is
///   already cached.
#[test]
fn an_unprimed_entry_pays_the_content_hash_on_every_slice() {
    clear_baseline_cache();
    let module = Arc::new(slice_module());
    let pick = module.atom_table.lookup("pick").expect("pick is interned");
    let arg = make_atom(module.atom_table.lookup("a").expect("a is interned"));

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let before = fingerprint_calls();
    let first = run_baseline_cached(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the unprimed entry runs");
    assert_eq!(
        fingerprint_calls(),
        before + 2,
        "the first entry into a cold cache derives the key and stages an artifact"
    );

    // Every later entry pays the key derivation again: nothing filled the address memo.
    let before = fingerprint_calls();
    let second = run_baseline_cached(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the second unprimed entry runs");
    assert_eq!(
        fingerprint_calls(),
        before + 1,
        "an unprimed entry re-hashes the IR on every later slice"
    );
    assert_eq!(first, second, "both entries return the same value");
}

/// **The priming pin**: after `remember_module`, all four cached tier entries are O(1)
/// memo hits that call `module_fingerprint` **zero** times, and each still reproduces
/// the interpreter's result exactly.
///
/// The four paths are `run_baseline_cached`, `run_baseline_cached_outcome`,
/// `run_baseline_cached_label_outcome` and `run_baseline_cached_resume_outcome` — the
/// complete set of ways the runtime enters or re-enters a module on the baseline tier.
#[test]
fn remember_module_priming_makes_all_four_entry_paths_rehash_free() {
    clear_baseline_cache();
    let module = Arc::new(slice_module());
    let pick = module.atom_table.lookup("pick").expect("pick is interned");
    let entry = module.export(pick, 1).expect("pick/1 is exported");
    let arg = make_atom(module.atom_table.lookup("a").expect("a is interned"));

    // Prime once: one fingerprint, then every entry is a memo hit.
    let before = fingerprint_calls();
    remember_module(&module);
    assert_eq!(
        fingerprint_calls(),
        before + 1,
        "priming derives the module's content fingerprint exactly once"
    );

    // The interpreter's answer for `pick(a)`, the reference every path is compared to.
    let mut reference = Machine::new();
    let mut reference_ctx = MinimalContext::new();
    let interpreter = reference
        .run_export(&module, pick, 1, &[arg], &mut reference_ctx)
        .expect("the interpreter runs pick/1");

    // 1. `run_baseline_cached` — the non-blocking entry.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let before = fingerprint_calls();
    let value = run_baseline_cached(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the primed entry runs");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a primed `run_baseline_cached` must not re-hash the IR"
    );
    assert_eq!(value, interpreter, "the baseline entry reproduces the interpreter");

    // 2. `run_baseline_cached_outcome` — the outcome-aware twin.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let before = fingerprint_calls();
    let outcome = run_baseline_cached_outcome(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the primed outcome entry runs");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a primed `run_baseline_cached_outcome` must not re-hash the IR"
    );
    assert_eq!(
        outcome,
        RunOutcome::Returned(interpreter),
        "the primed outcome entry reproduces the interpreter"
    );

    // 3. `run_baseline_cached_label_outcome` — the label-addressed entry.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let before = fingerprint_calls();
    let outcome = run_baseline_cached_label_outcome(
        &mut machine,
        &module,
        entry,
        1,
        &[arg],
        &mut ctx,
    )
    .expect("the primed label entry runs");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a primed `run_baseline_cached_label_outcome` must not re-hash the IR"
    );
    assert_eq!(
        outcome,
        RunOutcome::Returned(interpreter),
        "the primed label entry reproduces the interpreter"
    );

    // 4. `run_baseline_cached_resume_outcome` — park, then resume. A park is itself a
    // slice entry, so the parker module is primed the same way.
    let (parker, park) = parker_module();
    remember_module(&parker);
    let mut parked = Machine::new();
    let mut parked_ctx = parking_context();
    let before = fingerprint_calls();
    let wait = run_baseline_cached_outcome(&mut parked, &parker, park, 0, &[], &mut parked_ctx)
        .expect("the primed parker suspends");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a primed entry into a suspending module must not re-hash the IR"
    );
    let (func, pc) = match wait {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("the parker must suspend on the empty mailbox, got {other:?}"),
    };

    // The interpreter parks at the same address, and resumes to the same outcome.
    let mut reference = Machine::new();
    let mut reference_ctx = parking_context();
    let interpreter_wait = reference
        .run_export_outcome_with_tier(
            &parker,
            park,
            0,
            &[],
            &mut reference_ctx,
            Tier::Interpreter,
        )
        .expect("the interpreter parks");
    assert_eq!(wait, interpreter_wait, "both tiers park at the same address");

    let before = fingerprint_calls();
    let resumed =
        run_baseline_cached_resume_outcome(&mut parked, &parker, func, pc, &mut parked_ctx)
            .expect("the primed resume runs");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a primed `run_baseline_cached_resume_outcome` must not re-hash the IR"
    );
    let interpreter_resumed = reference
        .resume_outcome_with_tier(&parker, func, pc, &mut reference_ctx, Tier::Interpreter)
        .expect("the interpreter resumes");
    assert_eq!(
        resumed, interpreter_resumed,
        "the primed resume reproduces the interpreter"
    );
}

/// Priming is **idempotent** and **never replaces an existing artifact**: a second
/// `remember_module` derives no further fingerprint and leaves the cached artifact —
/// and its compiled code — exactly where it was.
#[test]
fn remember_module_is_idempotent_and_keeps_the_cached_artifact() {
    clear_baseline_cache();
    let module = Arc::new(slice_module());

    // Stage a lazy artifact through the real entry path (one content hash), then prime.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let pick = module.atom_table.lookup("pick").expect("pick is interned");
    let arg = make_atom(module.atom_table.lookup("a").expect("a is interned"));
    run_baseline_cached_outcome(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the first entry stages the artifact");
    let staged = baseline_compile_count();
    assert_eq!(staged, 1, "one module, one stage");

    remember_module(&module);
    assert_eq!(baseline_compile_count(), staged, "priming must not re-stage");
    assert_eq!(baseline_cache_len(), 1, "priming must not add an entry");

    // A second priming call is a no-op: the address is already memoized for a live
    // module, so it does not even re-derive the key.
    let before = fingerprint_calls();
    remember_module(&module);
    assert_eq!(
        fingerprint_calls(),
        before,
        "a repeat priming call must not re-hash the IR"
    );
    assert_eq!(baseline_compile_count(), staged);
    assert_eq!(baseline_cache_len(), 1);
}