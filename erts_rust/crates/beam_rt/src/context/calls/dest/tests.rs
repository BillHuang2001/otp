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
        ctx.move_result_into_process_heap(term)
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
        ctx.move_result_into_process_heap(term)
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
/// A3 — the **store** arm (`-eval`/`-s`/`-run`) on both sides.
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
            ctx.move_result_into_runtime_store(held),
            Some(held),
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
        ctx.move_result_into_runtime_store(elsewhere)
            .expect("a foreign container is copied into the store")
    };
    assert_ne!(
        copied, elsewhere,
        "a container outside the store is still copied into it"
    );
    assert_eq!(
        rt.eval_heap.used(),
        store_used + 8,
        "the four cells (eight words) landed in the store"
    );
}
