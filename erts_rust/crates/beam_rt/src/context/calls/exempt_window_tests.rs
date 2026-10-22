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

//! The **young-space-replacement window** pins for the copy-out
//! ([`RuntimeContext::move_result_into_process_heap`]).
//!
//! A growth retires the space it replaces and keeps every handed-out word addressable at its
//! original address only until the next **collection collapses the chain**, and that collection
//! runs at a non-exempt admission door whose root set cannot name the words a relocation-exempt
//! window holds on the Rust side. So a space **replacement** is legal only while the process's
//! root set is complete — one rule, one definition
//! ([`super::super::gc::may_replace_young_space`]), consulted by this copy-out and by the two
//! `gc.rs` allocation seams.
//!
//! The cases below are that rule's full boundary: inside the window an oversized copy-out never
//! replaces the space and is **stabilized** onto the runtime's pointer-stable store instead
//! (**the pin** — `refuse ⇒ stabilize, never dangle`), the window still re-homes what fits, the
//! stabilized result licenses the caller's pool rewind like any other copy, and the same oversized
//! copy-out grows the nursery again the moment the window closes — so the gate is window-scoped
//! rather than a permanent refusal.

use std::rc::Rc;

use beam_bifs::BifContext;
use beam_common::Result;
use beam_engine::ExecutionContext;
use beam_process::SignalAtoms;
use beam_process::ProcessId;
use beam_term::make_small;
use beam_term::NIL;

use super::*;

/// A `runtime` + a caller-owned `ProcessTable` holding one default process, plus the pid — the
/// `for_process` shape, i.e. the one where the copy-out (and its retry) is ON.
///
/// The same fixture [`super::tests`] builds; reused rather than re-written so the two suites
/// cannot drift.
fn process_table(rt: &mut crate::runtime::Runtime) -> (ProcessTable, ProcessId) {
    let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
    let mut table = ProcessTable::new(signals);
    let pid = table.spawn_default().expect("a default process");
    (table, pid)
}

/// An `n`-cell list of small ints on `heap` — `2n` words, every one a real heap word the copier
/// must write into the destination.
fn int_list(heap: &mut beam_alloc::Heap, n: i64) -> Result<Eterm> {
    let mut list = beam_term::prelude::NIL;
    for i in (1..=n).rev() {
        list = heap.cons(make_small(i), list)?;
    }
    Ok(list)
}

/// Whether `term`'s own pointer lands inside the runtime's long-lived **store**
/// ([`Runtime::eval_heap`](crate::runtime::Runtime::eval_heap)) — the pointer-stable, never-reset
/// destination of [`RuntimeContext::move_result_into_runtime_store`], i.e. the region a
/// stabilized copy-out must land on.
///
/// The check is on the **term's own address only** (no dereference): an immediate carries no
/// address and a literal lives in its module's frozen area, so both answer `false` by the same
/// reasoning that makes them immune to a pool rewind.
fn lies_in_the_store(rt: &crate::runtime::Runtime, term: Eterm) -> bool {
    if !(beam_term::prelude::is_list(term) || beam_term::prelude::is_boxed(term)) {
        return false;
    }
    let address = beam_term::prelude::ptr_val(term) as usize & !0b11;
    let hp = rt.eval_heap.hp() as usize;
    let base = hp - rt.eval_heap.used() * core::mem::size_of::<Eterm>();
    address >= base && address < hp
}

/// The pooled BIF heap's frontier, i.e. how many words the region the next call allocates on is
/// currently handing out. A call that copied its container result out has rewound it to its own
/// `mark`; a call that could not answers with a non-zero figure, which is the live word's address
/// space.
fn pool_frontier_used(rt: &crate::runtime::Runtime) -> usize {
    rt.bif_heaps.last().expect("the pool keeps a region").used()
}

/// An `n`-cell list of small ints built on the **pooled BIF heap** — the shape a registered BIF
/// hands back, so the fixture below returns a genuine foreign container result.
const POOL_CELLS: usize = 500;

fn pool_int_list(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let mut list = NIL;
    for i in (1..=POOL_CELLS as i64).rev() {
        list = ctx.heap().cons(make_small(i), list)?;
    }
    Ok(list)
}

/// The same rule at the **allocation seams** ([`super::super::gc::alloc_words`] /
/// [`alloc_words_rooted`](super::super::gc::alloc_words_rooted)): an exempt application that asks
/// for more words than the nursery has free gets the pointer-stable **arena** block and replaces
/// nothing.
///
/// The seam is the other half of the fix — it is the one a *non*-copy-out allocation reaches — and
/// its failure mode is the same dangling word, so it carries the same pin: the block must not come
/// from the young heap (`HeapSpace::contains` is the discriminator, not a guess), and the space
/// count and capacity must be untouched.
#[test]
fn an_exempt_allocation_seam_does_not_replace_the_young_space() {
    /// Words to request: well past the default 233-word nursery, far inside the arena's capacity.
    const WORDS: usize = 4_000;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let available = table.get(pid).expect("the spawned process").heap().available();
    assert!(
        WORDS > available,
        "the request must not fit: {WORDS} words against {available} free"
    );
    assert!(
        rt.gc_process_enabled(pid),
        "the seam only reaches the young heap for a GC-enabled process"
    );
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let capacity_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    table
        .set_relocation_exempt(pid, true)
        .expect("the process is exempt");
    let block = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        super::super::gc::alloc_words(&mut ctx, WORDS).expect("the arena serves the block")
    };
    let process = table.get(pid).expect("the spawned process");
    assert!(
        !process.collector().young().contains(block),
        "an exempt window must not hand the block out of a replaced young space"
    );
    assert_eq!(
        process.collector().young_space_count(),
        spaces_before,
        "the allocation seam must not replace the young space inside the window"
    );
    assert_eq!(
        process.collector().young().capacity_words(),
        capacity_before,
        "and must not install a larger current space either"
    );
    assert!(
        !process.collection_debt(),
        "nothing was retired, so nothing owes a collapsing collection"
    );
}

/// The **pin**: an oversized copy-out taken inside a relocation-exempt window leaves the young
/// space exactly as it found it, and answers a **stabilized** copy on the runtime store.
///
/// The graph is provably larger than the destination's free words (asserted, not assumed), so the
/// copy can only succeed on the young heap by **replacing** it — which a window whose root set is
/// incomplete may not do. The rule is `refuse ⇒ stabilize, never dangle`: instead of either a
/// growth a later chain collapse revokes or a live `Eterm` left on a pooled region the caller's
/// own rewind then recycles, the result is copied onto the runtime's pointer-stable, never-reset
/// store and that copy is the answer.
///
/// Every observation is on the process itself, on the store and on the copy-out's own structural
/// counters, so a future change cannot satisfy this case by anything but the rule:
///
/// * the answer is `Some(copy)` whose graph is **intact** (same flat word count as the source) and
///   which lies on the store — **not** on the process young heap, which is what the pre-rule code
///   could not produce (it answered `None`);
/// * `young_space_count` and the current space's `capacity_words` are unchanged — **no**
///   replacement;
/// * `collection_debt` stays `false` — nothing was retired, so nothing owes a collection;
/// * neither `notes` growth counter moved, so neither growth site ran: not the learned-hint gate
///   (which would otherwise grow *before* the first attempt, because the hint is set) and not the
///   post-refusal retry;
/// * the copy is still **attempted** — once on the process young heap and, when that refuses, once
///   more on the store (`attempts >= 2`) — so the rule declines the *replacement* and not the
///   retention.
#[test]
fn an_exempt_copy_out_never_replaces_the_young_space() {
    /// Cells in the source graph: 500 cons cells = 1 000 words, against a 233-word nursery.
    const CELLS: i64 = 500;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let mut source = beam_alloc::Heap::new(2 * CELLS as usize + 64).expect("the source heap");
    let container = int_list(&mut source, CELLS).expect("the source graph");
    // The precondition the case depends on, asserted rather than assumed: the graph does NOT fit
    // the destination, so only a replacement could make the copy succeed there.
    let available = table.get(pid).expect("the spawned process").heap().available();
    let graph_words = beam_term::size_object(container);
    assert!(
        graph_words > available,
        "the source graph must not fit the destination: {graph_words} words against {available} free"
    );
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let capacity_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    assert_eq!(
        rt.eval_heap.used(),
        0,
        "the store starts empty, so the copy is the only thing on it"
    );
    // The window is opened the way `RuntimeContext::with_relocation_exempt` opens it: the
    // process's own mark, on the table the context runs against.
    table
        .set_relocation_exempt(pid, true)
        .expect("the process is exempt");
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // A learned hint of exactly this graph: the gate would otherwise spend it *before* the
        // first attempt, which is itself a replacement — so the window must decline that too.
        ctx.copy_out_hint = graph_words;
        notes::reset();
        ctx.move_result_into_process_heap(container)
    };
    let copy = copy.expect(
        "an exempt window STABILIZES a result it may not grow the space for: the answer must be \
         the copy on the runtime store, never `None`",
    );
    {
        let process = table.get(pid).expect("the spawned process");
        assert_eq!(
            process.collector().young_space_count(),
            spaces_before,
            "an exempt copy-out must not REPLACE the young space"
        );
        assert_eq!(
            process.collector().young().capacity_words(),
            capacity_before,
            "and must not install a larger current space either"
        );
        assert!(
            !process.collection_debt(),
            "nothing was retired, so nothing owes a collapsing collection"
        );
        assert!(
            !process.collector().young().contains_term(copy),
            "the answer must not be a word of the (unreplaceable) young space"
        );
    }
    assert_eq!(
        beam_term::size_object(copy),
        graph_words,
        "the stabilized copy is the whole graph, not a truncated spine"
    );
    assert!(
        lies_in_the_store(&rt, copy),
        "the copy lives on the runtime's pointer-stable store — the destination a rewind and a \
         chain collapse cannot reach"
    );
    assert_eq!(
        rt.eval_heap.used(),
        graph_words,
        "the copy — and only the copy — is on the store"
    );
    let (attempts, grows, _empties) = notes::snapshot();
    assert_eq!(
        grows, 0,
        "neither growth site may run inside the window: the hint gate is gated on the same rule \
         and the post-refusal retry is not reached"
    );
    assert_eq!(
        notes::hint_grows(),
        0,
        "the learned-hint gate is a replacement too, so it is skipped even with a hint set"
    );
    assert!(
        attempts >= 2,
        "the copy is still ATTEMPTED inside the window — the rule declines the replacement, not \
         the retention — and the stabilized retry on the store is a second attempt (got {attempts})"
    );
    // The value is intact end to end: the renderer walks the copy on the store.
    let rendered = rt
        .format_term_w(copy, None)
        .expect("the stabilized copy renders");
    let rendered = rendered.trim_end();
    assert!(
        rendered.starts_with("[1,2,3,4,5,") && rendered.ends_with(",499,500]"),
        "the stabilized copy is the whole list: {rendered}"
    );
}

/// The stabilized copy **licenses the caller's pool rewind**, which is the defect the refusal
/// class produced: a `None` from the copy-out left the result on the pooled BIF region the
/// caller then did not rewind, so the value kept pointing at words a *later* call would recycle.
///
/// This drives the real dispatch (`call_bif` → `call_registered` → the copy-out) on a registered
/// fixture whose result is a 500-cell list built on the pool: inside the exempt window the young
/// space is untouched, the returned value is a self-contained store copy, and the pool frontier
/// returns to the call's `mark` — so the same call is now safe on both counts.
#[test]
fn an_exempt_copy_out_that_cannot_fit_still_licenses_the_pool_rewind() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    {
        let registry = Rc::get_mut(&mut rt.registry).expect("the runtime holds the only handle");
        let _ = registry.register("exempt_fixture", "big_list", 0, pool_int_list);
    }
    let (mut table, pid) = process_table(&mut rt);
    let available = table.get(pid).expect("the spawned process").heap().available();
    assert!(
        2 * POOL_CELLS > available,
        "the fixture's graph must not fit the destination: {} words against {available} free",
        2 * POOL_CELLS
    );
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let capacity_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    assert_eq!(pool_frontier_used(&rt), 0, "the pool starts clean");
    table
        .set_relocation_exempt(pid, true)
        .expect("the process is exempt");
    let result = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        ctx.call_bif("exempt_fixture", "big_list", 0, &[])
            .expect("the fixture BIF returns")
    };
    assert_eq!(
        pool_frontier_used(&rt),
        0,
        "a stabilized copy-out rewinds the pool exactly as a grown one does — the result is no \
         longer left on a region a later call recycles"
    );
    {
        let process = table.get(pid).expect("the spawned process");
        assert_eq!(
            process.collector().young_space_count(),
            spaces_before,
            "the window still replaces nothing"
        );
        assert_eq!(
            process.collector().young().capacity_words(),
            capacity_before,
            "and installs no larger current space"
        );
        assert!(
            !process.collector().young().contains_term(result),
            "the returned value is not a young-space word"
        );
    }
    assert!(
        lies_in_the_store(&rt, result),
        "the returned value lives on the runtime store"
    );
    assert_eq!(
        beam_term::size_object(result),
        2 * POOL_CELLS,
        "and is the fixture's whole graph"
    );
    let rendered = rt
        .format_term_w(result, None)
        .expect("the returned value renders");
    let rendered = rendered.trim_end();
    assert!(
        rendered.starts_with("[1,2,3,4,5,") && rendered.ends_with(",499,500]"),
        "the returned value is intact after the rewind: {rendered}"
    );
    let (_attempts, grows, _empties) = notes::snapshot();
    assert_eq!(grows, 0, "no growth ran inside the window");
}
/// The boundary the other way: a container that **fits** is still re-homed inside the window.
///
/// An exempt window needs the retention as much as any other route — the pooled BIF heap is a
/// region no process collector scans and a later call's rewind recycles — so refusing every copy
/// inside it would be a regression, not a fix. Nothing is replaced here, so the rule has nothing
/// to decline.
#[test]
fn an_exempt_copy_out_still_moves_a_container_that_fits() {
    /// Cells in the source graph: a 5-cell list is 10 words, far inside the 233-word nursery.
    const CELLS: i64 = 5;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let mut source = beam_alloc::Heap::new(64).expect("the source heap");
    let container = int_list(&mut source, CELLS).expect("the source graph");
    let available = table.get(pid).expect("the spawned process").heap().available();
    let graph_words = beam_term::size_object(container);
    assert!(
        graph_words <= available,
        "the source graph must fit the destination: {graph_words} words against {available} free"
    );
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    table
        .set_relocation_exempt(pid, true)
        .expect("the process is exempt");
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        ctx.move_result_into_process_heap(container)
    };
    let copy = copy.expect("a graph that fits is copied out even inside an exempt window");
    assert!(
        beam_term::prelude::is_list(copy) && !beam_term::literal::is_literal(copy),
        "the copy lives on the process's own heap"
    );
    assert_eq!(
        table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count(),
        spaces_before,
        "a copy that fits replaces nothing"
    );
    let (_attempts, grows, _empties) = notes::snapshot();
    assert_eq!(grows, 0, "and grows nothing");
}

/// The gate is **window-scoped**, not a permanent refusal: the very same oversized copy-out grows
/// the destination once the exemption is cleared.
///
/// This is what makes the first case a statement about the *window* rather than about the graph:
/// the refusal disappears with the condition that caused it.
#[test]
fn the_same_copy_out_grows_once_the_window_closes() {
    /// Cells in the source graph — identical to the pin's, so the only difference between the two
    /// cases is the window.
    const CELLS: i64 = 500;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let mut source = beam_alloc::Heap::new(2 * CELLS as usize + 64).expect("the source heap");
    let container = int_list(&mut source, CELLS).expect("the source graph");
    let available = table.get(pid).expect("the spawned process").heap().available();
    let graph_words = beam_term::size_object(container);
    assert!(
        graph_words > available,
        "the source graph must not fit the destination: {graph_words} words against {available} free"
    );
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    // No exemption at all: a scheduled slice's ordinary, published-root state.
    assert!(!table
        .get(pid)
        .expect("the spawned process")
        .is_relocation_exempt());
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        ctx.move_result_into_process_heap(container)
    };
    let copy = copy.expect("a published-root caller may replace the space, so the retry fits");
    assert!(
        beam_term::prelude::is_list(copy) && !beam_term::literal::is_literal(copy),
        "the copy lives on the process's own heap"
    );
    assert!(
        table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count()
            > spaces_before,
        "the same graph that was refused inside the window grows the nursery outside it"
    );
    let (_attempts, grows, _empties) = notes::snapshot();
    assert!(grows >= 1, "the post-refusal growth site ran");
}
