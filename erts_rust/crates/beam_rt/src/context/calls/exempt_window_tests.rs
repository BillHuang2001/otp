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
//! The three cases below are that rule's full boundary: inside the window an oversized copy-out
//! never replaces the space (**the pin**), the window still re-homes what fits, and the same
//! oversized copy-out grows again the moment the window closes — so the gate is window-scoped
//! rather than a permanent refusal.

use beam_process::SignalAtoms;
use beam_process::ProcessId;
use beam_term::make_small;

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
/// space exactly as it found it.
///
/// The graph is provably larger than the destination's free words (asserted, not assumed), so the
/// copy can only succeed by **replacing** the young space — which a window whose root set is
/// incomplete may not do. Every observation is on the process itself and on the copy-out's own
/// structural counters, so a future change cannot satisfy this case by anything but the rule:
///
/// * the answer is `None`, i.e. the result stays on the never-rewound pooled BIF heap (a leak,
///   never a dangling word);
/// * `young_space_count` and the current space's `capacity_words` are unchanged — **no**
///   replacement, which is the assertion the pre-rule code fails;
/// * `collection_debt` stays `false` — nothing was retired, so nothing owes a collection;
/// * neither `notes` growth counter moved, so neither growth site ran: not the learned-hint gate
///   (which would otherwise grow *before* the first attempt, because the hint is set) and not the
///   post-refusal retry;
/// * the copy **is still attempted** (`attempts == 1`), so the rule declines the *replacement*
///   and not the retention.
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
    // the destination, so only a replacement could make the copy succeed.
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
    assert!(
        copy.is_none(),
        "an exempt window cannot re-home a result it would have to grow the space for: the \
         answer must be `None`, so the result stays on the pooled heap and the pool is not rewound"
    );
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
    let (attempts, grows, _empties) = notes::snapshot();
    assert_eq!(
        grows, 0,
        "neither growth site may run inside the window: the hint gate is gated on the same rule \
         and the post-refusal retry returns before it"
    );
    assert_eq!(
        notes::hint_grows(),
        0,
        "the learned-hint gate is a replacement too, so it is skipped even with a hint set"
    );
    assert_eq!(
        attempts, 1,
        "the copy is still ATTEMPTED inside the window — the rule declines the replacement, not \
         the retention"
    );
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
