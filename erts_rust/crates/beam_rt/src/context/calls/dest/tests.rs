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
//! Units for [`super`] — the destination-containment short-circuit — on **both** copy-out arms.
//!
//! Each case asserts the pair that makes the short-circuit a *decision* rather than a no-op:
//! a result whose root already lives on the destination is returned as the **same word** (and the
//! destination records no new word), while a result on any other region is still copied — onto a
//! different address, with the destination's frontier advanced by exactly the graph's words. The
//! negative arm is the guard against "the copy silently became a no-op".
use beam_term::Eterm;
use beam_term::make_small;
use crate::context::Rehomed;
use crate::context::RuntimeContext;
use crate::context::calls::notes;
use crate::context::calls::tests::process_table;
/// An `n`-cell list of small ints on `heap` — `2n` real destination words the copier must write.
fn int_list(heap: &mut beam_alloc::Heap, n: i64) -> beam_common::Result<Eterm> {
    let mut list = beam_term::NIL;
    for i in (1..=n).rev() {
        list = heap.cons(make_small(i), list)?;
    }
    Ok(list)
}
/// A3 — the **process** arm's positive side: a container already on the process's scanned heap is
/// handed through unchanged, and no destination word is written for it.
///
/// This is `maps:put/3`'s no-op shape at the copy-out's door: `beam_bifs` returns the argument
/// word, the root already lives on the young heap, so the tail must not replace it by a twin —
/// which is what makes `erts_debug:same(maps:put(a,V,M), M)` answer `true`.
#[test]
fn a_result_already_on_the_process_heap_is_handed_through_unchanged() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // The term a no-op BIF would hand back: a real container on THIS process's scanned heap.
    let term = {
        let process = table.get_mut(pid).expect("the spawned process");
        int_list(process.heap_mut(), 4).expect("four cells fit the default nursery")
    };
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    notes::reset();
    let answer = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.move_result_into_process_heap(term, &[])
    };
    assert_eq!(
        answer,
        Some(term),
        "a result already on the destination is handed through as the same word, not copied"
    );
    assert_eq!(
        notes::dest_contained(),
        1,
        "the short-circuit (not a failed copy) is what answered"
    );
    let (attempts, grows, _) = notes::snapshot();
    assert_eq!(attempts, 0, "the copier never ran");
    assert_eq!(grows, 0, "no nursery growth was asked for");
    assert_eq!(
        table.get(pid).expect("the spawned process").heap().used(),
        used_before,
        "the destination wrote no word for a term it already held"
    );
}
/// A3 — the **process** arm's negative side: a container on a region the collector does **not**
/// scan is still copied out, so the short-circuit did not turn the copy-out into a no-op.
///
/// The term is built on a local `beam_alloc::Heap` — the shape a registered BIF hands back when it
/// really allocated (a fresh map, a rebuilt list) — and must land on the process's own young heap.
#[test]
fn a_result_on_another_region_is_still_copied() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let mut elsewhere = beam_alloc::Heap::new(32).expect("a local heap");
    let term = int_list(&mut elsewhere, 4).expect("four cells fit");
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    notes::reset();
    let answer = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.move_result_into_process_heap(term, &[])
    };
    let copied = answer.expect("a foreign container is re-homed");
    assert_ne!(
        copied, term,
        "a container on a region the collector does not scan must be copied, not handed through"
    );
    assert_eq!(
        notes::dest_contained(),
        0,
        "the short-circuit did not fire for a foreign container"
    );
    let (attempts, _, _) = notes::snapshot();
    assert_eq!(attempts, 1, "the copier ran exactly once");
    assert_eq!(
        table.get(pid).expect("the spawned process").heap().used(),
        used_before + 8,
        "the four cells (eight words) landed on the process's own heap"
    );
    // The copy is a real list word — never a mis-sized block.
    assert!(beam_term::prelude::is_list(copied));
}
/// A3 — the **store** arm (`-eval`/`-s`/`-run`) on both sides, with an **unknown origin**
/// (`pool == None`: the aliased-ETS `TermStore`, the `system_info` scratch region).
///
/// The destination here is the runtime's long-lived store, not a scanned process heap, so the
/// containment test is the store's own block. A term already in it is handed through (the
/// `-eval` route's `erts_debug:same` shape); a term on any other region is copied into it.
#[test]
fn the_store_arm_hands_through_what_it_already_holds_and_copies_what_it_does_not() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let held = int_list(&mut rt.eval_heap, 4).expect("four cells fit the store");
    // A region that is neither the store nor a scanned heap: exactly what a real registered BIF's
    // pooled result looks like to the store arm. It stays alive across both calls below.
    let mut elsewhere_heap = beam_alloc::Heap::new(32).expect("a local heap");
    let elsewhere = int_list(&mut elsewhere_heap, 4).expect("four cells fit");
    let store_used = rt.eval_heap.used();
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        assert_eq!(
            ctx.move_result_into_runtime_store(held, None),
            Rehomed::RewindSafe(held),
            "a result the store already holds is handed through as the same word"
        );
    }
    assert_eq!(
        rt.eval_heap.used(),
        store_used,
        "no fresh store word was written for a term it already held"
    );
    let copied = {
        let mut ctx = RuntimeContext::new(&mut rt);
        match ctx.move_result_into_runtime_store(elsewhere, None) {
            Rehomed::RewindSafe(copied) => copied,
            other => panic!("expected a foreign container to be copied into the store, got {other:?}"),
        }
    };
    assert_ne!(
        copied, elsewhere,
        "an unknown-origin result outside the store is still copied into it"
    );
    assert_eq!(
        rt.eval_heap.used(),
        store_used + 8,
        "the four cells (eight words) landed in the store"
    );
}
/// A3 — the **store** arm with a **known** pooled region: the copy has exactly one job (move the
/// result off the region the call allocated from and the tail is about to rewind), so a root
/// standing on **no** pooled region has nothing to move and is handed through — while a root still
/// on the pool is copied into the store.
///
/// The hand-through arm is what keeps a no-op container BIF's term identity observable on the
/// `-eval`/`-s`/`-run` route: the evaluator's own containers stand on the engine **arena**, which is
/// pointer-stable and never reset, so `erts_debug:same(maps:put(a,V,M), M)` answers `true` there
/// exactly as on the oracle. A root that still stands on the call's own pooled region is likewise
/// handed through by reference (`Rehomed::KeepPool`) rather than copied, because the pool is
/// pointer-stable and the next call's mark sits above its words — see [`super::move_result_into_runtime_store`].
#[test]
fn the_store_arm_hands_through_a_result_that_is_on_no_pooled_region() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    // The call's own pooled region, and a term the call really built on it — the shape the rewind
    // would free.
    let mut pool = beam_alloc::Heap::new(32).expect("a pooled region");
    let on_pool = int_list(&mut pool, 4).expect("four cells fit the pool");
    // A region the rewind cannot touch (the stand-in for the engine arena / a literal area).
    let mut stable = beam_alloc::Heap::new(32).expect("a stable region");
    let elsewhere = int_list(&mut stable, 4).expect("four cells fit");
    let store_used = rt.eval_heap.used();
    let kept = {
        let mut ctx = RuntimeContext::new(&mut rt);
        match ctx.move_result_into_runtime_store(on_pool, Some(&pool)) {
            Rehomed::KeepPool(kept) => kept,
            other => panic!("expected a pool-resident result to be handed through, got {other:?}"),
        }
    };
    assert_eq!(kept, on_pool, "a result on the pool is handed through by reference, never copied");
    assert_eq!(
        rt.eval_heap.used(),
        store_used,
        "the hand-through wrote no store word for a result the pool owns"
    );
    let handed = {
        let mut ctx = RuntimeContext::new(&mut rt);
        match ctx.move_result_into_runtime_store(elsewhere, Some(&pool)) {
            Rehomed::RewindSafe(handed) => handed,
            other => panic!("expected a stable-region result to be handed through, got {other:?}"),
        }
    };
    assert_eq!(
        handed, elsewhere,
        "a result on no pooled region has nothing to move and is handed through unchanged"
    );
    assert_eq!(
        rt.eval_heap.used(),
        store_used,
        "the hand-through arm wrote no store word"
    );
    // The pool's own words must still be recognisable as being on the pool, i.e. the predicate is
    // a region test and not "everything is safe now".
    assert!(
        crate::context::calls::dest::pool_holds(&rt, &pool, on_pool),
        "the pool half of the test still sees the term the call built"
    );
    assert!(!crate::context::calls::dest::pool_holds(&rt, &pool, elsewhere));
}
/// A3 — the **store** arm's GC guard: a container whose root already lives on the never-scanned
/// store but whose **child** lives on a scheduled process's scanned young heap is **never** handed
/// through. It is deep-copied whole, so a later collection of that process relocates the young heap
/// and leaves the returned term intact (the old code handed the word through, and the collection
/// left its slot naming a `0x3f` forwarding header — the D1c `error_info_abort_probe` from-space
/// CAR).
#[test]
fn a_store_root_with_scanned_heap_children_is_deep_copied_and_survives_a_forced_collection() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // A real scheduled process's young heap holds the child list; it is published as a root so the
    // forced collection relocates it (a garbage child would be overwritten, not forwarded).
    let child = {
        let process = table.get_mut(pid).expect("the spawned process");
        int_list(process.heap_mut(), 4).expect("four cells fit the nursery")
    };
    table
        .get_mut(pid)
        .expect("the spawned process")
        .registers_mut()
        .extra_roots_mut()
        .push(child);
    // The spawned process must be in the runtime's table before the guard runs: the walk classifies
    // "scanned" against the live processes.
    rt.put_processes(table);
    // The outer container stands on the runtime's store — the hand-through's positive root test —
    // but names a child on the scanned young heap.
    let outer = rt
        .eval_heap
        .tuple(&[child])
        .expect("the outer container on the store");
    assert!(
        crate::context::calls::dest::store_holds(&rt, outer),
        "the root really is on the store"
    );
    assert!(
        crate::context::calls::dest::graph_reaches_scanned_heap(&rt, outer),
        "the guard really sees the scanned-heap child"
    );
    let copied = {
        let mut ctx = RuntimeContext::new(&mut rt);
        match ctx.move_result_into_runtime_store(outer, None) {
            Rehomed::RewindSafe(copied) => copied,
            other => panic!("expected a mixed-heap graph to be re-homed, got {other:?}"),
        }
    };
    assert_ne!(
        copied, outer,
        "a store root with scanned-heap children must be deep-copied, never handed through"
    );
    // Force the spawned process's collection: its young heap moves, so the pre-fix hand-through
    // would have left `outer` (and therefore `copied`) pointing at the from-space.
    let mut table = rt.take_processes();
    rt.request_gc(pid);
    let collected = rt.serve_gc_request(&mut table, pid);
    rt.put_processes(table);
    assert!(collected, "the forced collection of the spawned process really ran");
    // The returned term is a self-contained structural twin of `{ [1,2,3,4] }`: its child was
    // deep-copied onto the store, so the collection moved no word it names.
    let atoms = rt.code_server().atoms_handle();
    let mut expected_heap = beam_alloc::Heap::new(32).expect("a local heap");
    let expected_child = int_list(&mut expected_heap, 4).expect("four cells fit");
    let expected = expected_heap
        .tuple(&[expected_child])
        .expect("the expected shape");
    assert!(
        beam_term::compare::eq(copied, expected, &atoms),
        "the re-homed container's contents are intact after the collection"
    );
}
