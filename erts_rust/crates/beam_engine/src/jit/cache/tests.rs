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
//! these tests assert on — it counts calls to
//! [`module_fingerprint`](super::module_fingerprint), the **fallback** key derivation.
//!
//! Since the key is a real `.beam`'s O(1) **load-time digest**, a slice entry/resume no
//! longer hashes anything at all: the counter must stay untouched across the four entry
//! paths of a loaded module, and it moves exactly once per lookup only for a **synthetic**
//! (zero-checksum) module, which is what the fallback assertions pin.

use super::*;
use super::key::ModuleKey;
use beam_loader::{load_file, Block, Instr, Label, Operand};
use beam_term::{make_atom, make_small};

use crate::context::MinimalContext;
use crate::jit::Tier;
use crate::machine::test_support::{EmptyMailbox, Fixture};
/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// The engine's receive-family fixture — a real `.beam` whose `wait/0` is
/// `receive X -> X end`: it parks on an empty mailbox and re-parks on every resume, which
/// is the park+resume slice cycle a per-slice cost measurement needs.
const RECV_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recv.beam");

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

fn recv_module() -> Module {
    load_file(RECV_BEAM).expect("load recv.beam")
}

/// A **synthetic** (hand-built) module exporting `id/1` (which returns its argument).
///
/// `Module::default()` leaves the checksum all-zero, which no real `.beam` has: this is
/// exactly the shape that takes the [`ModuleKey::Content`] (content-hash) fallback rather
/// than the O(1) load-time digest. The function's blocks are cut the way the loader cuts
/// them (at every [`Instr::Label`]) because the baseline tier lowers those.
fn synthetic_module(name: &str) -> Module {
    let mut fixture = Fixture::new(name);
    let entry = fixture.add_function("id", 1, vec![Instr::Label(Label(1)), Instr::Return]);
    fixture.add_export("id", 1, entry);
    let mut module = fixture.finish();
    module.functions[0].blocks = vec![Block {
        label: Label(1),
        start: 0,
        end: 2,
    }];
    module
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
fn get_or_compile_reuses_the_artifact_for_the_same_module() {
    let module = slice_module();
    let mut cache = CodeCache::new();
    assert!(cache.is_empty());

    let range1 = cache
        .get_or_compile(&module)
        .expect("compile")
        .code_address_range();
    assert_eq!(cache.compiles(), 1);
    assert_eq!(cache.len(), 1);

    let range2 = cache
        .get_or_compile(&module)
        .expect("reuse")
        .code_address_range();
    // The same module object => the same key (identity **and** namespace) => the same
    // artifact.
    assert_eq!(cache.compiles(), 1, "a cache hit must not recompile");
    assert_eq!(cache.len(), 1);
    assert_eq!(range1, range2);
    assert!(cache.contains(&module));
}

/// Two separately loaded copies of one `.beam` share the **module half** of the key (the
/// container's MD5) but not the **namespace half**: each load carries its own
/// [`AtomTable`](beam_term::AtomTable) instance (`beam_loader::load` interns into a fresh
/// one and `AtomTable::clone` deliberately takes a fresh identity), so each stages its own
/// artifact. That is the documented trade — an extra compile, never a wrong answer: the
/// baseline tier bakes atom *indices* into its code, and two namespaces number the same
/// name differently.
#[test]
fn two_loads_of_one_beam_are_two_namespaces_and_two_entries() {
    let m1 = slice_module();
    let m2 = slice_module();
    assert_eq!(m1.checksum(), m2.checksum(), "one `.beam` is one digest");
    assert_ne!(
        m1.atom_table.snapshot(),
        m2.atom_table.snapshot(),
        "a separate load is a separate atom namespace"
    );

    let mut cache = CodeCache::new();
    let range1 = cache
        .get_or_compile(&m1)
        .expect("compile m1")
        .code_address_range();
    assert_eq!(cache.compiles(), 1);

    let range2 = cache
        .get_or_compile(&m2)
        .expect("compile m2")
        .code_address_range();
    assert_eq!(
        cache.compiles(),
        2,
        "a second namespace must not be handed the first's atom-indexed code"
    );
    assert_eq!(cache.len(), 2, "one entry per (module, namespace)");
    assert_ne!(range1, range2, "two artifacts, two mappings");

    // Each is still a hit for its own module.
    cache.get_or_compile(&m1).expect("m1 still hits");
    cache.get_or_compile(&m2).expect("m2 still hits");
    assert_eq!(cache.compiles(), 2, "both are cached; neither recompiles");
    assert_eq!(cache.len(), 2);
}
#[test]
fn different_content_with_the_same_name_does_not_collide() {
    // A **synthetic** module has no load-time digest, so its identity falls back to the
    // content hash — and two same-named modules with different bodies must still key
    // differently (the `Content` variant carries both the name and the fingerprint).
    let a = synthetic_module("collide");
    let mut b = a.clone();
    b.functions[0].code.push(Instr::NifStart);
    assert_eq!(a.atom_name(a.name), b.atom_name(b.name));
    assert_ne!(
        ModuleKey::of(&a),
        ModuleKey::of(&b),
        "the content fallback must distinguish differently-bodied synthetic modules"
    );

    let mut cache = CodeCache::new();
    cache.get_or_compile(&a).expect("compile a");
    assert!(cache.contains(&a));
    assert!(!cache.contains(&b), "a differently-bodied module must miss");

    cache.get_or_compile(&b).expect("compile b");
    assert_eq!(cache.compiles(), 2, "two distinct modules => two compiles");
    assert_eq!(cache.len(), 2, "distinct keys must not collide");
}

/// The **module half** of a real `.beam`'s key is its **load-time** MD5 digest, which —
/// unlike the content hash — is a property of the container, computed once by
/// `beam_loader` at the single `Module` construction site. So a post-load edit of the
/// decoded IR does not move it (the decoded IR `Debug`-differs, the digest does not).
///
/// The **namespace half** is what the clone below does move: `Module::clone` clones its
/// atom table, and `AtomTable::clone` deliberately takes a **fresh** identity (the two
/// tables may later diverge by independent appends), so the clone is a second namespace
/// and therefore a second entry. That is the documented namespace trade — an extra
/// compile for a re-loaded module, never a wrong answer. A post-load mutation of the
/// decoded IR is outside the cache's identity contract either way (and is not something a
/// host does — see the crate's post-load-mutation sweep in the cache `CONTEXT.md`); the
/// synthetic-module fallback above is how an edited-in-place module is meant to be keyed.
#[test]
fn the_module_half_of_a_real_beams_key_is_its_load_time_digest() {
    let a = slice_module();
    assert_ne!(a.checksum(), [0u8; 16], "a real `.beam` carries an MD5");
    let mut b = a.clone();
    b.functions[0].code.push(Instr::NifStart);
    // The decoded content differs (the hash sees it)...
    assert_ne!(module_fingerprint(&a), module_fingerprint(&b));
    // ...but the module half is the container's digest: the edit does not move it, and
    // it is exactly `Digest(checksum)`.
    assert_eq!(ModuleKey::of(&a), ModuleKey::of(&b));
    assert_eq!(ModuleKey::of(&a), ModuleKey::Digest(a.checksum()));
    // The namespace half separates the clone: a fresh atom-table identity, so a
    // distinct key.
    assert_ne!(
        CacheKey::of(&a),
        CacheKey::of(&b),
        "a cloned module is a fresh atom namespace"
    );

    let mut cache = CodeCache::new();
    let first = cache.compiled_handle(&a).expect("stage a");
    let second = cache.compiled_handle(&b).expect("stage b");
    assert!(
        !Rc::ptr_eq(&first, &second),
        "a fresh namespace gets its own atom-indexed artifact"
    );
    assert_eq!(cache.compiles(), 2);
    assert_eq!(cache.len(), 2);
    assert!(cache.contains(&a) && cache.contains(&b));

    // ...and each is still a hit for its own module.
    assert!(Rc::ptr_eq(
        &first,
        &cache.compiled_handle(&a).expect("a still hits")
    ));
    assert_eq!(cache.compiles(), 2, "neither recompiles");
    assert_eq!(cache.len(), 2);
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
    // A real `.beam` needs no memo at all (its digest key is O(1)), so this test's signal
    // is the *absence* of any hash on the memoized path, whichever mechanism served the
    // hit.
    let module = Arc::new(slice_module());
    let mut cache = CodeCache::new();

    // First resolution: the key is derived from the module's identity and the memo
    // is filled under the module's address — exactly what `cached_module_blocks`
    // does on the cross-module path.
    let first = cache.compiled_handle(&module).expect("stage the artifact");
    cache.remember(&module, &first);
    assert_eq!(cache.compiles(), 1);
    assert_eq!(cache.len(), 1);

    // A second lookup is an O(1) hit (the memo, or the digest behind it): the same
    // artifact, no re-hash, no compile.
    let before = fingerprint_calls();
    let second = cache.compiled_handle(&module).expect("memoized hit");
    assert_eq!(
        fingerprint_calls(),
        before,
        "a memoized hit must not re-hash the module's IR"
    );
    assert!(
        Rc::ptr_eq(&first, &second),
        "a hit returns the same artifact"
    );
    assert_eq!(cache.compiles(), 1, "a hit must not recompile");
    assert_eq!(cache.len(), 1);
}

#[test]
fn a_dead_memo_entry_is_ignored_and_falls_back_to_the_content_hash() {
    let mut cache = CodeCache::new();
    // Resolve a **synthetic** (zero-checksum) module — the shape that has no digest to
    // fall back on, so the lookup really must go through the content hash — and memoize
    // it under its address, then **drop** it: the memo entry now outlives its module, so
    // it must never be trusted again. Its atom table is carried over to the fresh module
    // below, so that the only thing the second lookup can key on is the content hash.
    let (first, table) = {
        let module = Arc::new(synthetic_module("dead_probe"));
        let handle = cache.compiled_handle(&module).expect("stage the artifact");
        cache.remember(&module, &handle);
        let unwrapped = Arc::try_unwrap(module)
            .expect("the cache holds only a Weak<Module>, so this is the last strong ref");
        (handle, unwrapped.atom_table)
    };

    // A separately built copy of the same content **in the same namespace**: its address
    // may or may not reuse the dead module's, but either way the lookup must ignore the
    // dead memo entry, fall back to the content hash and land on the identical-content
    // entry (never a stale pointer-keyed guess).
    let mut fresh = synthetic_module("dead_probe");
    fresh.atom_table = table;
    let before = fingerprint_calls();
    let second = cache.compiled_handle(&fresh).expect("content lookup");
    assert_eq!(
        fingerprint_calls(),
        before + 1,
        "a dead memo entry must fall back to the honest content hash"
    );
    assert!(
        Rc::ptr_eq(&first, &second),
        "identical content in one namespace resolves to the same artifact"
    );
    assert_eq!(cache.compiles(), 1, "the content hit must not recompile");
    assert_eq!(
        cache.len(),
        1,
        "no stale pointer key may add a second entry"
    );
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
        assert!(
            Rc::ptr_eq(&first, &again),
            "a hit returns the same artifact"
        );
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
            Instr::Wait { target: Label(1) }, // pc 5
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

/// **The negative control** — the one shape that still re-hashes: a **synthetic**
/// (zero-checksum) module, which has no load-time digest, so its key is the honest
/// content hash of the whole decoded IR and it must be derived on **every** slice.
///
/// A real `.beam` no longer takes this path at all (its key is the O(1) digest — pinned
/// by [`a_real_loaded_beam_never_rehashes_its_ir_on_any_entry_path`]); the assertions here
/// keep the fallback's meaning, and the counter is exact, which is what makes the primed
/// assertions below meaningful:
///
/// * the **first** entry into a cold cache costs the key's content hash plus, while
///   [`CompiledModule::lazy`] still keeps its own content fingerprint at construction,
///   one more inside the artifact it stages (so 1..=2 — the upper bound is the residual
///   one-time cold-staging hash owned by `src/jit/baseline/`, outside this file);
/// * **every later** entry costs exactly **one** — the key, since the artifact is
///   already cached.
#[test]
fn an_unprimed_entry_pays_the_content_hash_on_every_slice() {
    clear_baseline_cache();
    let module = Arc::new(synthetic_module("unprimed_probe"));
    assert_eq!(
        module.checksum(),
        [0u8; 16],
        "the fallback's premise: a synthetic module has no load-time digest"
    );
    let id = module
        .atom_table
        .lookup("id")
        .expect("the fixture interns `id`");
    let arg = make_small(7);

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let before = fingerprint_calls();
    let first = run_baseline_cached(&mut machine, &module, id, 1, &[arg], &mut ctx)
        .expect("the unprimed entry runs");
    let cold = fingerprint_calls() - before;
    assert!(
        (1..=2).contains(&cold),
        "the first entry into a cold cache derives the content key (one hash) and, while \
         the lazy artifact still stores its own content fingerprint, stages it (a second); \
         saw {cold}"
    );
    assert_eq!(first, arg, "`id/1` returns its argument");

    // Every later entry pays the key derivation again: nothing filled the address memo.
    let before = fingerprint_calls();
    let second = run_baseline_cached(&mut machine, &module, id, 1, &[arg], &mut ctx)
        .expect("the second unprimed entry runs");
    assert_eq!(
        fingerprint_calls(),
        before + 1,
        "an unprimed synthetic entry re-hashes the IR on every later slice"
    );
    assert_eq!(first, second, "both entries return the same value");
}

/// **The priming pin**: after `remember_module`, all four cached tier entries resolve in
/// O(1) and call `module_fingerprint` **zero** times, and each still reproduces the
/// interpreter's result exactly.
///
/// The four paths are `run_baseline_cached`, `run_baseline_cached_outcome`,
/// `run_baseline_cached_label_outcome` and `run_baseline_cached_resume_outcome` — the
/// complete set of ways the runtime enters or re-enters a module on the baseline tier.
/// (For a real `.beam` the O(1) load-time digest already makes them hash-free without any
/// priming — see [`a_real_loaded_beam_never_rehashes_its_ir_on_any_entry_path`]; this test
/// pins that priming neither breaks nor is needed by the digest key.)
#[test]
fn remember_module_priming_makes_all_four_entry_paths_rehash_free() {
    clear_baseline_cache();
    let module = Arc::new(slice_module());
    let pick = module.atom_table.lookup("pick").expect("pick is interned");
    let entry = module.export(pick, 1).expect("pick/1 is exported");
    let arg = make_atom(module.atom_table.lookup("a").expect("a is interned"));

    // Prime once: a real `.beam` keys on its O(1) digest, so priming derives **no**
    // content key; the only hash left on this path is the lazy artifact's own one-time
    // cold-staging fingerprint (`CompiledModule::lazy`, in `src/jit/baseline/` — outside
    // this file), which the digest supersedes and a sibling change may remove. Either
    // way every entry below is hash-free.
    let before = fingerprint_calls();
    remember_module(&module);
    let cold = fingerprint_calls() - before;
    assert!(
        cold <= 1,
        "priming a real module must not derive a content key (the digest is O(1)); at \
         most the lazy artifact's own one-time cold-staging hash remains; saw {cold}"
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
    assert_eq!(
        value, interpreter,
        "the baseline entry reproduces the interpreter"
    );

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
    let outcome =
        run_baseline_cached_label_outcome(&mut machine, &module, entry, 1, &[arg], &mut ctx)
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
        .run_export_outcome_with_tier(&parker, park, 0, &[], &mut reference_ctx, Tier::Interpreter)
        .expect("the interpreter parks");
    assert_eq!(
        wait, interpreter_wait,
        "both tiers park at the same address"
    );

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

    // Stage a lazy artifact through the real entry path, then prime.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let pick = module.atom_table.lookup("pick").expect("pick is interned");
    let arg = make_atom(module.atom_table.lookup("a").expect("a is interned"));
    run_baseline_cached_outcome(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the first entry stages the artifact");
    let staged = baseline_compile_count();
    assert_eq!(staged, 1, "one module, one stage");

    remember_module(&module);
    assert_eq!(
        baseline_compile_count(),
        staged,
        "priming must not re-stage"
    );
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

// ---------------------------------------------------------------------------
// The two-part key (real `.beam`) and its content-hash fallback (synthetic module)
//
// A real `.beam` carries a load-time MD5 (`Module::checksum`), read in O(1), so no slice
// entry/resume ever hashes the decoded IR. A synthetic module (`Module::default()`,
// checksum all-zero) has no digest, so its identity keeps the `(name, content
// fingerprint)` module half it always had. In both cases the key also carries the atom
// namespace the artifact was lowered against: the tier bakes atom *indices* into its
// code, so two namespaces (every separate load is one) are two entries — an extra
// compile, never a wrong answer.
// ---------------------------------------------------------------------------

/// Two independently loaded copies of one `.beam` carry the same load-time digest — the
/// **module** half of their key — but each load has its own atom namespace, so they stage
/// **two** artifacts and resolve to two distinct `Rc`s. The digest alone is no longer the
/// key (see [`a_second_atom_namespace_is_never_handed_the_first_namespaces_artifact`] for
/// the wrong-answer this prevents).
#[test]
fn two_loads_of_the_same_beam_share_the_digest_half_but_not_the_namespace_half() {
    let a = slice_module();
    let b = slice_module();
    assert_ne!(a.checksum(), [0u8; 16], "a real `.beam` carries an MD5");
    assert_eq!(a.checksum(), b.checksum(), "the digest is the container's");
    assert_eq!(ModuleKey::of(&a), ModuleKey::of(&b), "the module half collides");
    assert_ne!(
        CacheKey::of(&a),
        CacheKey::of(&b),
        "the namespace half does not"
    );

    let mut cache = CodeCache::new();
    let first = cache.compiled_handle(&a).expect("stage a");
    let second = cache.compiled_handle(&b).expect("stage b");
    assert!(
        !Rc::ptr_eq(&first, &second),
        "a separate load is a separate namespace => its own artifact"
    );
    assert_eq!(
        cache.compiles(),
        2,
        "the second load stages its own atom-indexed artifact"
    );
    assert_eq!(cache.len(), 2, "one entry per (module, namespace)");
    assert!(cache.contains(&a) && cache.contains(&b));
}
/// **The fallback pin**: a synthetic (zero-checksum) module has no digest, so its *module*
/// half is the content hash of its decoded IR — each lookup pays it, and two synthetic
/// modules key on their own contents (a same-named, same-content rebuild **in the same
/// namespace** keys identically).
#[test]
fn two_synthetic_modules_still_key_on_the_content_hash() {
    let mut a = synthetic_module("synthetic_a");
    let b = synthetic_module("synthetic_b");
    assert_eq!(a.checksum(), [0u8; 16]);
    assert_eq!(b.checksum(), [0u8; 16]);

    let mut cache = CodeCache::new();
    let before = fingerprint_calls();
    let first = cache.compiled_handle(&a).expect("stage a");
    let second = cache.compiled_handle(&b).expect("stage b");
    let hashed = fingerprint_calls() - before;
    assert!(
        hashed >= 2,
        "each synthetic lookup derives its key from the decoded IR (at least one content \
         hash each); saw {hashed}"
    );
    assert!(
        !Rc::ptr_eq(&first, &second),
        "the content fallback keeps two differently-named synthetic modules apart"
    );
    assert_eq!(cache.compiles(), 2);
    assert_eq!(cache.len(), 2);

    // A rebuild of `a`'s content keys identically — one content hash per lookup, and the
    // artifact it lands on is `a`'s. The rebuild has to be in `a`'s atom namespace for the
    // key to match (a fresh namespace would be, correctly, a third entry), so `a`'s table
    // is moved across.
    let mut rebuilt = synthetic_module("synthetic_a");
    rebuilt.atom_table = std::mem::replace(&mut a.atom_table, beam_term::AtomTable::new());
    let before = fingerprint_calls();
    let again = cache.compiled_handle(&rebuilt).expect("content hit");
    assert_eq!(
        fingerprint_calls(),
        before + 1,
        "a synthetic lookup derives exactly one content key"
    );
    assert!(Rc::ptr_eq(&first, &again));
    assert_eq!(cache.compiles(), 2, "a content hit must not recompile");
    assert_eq!(cache.len(), 2);
}
/// **The namespace pin — the second half of the key's reason to exist.**
///
/// The native baseline tier bakes atom **indices** into the emitted code: an IR
/// `Operand::Atom(a)` becomes the `Eterm` immediate `make_atom(a)`, and a block that
/// decides `pick(a) -> …; pick(_) -> …` compares that baked word against the argument.
/// A module's MD5 says nothing about those indices, so a cache keyed on the module alone
/// hands the second runtime in a thread the first runtime's atom-indexed code.
///
/// This test builds exactly that: **one thread**, one `.beam`'s bytes, two atom
/// namespaces. Module B's table has an extra name interned *first*, so every wire name
/// shifts by one — the shape `beam_rt`'s code server creates for every module it loads
/// (it interns into its shared table, then hands a clone to `lower::build_module`).
///
/// The program is the committed `slice.beam` fixture's `pick/1`
/// (`pick(a) -> 1; pick(_) -> 2.`), driven with the argument `a` **as the running
/// module's own table numbers it** — precisely the term the loader would produce. Run A
/// first (so A's artifact — and A's lowered `pick/1` block — is cached), then B:
///
/// * with the namespace in the key, B is a miss, compiles against B's table, and the
///   clause head matches: **1**, identical to B's interpreter-tier result;
/// * without it, B is a hit on A's artifact, whose baked atom is A's index for `a`, so
///   the head clause misses and the wildcard arm returns **2** — a silent wrong answer.
#[test]
fn a_second_atom_namespace_is_never_handed_the_first_namespaces_artifact() {
    clear_baseline_cache();

    let bytes = std::fs::read(SLICE_BEAM).expect("read slice.beam");
    let module_a = load_in_namespace(&bytes, None);
    let module_b = load_in_namespace(&bytes, Some("namespace_probe"));

    // The module half of the key collides: the very same `.beam`, hence the very same
    // container MD5.
    assert_ne!(module_a.checksum(), [0u8; 16], "a real `.beam` carries an MD5");
    assert_eq!(
        module_a.checksum(),
        module_b.checksum(),
        "the module half of the key is the container's digest, identical for both"
    );

    // The namespace half does not: the probe name occupies index 0, so every wire name
    // is one index higher in B.
    let a_in_a = module_a.atom_table.lookup("a").expect("A interns `a`");
    let a_in_b = module_b.atom_table.lookup("a").expect("B interns `a`");
    assert_eq!(
        module_b.atom_table.lookup("namespace_probe"),
        Some(Atom::from_raw(0)),
        "the probe name is interned first, so it takes index 0"
    );
    assert_eq!(
        a_in_b.raw(),
        a_in_a.raw() + 1,
        "the probe shifts every wire name's index by one"
    );
    assert_ne!(
        module_a.atom_table.snapshot(),
        module_b.atom_table.snapshot(),
        "two namespaces"
    );

    let pick_a = module_a.atom_table.lookup("pick").expect("A interns `pick`");
    let pick_b = module_b.atom_table.lookup("pick").expect("B interns `pick`");
    // The argument is the atom `a` **as the running module's own table numbers it**.
    let arg_a = make_atom(a_in_a);
    let arg_b = make_atom(a_in_b);

    // The interpreter is the reference on each side: both match the atom head clause.
    let mut reference = Machine::new();
    let mut reference_ctx = MinimalContext::new();
    let expected_a = reference
        .run_export(&module_a, pick_a, 1, &[arg_a], &mut reference_ctx)
        .expect("the interpreter runs A's pick/1");
    let expected_b = reference
        .run_export(&module_b, pick_b, 1, &[arg_b], &mut reference_ctx)
        .expect("the interpreter runs B's pick/1");
    assert_eq!(
        (expected_a, expected_b),
        (make_small(1), make_small(1)),
        "each namespace's own `a` matches the `pick(a)` head clause"
    );

    // A runs first: its artifact is staged and its `pick/1` block lowered out of A's
    // module, baking A's index for `a`.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let got_a = run_baseline_cached(&mut machine, &module_a, pick_a, 1, &[arg_a], &mut ctx)
        .expect("A's baseline run");
    assert_eq!(got_a, expected_a, "A matches its own baked atom");
    assert_eq!(baseline_compile_count(), 1, "A stages its own artifact");
    assert_eq!(baseline_cache_len(), 1);

    // B runs the same way. Reusing A's artifact would compare B's argument against A's
    // baked index and take the wildcard arm.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let got_b = run_baseline_cached(&mut machine, &module_b, pick_b, 1, &[arg_b], &mut ctx)
        .expect("B's baseline run");
    assert_eq!(
        got_b, expected_b,
        "B must be compiled against B's own atom namespace: reusing A's artifact compares \
         B's `a` against A's baked index for `a` and returns the wildcard clause"
    );
    assert_eq!(
        baseline_compile_count(),
        2,
        "B's namespace is a second key, so B stages its own artifact"
    );
    assert_eq!(baseline_cache_len(), 2);

    clear_baseline_cache();
}

/// Load `bytes` through the `beam_loader` public pipeline the way the runtime's code
/// server does — container → chunks → wire atom map → instruction decode → lowering — but
/// into a **caller-owned** namespace, with an optional `probe` name interned *first* so
/// every wire name's index shifts by one.
///
/// This is `beam_loader::load` with the one degree of freedom the cache key must witness:
/// the atom table. `build_module` re-interns the wire names idempotently, so the shifts
/// survive lowering.
fn load_in_namespace(bytes: &[u8], probe: Option<&str>) -> Module {
    let container = beam_loader::container::parse(bytes).expect("the container parses");
    let chunks = beam_loader::chunk::parse(&container).expect("the chunks parse");

    let mut table = beam_term::AtomTable::new();
    if let Some(probe) = probe {
        table.intern(probe);
    }
    // `atoms[0]` is the NIL placeholder, `atoms[1]` the module name — the same layout
    // `beam_loader::load` builds.
    let mut wire: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
    wire.push(Atom::MAX);
    for name in &chunks.atoms[1..] {
        wire.push(table.intern(name));
    }

    let raw = beam_loader::decode::decode_stream(
        chunks.code_stream,
        &wire,
        &chunks.code_version,
        &chunks.types,
    )
    .expect("the instruction stream decodes");

    beam_loader::lower::build_module(&chunks, &raw, table).expect("the module lowers")
}

/// **The digest key pin (real `.beam`), end to end**: once a real module's artifact is
/// staged, *every* slice entry and resume — the four `run_baseline_cached*` paths, a
/// `receive` park and its resume — resolves it with **zero** `module_fingerprint` calls
/// and **no** priming, and each still matches the interpreter.
#[test]
fn a_real_loaded_beam_never_rehashes_its_ir_on_any_entry_path() {
    clear_baseline_cache();
    let module = Arc::new(slice_module());
    assert_ne!(
        module.checksum(),
        [0u8; 16],
        "a real `.beam` carries an MD5"
    );
    let pick = module.atom_table.lookup("pick").expect("pick is interned");
    let entry = module.export(pick, 1).expect("pick/1 is exported");
    let arg = make_atom(module.atom_table.lookup("a").expect("a is interned"));
    let (recv, wait) = {
        let recv = Arc::new(recv_module());
        let wait = recv
            .atom_table
            .lookup("wait")
            .expect("the recv fixture interns `wait`");
        (recv, wait)
    };

    // ---- Staging: the ONE-TIME cold cost, reported rather than asserted to be zero. ----
    // The lazy artifact computes its own content fingerprint at construction
    // (`CompiledModule::lazy`, in `src/jit/baseline/` — outside this file), which the O(1)
    // digest supersedes; a sibling change may remove it. Either way it is paid once per
    // artifact and never again.
    let before_staging = fingerprint_calls();
    let mut cache = CodeCache::new();
    let staged = cache
        .compiled_handle(&module)
        .expect("stage on a local cache");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = run_baseline_cached(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("stage through the entry path");
    assert_eq!(value, make_small(1), "slice:pick(a) is 1");
    let mut parked = Machine::new();
    let mut parked_ctx = parking_context();
    let parked_outcome =
        run_baseline_cached_outcome(&mut parked, &recv, wait, 0, &[], &mut parked_ctx)
            .expect("stage recv:wait/0 and park");
    let (func, pc) = match parked_outcome {
        RunOutcome::Wait { func, pc } => (func, pc),
        other => panic!("recv:wait/0 must park on the empty mailbox, got {other:?}"),
    };
    let cold = fingerprint_calls() - before_staging;
    println!(
        "real-`.beam` cold staging: {cold} IR hashes for 3 artifact stagings (the residual \
         one-time `CompiledModule::lazy` construction hash, if baseline still pays it)"
    );
    assert!(
        cold <= 3,
        "staging a real module must not derive a content key per module (only the lazy \
         artifact's own one-time construction hash may remain); saw {cold}"
    );

    // ---- The measured window: every entry and resume must be hash-free. ----
    let before = fingerprint_calls();
    for _ in 0..3 {
        let again = cache.compiled_handle(&module).expect("digest lookup");
        assert!(
            Rc::ptr_eq(&staged, &again),
            "a digest lookup returns the same artifact"
        );
    }
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = run_baseline_cached(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the digest-keyed entry runs");
    assert_eq!(value, make_small(1));
    let outcome = run_baseline_cached_outcome(&mut machine, &module, pick, 1, &[arg], &mut ctx)
        .expect("the digest-keyed outcome entry runs");
    assert_eq!(outcome, RunOutcome::Returned(make_small(1)));
    let outcome =
        run_baseline_cached_label_outcome(&mut machine, &module, entry, 1, &[arg], &mut ctx)
            .expect("the digest-keyed label entry runs");
    assert_eq!(outcome, RunOutcome::Returned(make_small(1)));
    let resumed = run_baseline_cached_resume_outcome(&mut parked, &recv, func, pc, &mut parked_ctx)
        .expect("the digest-keyed resume re-parks");
    assert_eq!(
        fingerprint_calls(),
        before,
        "no entry or resume of a real `.beam` may re-hash its IR (the key is the load-time \
         digest), and none of them was primed"
    );

    // The parked address and the resume outcome are still the interpreter's.
    let mut reference = Machine::new();
    let mut reference_ctx = parking_context();
    let interpreter_wait = reference
        .run_export_outcome_with_tier(&recv, wait, 0, &[], &mut reference_ctx, Tier::Interpreter)
        .expect("the interpreter parks");
    assert_eq!(
        RunOutcome::Wait { func, pc },
        interpreter_wait,
        "both tiers park at the same address"
    );
    let interpreter_resumed = reference
        .resume_outcome_with_tier(&recv, func, pc, &mut reference_ctx, Tier::Interpreter)
        .expect("the interpreter re-parks");
    assert_eq!(
        resumed, interpreter_resumed,
        "the digest-keyed resume reproduces the interpreter"
    );
}

// ---------------------------------------------------------------------------
// Measurement: the per-slice entry/resume cost on both tiers
//
// The two-process copy-bound probe lives in the baseline tier's own tests; this is the
// cache's equivalent minimal probe — a real loaded `.beam` (`recv:wait/0`, whose resume
// re-parks on the empty mailbox) entered and re-entered many times, timed on BOTH tiers
// by name, with the deterministic `fingerprint_calls` counter as the assertion and
// `/proc/loadavg` recorded next to the (shared-host) ns/op figures.
// ---------------------------------------------------------------------------

/// Slice cycles per pass: the warmup populates the compiled-block cache, the measured
/// pass is what gets reported.
const SLICE_WARMUP: u32 = 20_000;
/// The measured pass's slice cycles.
const SLICE_MEASURE: u32 = 200_000;

/// `/proc/loadavg`, for the context a shared host's timing needs.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

/// Drive `cycles` slices of `recv:wait/0` on `tier`: one entry
/// ([`Machine::run_export_outcome_with_tier`]) and then one
/// [`Machine::resume_outcome_with_tier`] per further cycle — each resume re-parks on the
/// empty mailbox, so every cycle is exactly one entry or re-entry into the module, which
/// is the `CodeCache` lookup the digest key makes O(1).
///
/// Returns the cycle count and the `module_fingerprint` calls the pass paid.
fn recv_slice_cycles(recv: &Module, wait: Atom, tier: Tier, cycles: u32) -> (u32, u64) {
    let mut machine = Machine::new();
    let mut ctx = parking_context();
    let before = fingerprint_calls();
    let mut outcome = machine
        .run_export_outcome_with_tier(recv, wait, 0, &[], &mut ctx, tier)
        .expect("the entry parks");
    let mut done = 1;
    while done < cycles {
        let (func, pc) = match outcome {
            RunOutcome::Wait { func, pc } => (func, pc),
            other => panic!("recv:wait/0 must park on the empty mailbox, got {other:?}"),
        };
        outcome = machine
            .resume_outcome_with_tier(recv, func, pc, &mut ctx, tier)
            .expect("the resume re-parks");
        done += 1;
    }
    (done, fingerprint_calls() - before)
}

/// **The unprimed slice-cost measurement**, both tiers named: after ONE staging entry (out
/// of the timed window), a real `.beam`'s slice entries and resumes cost no IR hash on
/// either tier — the deterministic assertion — and their ns/op ratio is printed with
/// `/proc/loadavg` (the absolute figures only mean something on a shared host; the ratio
/// is what to quote).
#[test]
fn the_entry_and_resume_slice_cost_is_measured_on_both_tiers() {
    clear_baseline_cache();
    let recv = recv_module();
    let wait = recv
        .atom_table
        .lookup("wait")
        .expect("the recv fixture interns `wait`");

    // Stage the artifact once, outside the timed window, so the timed passes measure
    // steady state rather than first-entry compilation. Nothing primes anything: the
    // digest is the key.
    let _ = recv_slice_cycles(&recv, wait, Tier::BaselineJit, 4);

    println!("=== cache slice probe: recv:wait/0 entry + resume, unprimed ===");
    println!("loadavg (start): {}", loadavg());
    let mut measured = Vec::new();
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let _ = recv_slice_cycles(&recv, wait, tier, SLICE_WARMUP);
        let start = std::time::Instant::now();
        let (cycles, hashes) = recv_slice_cycles(&recv, wait, tier, SLICE_MEASURE);
        let elapsed = start.elapsed();
        assert_eq!(cycles, SLICE_MEASURE);
        let ns_per_slice = elapsed.as_nanos() as f64 / f64::from(SLICE_MEASURE);
        println!(
            "{:>12} tier: {:>9.2} ns/slice-entry over {} entries in {:?} ({} IR hashes)",
            tier.name(),
            ns_per_slice,
            SLICE_MEASURE,
            elapsed,
            hashes,
        );
        measured.push((tier.name(), ns_per_slice, hashes));
    }
    println!("loadavg (end): {}", loadavg());

    // The deterministic assertion: no tier may hash the module's IR on a slice.
    assert_eq!(
        measured[0].2, 0,
        "the interpreter tier never resolves a module through the JIT cache"
    );
    assert_eq!(
        measured[1].2, 0,
        "an UNPRIMED entry into a real `.beam` must be resolved by its load-time digest, \
         with zero IR hashes per slice"
    );
    println!(
        "baseline/interpreter ns-per-slice ratio: {:.2}x (unprimed; the digest key must \
         keep the baseline tier at parity — the pre-digest unprimed figure was >2x)",
        measured[1].1 / measured[0].1,
    );
}
