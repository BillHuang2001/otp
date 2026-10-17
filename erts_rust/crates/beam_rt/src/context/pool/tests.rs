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

//! Units for the **table-less** route's BIF-result store (see [`super`]): the copy onto
//! [`Runtime::eval_heap`], the pool rewind it licenses, the region-replacement growth of the
//! store, and the cases in which the pool must stay exactly where the call left it.
//!
//! # Why nothing here asks for a tiny region
//!
//! [`crate::config::RuntimeConfig::with_heap_words`] sizes *both* the pooled BIF heap and the
//! store, and the runtime **floors** it at its own minimum (1024 words), so no test can conjure a
//! 256-word region — and a region crossing is not something a test should hope for anyway. Every
//! test below therefore either (a) holds a workload that is a sizeable fraction of the region it
//! configures — the suite's [`REGION_WORDS`], or the smallest legal one where a store crossing has
//! to be forced — so a crossing is arithmetic rather than luck, or (b) drives
//! [`Runtime::grow_eval_heap`] itself, whose contract is what the copy's growth step relies on.
//! The premise that used to be asserted here — "a 256-word runtime gets a 256-word region" — was
//! simply false.

use std::rc::Rc;

use beam_bifs::BifContext;
use beam_common::Result;
use beam_engine::terms;
use beam_engine::ExecutionContext;
use beam_term::prelude::make_header;
use beam_term::Eterm;
use beam_term::{make_small, NIL};

use crate::config::RuntimeConfig;
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// The term-heap region this suite asks for, in words.
///
/// `RuntimeConfig::with_heap_words` floors its argument at the runtime's own minimum (1024 words),
/// so this is a *request* the runtime honours exactly — and it sizes **both** heaps the suite
/// reasons about: the pooled BIF heap ([`Runtime::bif_heaps`]) and the long-lived store
/// ([`Runtime::eval_heap`]). A 1024-word pool would refuse a 700-element list (1400 words), which
/// is the workload that makes a store region cross; 2048 holds it.
const REGION_WORDS: usize = 2_048;

/// A runtime whose term-heap regions are [`REGION_WORDS`] — small enough that a few hundred
/// elements is a sizeable fraction of a region, large enough that the workloads below are legal
/// BIF calls rather than `error:system_limit`.
fn small_region_runtime() -> Runtime {
    Runtime::new(crate::config::RuntimeConfig::default().with_heap_words(REGION_WORDS))
        .expect("a runtime with a small (but legal) term-heap region")
}

/// The frontier region's `used` offset — the pool's allocation position after a call returns.
fn frontier_used(rt: &Runtime) -> usize {
    rt.bif_heaps.last().expect("the pool keeps a region").used()
}

/// Every region the pool holds, as `(capacity, used)`, in pool order.
///
/// A retired region is pushed back **before** the fresh frontier is built, so the last entry is
/// always the region the next call will allocate on.
fn pool_shape(rt: &Runtime) -> Vec<(usize, usize)> {
    rt.bif_heaps
        .iter()
        .map(|heap| (heap.capacity(), heap.used()))
        .collect()
}

/// The address range a pooled region currently covers: a [`beam_alloc::Heap`] hands out words
/// from its base upward, and `hp() == base + used`, so the base is recoverable from the region's
/// own offset.
fn region_bounds(heap: &beam_alloc::Heap) -> (usize, usize) {
    let hp = heap.hp() as usize;
    let base = hp - heap.used() * core::mem::size_of::<Eterm>();
    (base, heap.hend() as usize)
}

/// The **cumulative high-water** of the pool: the words every region has ever handed out.
///
/// `peak_used` survives a [`beam_alloc::Heap::reset_to`] (it is a property of the live-word
/// history, not of the current offset), so this is exactly "how many pool words have been
/// first-touched" — the quantity that marched with the call count before the rewind existed.
fn pool_words_touched(rt: &Runtime) -> usize {
    rt.bif_heaps.iter().map(|heap| heap.peak_used()).sum()
}

/// Whether `term`'s own pointer lands inside a pooled region — i.e. whether the caller was handed
/// a value the pool owns rather than a copy of it.
///
/// The check is on the **term's own address only** (no dereference): an immediate carries no
/// address, a literal lives in the frozen literal area, and both are outside every region, so the
/// question this suite asks — "did the result leave the pool?" — is answered for the term itself.
/// Anything the term points at is reached through the copy the store made, and its integrity is
/// what the rendering assertions below check.
fn lies_in_the_pool(rt: &Runtime, term: Eterm) -> bool {
    if !(beam_term::prelude::is_list(term) || beam_term::prelude::is_boxed(term)) {
        return false;
    }
    let address = beam_term::prelude::ptr_val(term) as usize;
    rt.bif_heaps.iter().any(|heap| {
        let (base, end) = region_bounds(heap);
        // Every heap pointer is word-aligned and the low two bits carry the primary tag, so the
        // masked address is the object's own address.
        (address & !0b11) >= base && (address & !0b11) < end
    })
}

/// **A container result is copied off the pool and the pool is rewound to the call's mark.**
///
/// On the table-less route there is no process heap to move a result onto, so before the store
/// existed nothing rewound the pool: `lists:seq/2`'s 200 words (and every later container) stayed
/// on the frontier region for the run's whole life. Now the result is copied onto
/// [`Runtime::eval_heap`], the caller is handed the *copy*, and the frontier returns to its mark —
/// the words the call handed out are given back, while the value stays readable.
#[test]
fn a_container_result_is_copied_off_the_pool_and_the_pool_is_rewound() {
    let mut rt = small_region_runtime();
    assert_eq!(frontier_used(&rt), 0, "the pool starts clean");
    let result = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(100)])
            .expect("lists:seq(1, 100)")
    };
    assert_eq!(
        frontier_used(&rt),
        0,
        "every word the call handed out of the pool is given back"
    );
    assert_eq!(
        rt.eval_heap.used(),
        200,
        "the copy — and only the copy — is on the long-lived store (100 cons cells)"
    );
    assert_eq!(
        rt.eval_heap_retired.len(),
        0,
        "a copy that fits the store replaces no region"
    );
    assert!(
        !lies_in_the_pool(&rt, result),
        "the returned value must not point into the pool the caller just rewound"
    );
    // `format_term_w` is `~w~n`, so the rendering carries the format's trailing newline.
    let rendered = rt.format_term_w(result, None).expect("the copy renders");
    let rendered = rendered.trim_end();
    assert!(
        rendered.starts_with("[1,2,3,4,5,") && rendered.ends_with(",97,98,99,100]"),
        "the copy is the whole list, not a recycled word: {rendered}"
    );
    assert_eq!(
        rendered.matches(',').count(),
        99,
        "100 elements means 99 separators"
    );
}

/// **An immediate result rewinds the pool with no copy at all.**
///
/// [`RuntimeContext::call_registered`](super::super::calls) skips the copy for anything that is
/// not a foreign container: an immediate is neither boxed nor a cons, so nothing it could point at
/// lives on this heap and the rewind needs no destination. The fixture BIF allocates a temporary
/// tuple (three pool words) and returns the small integer `7`, so the assertion is exact: the
/// temporary is given back, and the store never grows.
#[test]
fn an_immediate_result_rewinds_the_pool_without_a_copy() {
    /// Allocates on the pooled heap, then returns an **immediate** — the shape whose result
    /// cannot reach a pool word.
    fn immediate_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
        let _pooled = ctx.heap().tuple(&[make_small(2), make_small(3)])?;
        Ok(make_small(7))
    }

    let mut rt = small_region_runtime();
    {
        let registry = Rc::get_mut(&mut rt.registry).expect("the runtime holds the only handle");
        let _ = registry.register("pool_fixture", "immediate", 0, immediate_result);
    }
    let result = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("pool_fixture", "immediate", 0, &[])
            .expect("the fixture BIF returns")
    };
    assert_eq!(result.raw(), make_small(7).raw(), "the result is unchanged");
    assert_eq!(
        frontier_used(&rt),
        0,
        "the temporary the call built on the pool is given back"
    );
    assert_eq!(
        rt.eval_heap.used(),
        0,
        "an immediate is never copied onto the store"
    );
}

/// **A result kept in the evaluator's environment survives many later calls, each of which rewinds
/// the pool past it.**
///
/// This is the test the rewind has to pass: the kept list was built *on the pool*, so a rewind
/// that recycled a live result would overwrite its words with the next calls' results. It also
/// pins the fix's whole point on this route: 500 container calls leave the pool at **one** region
/// whose first-touch high-water is exactly the kept list's own 600 words — the calls added
/// nothing, where before the store existed the frontier marched 2 words per call past them (and
/// would have crossed into a second region at ~700 calls).
#[test]
fn a_kept_result_survives_later_calls_that_rewind_the_pool() {
    let mut rt = small_region_runtime();
    let kept = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(300)])
            .expect("lists:seq(1, 300)")
    };
    assert_eq!(
        rt.eval_heap.used(),
        600,
        "the kept 300-element list is 600 store words"
    );
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for round in 0..500 {
            let args = beam_engine::terms::make_list(&mut ctx, &[make_small(round)]).expect("a list");
            let tuple = ctx
                .call_bif("erlang", "list_to_tuple", 1, &[args])
                .expect("list_to_tuple/1");
            assert!(
                !lies_in_the_pool(ctx.runtime, tuple),
                "round {round}: the result is a copy, not a pool word"
            );
            assert_eq!(
                ctx.runtime
                    .bif_heaps
                    .last()
                    .expect("the pool keeps a region")
                    .used(),
                0,
                "round {round}: the frontier returned to the call's mark"
            );
        }
    }
    assert_eq!(
        rt.bif_heaps.len(),
        1,
        "the pool never needed a second region: every call gave its own words back"
    );
    assert_eq!(
        pool_words_touched(&rt),
        600,
        "the pool's first-touch high-water is the kept list's own 600 words — the 500 container \
         calls that followed added not one word to it"
    );
    let rendered = rt.format_term_w(kept, None).expect("the kept list renders");
    let rendered = rendered.trim_end();
    assert!(
        rendered.starts_with("[1,2,3,") && rendered.ends_with(",298,299,300]"),
        "the kept list is intact after 500 rewinds: {rendered}"
    );
}

/// **A store region too small for a result is *replaced* (never moved), and everything the earlier
/// region holds stays valid** — [`Runtime::grow_eval_heap`]'s documented contract, reached the way
/// the copy reaches it.
///
/// Both results here are legal BIF calls on a [`REGION_WORDS`] pool; the second one is larger than
/// what the store has left, so the copier refuses it once, its own reported shortfall grows the
/// store by region replacement, and the retry lands in the fresh region. The first result — whose
/// words are in the now-retired region — still renders.
#[test]
fn the_store_grows_by_region_replacement_and_earlier_terms_stay_valid() {
    let mut rt = small_region_runtime();
    assert_eq!(
        rt.eval_heap.capacity(),
        REGION_WORDS,
        "the store starts at the configured size"
    );
    assert_eq!(rt.eval_heap_retired.len(), 0);

    // 900 elements is 1800 store words: it fits, so no region is replaced — and it leaves only
    // 248 words of headroom.
    let first = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(900)])
            .expect("lists:seq(1, 900)")
    };
    assert_eq!(rt.eval_heap.used(), 1_800, "the copy fits the starting region");
    assert_eq!(
        rt.eval_heap_retired.len(),
        0,
        "a copy that fits retires nothing"
    );

    // 700 elements is 1400 store words: more than the headroom, but still a legal BIF call on the
    // 2048-word pool. That is exactly the trade the growth step exists for — the copy is refused
    // once, the copier's own reported shortfall replaces the store region, the retry lands.
    let second = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(700)])
            .expect("lists:seq(1, 700)")
    };
    assert_eq!(
        rt.eval_heap_retired.len(),
        1,
        "the oversized result replaced exactly one region"
    );
    assert!(
        rt.eval_heap.capacity() >= 1_400,
        "the fresh region is at least the copier's own shortfall ({} words)",
        rt.eval_heap.capacity()
    );
    assert_eq!(
        rt.eval_heap.used(),
        1_400,
        "the retry landed in the fresh region"
    );
    assert_eq!(
        rt.eval_heap_retired
            .last()
            .expect("a retired region")
            .capacity(),
        REGION_WORDS,
        "the region that was replaced is the small one, kept alive rather than freed"
    );

    // A permit to grow is not a command to: a request the fresh region already satisfies retires
    // nothing and replaces nothing.
    let (capacity, retired) = (rt.eval_heap.capacity(), rt.eval_heap_retired.len());
    rt.grow_eval_heap(1_024).expect("a legal growth request");
    assert_eq!(
        (rt.eval_heap.capacity(), rt.eval_heap_retired.len()),
        (capacity, retired),
        "grow_eval_heap is a no-op while the region has room"
    );

    for (term, last) in [(first, "900"), (second, "700")] {
        let rendered = rt.format_term_w(term, None).expect("a stored term renders");
        let rendered = rendered.trim_end();
        assert!(
            rendered.starts_with("[1,2,3,") && rendered.ends_with(&format!(",{last}]")),
            "a term in a retired region is still intact: {rendered}"
        );
    }
}

/// The shape the `-eval`/single-pass **store** must have after `calls` container copies of
/// `words` words each, given a starting region of `region` words.
///
/// `(frontier_used, capacity, retired)`: what [`Runtime::eval_heap`]'s `used()` and `capacity()`
/// must read (a replacement starts a fresh region, so the offset restarts at `0`), and the widths
/// of the replaced regions in order. The model is [`Runtime::grow_eval_heap`]'s documented policy
/// and nothing more: a copy that fits the remaining room lands as it is, one that does not replaces
/// the region with one **twice as wide** (`needed.max(EVAL_STORE_GROWTH_FLOOR)` only matters below
/// 256 words, which `MIN_HEAP_WORDS` rules out) and lands at offset `0` of the fresh one — which is
/// kept, never freed, so its words stay readable.
fn expected_store_shape(calls: usize, words: usize, region: usize) -> (usize, usize, Vec<usize>) {
    let mut frontier = 0;
    let mut capacity = region;
    let mut retired = Vec::new();
    for _ in 0..calls {
        if frontier + words > capacity {
            retired.push(capacity);
            capacity *= 2;
            frontier = 0;
        }
        frontier += words;
    }
    (frontier, capacity, retired)
}

/// **A table-less container result is re-homed to the store, and the pooled BIF region is rewound
/// to the call's mark.**
///
/// The `-eval`/single-pass route has no scheduled process, so
/// [`RuntimeContext::call_registered`]'s copy-out takes the **store** arm
/// ([`RuntimeContext::move_result_into_runtime_store`]) instead of the process-heap one: the
/// result is deep-copied onto the runtime's long-lived `-eval` store ([`Runtime::eval_heap`], its
/// off-heap payloads adopted into [`Runtime::eval_off_heap`]) and only *then* is the pool rewound.
/// Three consequences, all pinned here per round:
///
/// * the caller gets a **correct, readable** container back — the load-bearing half, since a copy
///   that lost a word or a rewind that recycled one shows up here;
/// * the pooled region is **not** retained: one region, `used() == 0`, capacity unchanged. The
///   accumulation the pool's growth path used to exist for — every result staying on the region it
///   was built on for the run's whole life — no longer happens on this route;
/// * the surviving result words are owned by the **store**, exactly: each call adds the tuple's own
///   101 words (a header plus 100 elements) and the store's region replacements follow
///   [`expected_store_shape`] — here one, at the eleventh copy.
///
/// The first round's tuple is read back at the end, out of the region that replacement *retired*
/// (a retired store region is kept alive, never moved), so a store that merely forgot its earlier
/// words fails too.
#[test]
fn a_table_less_container_result_is_stored_and_the_pool_rewound_to_the_call_mark() {
    // The smallest region a runtime accepts (`MIN_HEAP_WORDS`), which sizes **both** the pooled BIF
    // heap and the store; 100 elements is a 101-word tuple, built from immediates alone.
    const REGION_WORDS: usize = 1024;
    const ELEMENTS: usize = 100;
    const TUPLE_WORDS: usize = ELEMENTS + 1;
    const ROUNDS: usize = 20;

    let mut rt = Runtime::new(RuntimeConfig::new().with_heap_words(REGION_WORDS))
        .expect("a runtime with the smallest legal term-construction region");
    assert_eq!(rt.bif_heap_words, REGION_WORDS, "the configured region");
    let mut kept = NIL;
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for round in 0..ROUNDS {
            let tuple = ctx
                .call_bif(
                    "erlang",
                    "make_tuple",
                    2,
                    &[make_small(ELEMENTS as i64), make_small(0)],
                )
                .unwrap_or_else(|e| panic!("round {round}: make_tuple({ELEMENTS}, 0): {e}"));
            assert_eq!(
                (
                    terms::tuple_arity(tuple).expect("the BIF built a tuple"),
                    terms::tuple_element(tuple, ELEMENTS - 1).expect("in range"),
                ),
                (ELEMENTS, make_small(0)),
                "round {round}: every call returns its whole container"
            );
            if round == 0 {
                kept = tuple;
            }
            // The pool gave every word this call handed out back, in one region, at its mark.
            let pool = ctx
                .runtime
                .bif_heaps
                .last()
                .expect("the pool keeps a region");
            assert_eq!(
                (ctx.runtime.bif_heaps.len(), pool.used(), pool.capacity()),
                (1, 0, REGION_WORDS),
                "round {round}: one region, at the call's mark, never grown"
            );
            // The store owns one result per completed call, in its own growth policy's shape.
            let (frontier, capacity, retired) =
                expected_store_shape(round + 1, TUPLE_WORDS, REGION_WORDS);
            assert_eq!(
                (
                    ctx.runtime.eval_heap.used(),
                    ctx.runtime.eval_heap.capacity(),
                    ctx.runtime.eval_heap_retired.len(),
                ),
                (frontier, capacity, retired.len()),
                "round {round}: the store owns every surviving result word"
            );
        }
    }

    let (_, capacity, retired) = expected_store_shape(ROUNDS, TUPLE_WORDS, REGION_WORDS);
    assert_eq!(
        rt.eval_heap.capacity(),
        capacity,
        "{ROUNDS} calls × {TUPLE_WORDS} words, ending on a {capacity}-word region"
    );
    assert_eq!(
        rt.eval_heap_retired
            .iter()
            .map(|heap| heap.capacity())
            .collect::<Vec<_>>(),
        retired,
        "the region the first ten copies landed in is retired, not freed: {retired:?}"
    );
    assert_eq!(
        (
            terms::tuple_arity(kept).expect("the first round's container is intact"),
            terms::tuple_element(kept, ELEMENTS - 1).expect("in range"),
        ),
        (ELEMENTS, make_small(0)),
        "a result whose words live in the retired region still reads"
    );
}

/// **Capacity independence: a table-less container workload that would exhaust a fixed pooled
/// region completes.**
///
/// The pool's own growth path ([`RuntimeContext::take_bif_frontier`]) is **pre-call** and
/// `+hmax`-capped: it retires a region *already* nearly exhausted when a call starts — exactly the
/// accumulation the store made unnecessary — and it does not rescue a call whose own result
/// exceeds the region (measured: `make_tuple(2000, 0)` on a 1024-word region is
/// `error:system_limit` with and without a `+hmax`). Pinning `+hmax` to the region's own size
/// therefore disables the branch outright (`grown = min(2 × capacity, cap) <= capacity`) and makes
/// this case's arithmetic a proof: [`ROUNDS`] × [`TUPLE_WORDS`] is 10 100 words against 1024, nearly
/// ten times its capacity, so a frontier that kept each result where it was built would raise
/// `error:system_limit` at the eleventh call. Every call succeeding is the rewind — and only the
/// rewind — at work. `tests/eval_pool_result.rs` drives the same discriminator end to end through
/// the CLI (a 1000-container `-eval` expression needed `+hmax 16384` before the store and completes
/// at `+hmax 4096` with it); this is the cheap in-node form.
#[test]
fn a_table_less_container_workload_that_would_exhaust_a_fixed_pool_now_completes() {
    const REGION_WORDS: usize = 1024;
    const ELEMENTS: usize = 100;
    const TUPLE_WORDS: usize = ELEMENTS + 1;
    const ROUNDS: usize = 100;

    let config = RuntimeConfig::new()
        .with_heap_words(REGION_WORDS)
        .with_heap_options(crate::config::HeapOptions::new().with_max_heap_words(REGION_WORDS));
    let mut rt = Runtime::new(config).expect("a runtime whose +hmax is pinned to its region");
    assert_eq!(
        rt.bif_heap_words, REGION_WORDS,
        "`+hmax` both sizes and caps the pooled region"
    );
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for round in 0..ROUNDS {
            let tuple = ctx
                .call_bif(
                    "erlang",
                    "make_tuple",
                    2,
                    &[make_small(ELEMENTS as i64), make_small(0)],
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "round {round}: the accumulated frontier would have exhausted the capped \
                         region here, the rewind is what keeps it legal: {e}"
                    )
                });
            assert_eq!(
                terms::tuple_arity(tuple).expect("the BIF built a tuple"),
                ELEMENTS,
                "round {round}: a complete container, not a truncated one"
            );
            let pool = ctx
                .runtime
                .bif_heaps
                .last()
                .expect("the pool keeps a region");
            assert_eq!(
                (pool.used(), pool.capacity()),
                (0, REGION_WORDS),
                "round {round}: one capped region, rewound to the call's mark"
            );
        }
        assert_eq!(
            ctx.runtime.bif_heaps.len(),
            1,
            "the capped pool never needed — nor could have got — a second region"
        );
    }

    // The store, not the pool, absorbed the workload, in its own growth policy's shape.
    let (frontier, capacity, retired) = expected_store_shape(ROUNDS, TUPLE_WORDS, REGION_WORDS);
    assert_eq!(
        (rt.eval_heap.used(), rt.eval_heap.capacity()),
        (frontier, capacity),
        "the store absorbed all {ROUNDS} results ({TUPLE_WORDS} words each)"
    );
    assert_eq!(
        rt.eval_heap_retired
            .iter()
            .map(|heap| heap.capacity())
            .collect::<Vec<_>>(),
        retired,
        "its region replacements are what pay for the bounded pool: {retired:?}"
    );
    assert!(
        retired.len() >= 3,
        "the workload really crossed several store regions: {retired:?}"
    );
}

/// A registered BIF that allocates words on the pooled heap and returns a **container the copier
/// refuses** (`make_header(0, 0)` is the C ABI's non-value, a bare header word).
fn refused_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let _pooled = ctx.heap().tuple(&[make_small(1), make_small(2)])?;
    Ok(make_header(0, 0))
}

/// **A refused copy leaves the pool exactly where the call left it** — the table-less half of
/// [`crate::context::calls`]'s same-named guarantee.
///
/// The refused result is returned to the caller **as it was built**, still on the pool, so a
/// rewind would recycle the very words it points at. Growth cannot make a malformed graph
/// copyable, so neither the store nor the pool moves.
#[test]
fn a_refused_copy_leaves_the_pooled_bif_heap_unrewound() {
    let mut rt = small_region_runtime();
    {
        let registry = Rc::get_mut(&mut rt.registry).expect("the runtime holds the only handle");
        let _ = registry.register("pool_fixture", "refused", 0, refused_result);
    }
    let retired_before = rt.eval_heap_retired.len();
    let capacity_before = rt.eval_heap.capacity();
    let (result, used) = {
        let mut ctx = RuntimeContext::new(&mut rt);
        let result = ctx
            .call_bif("pool_fixture", "refused", 0, &[])
            .expect("the fixture BIF returns");
        (result, ctx.runtime.bif_heaps.last().expect("a region").used())
    };
    assert_eq!(
        result.raw(),
        make_header(0, 0).raw(),
        "the un-re-homed result is handed to the caller unchanged"
    );
    assert_eq!(
        used, 3,
        "the refused call's own words are still owned by the result: no rewind"
    );
    assert_eq!(
        (rt.eval_heap_retired.len(), rt.eval_heap.capacity()),
        (retired_before, capacity_before),
        "a permanent refusal grows nothing"
    );
}

/// **An exhausted pooled region is RETIRED and replaced by a wider frontier.**
///
/// [`RuntimeContext::take_bif_frontier`](super::super::calls::RuntimeContext::take_bif_frontier)
/// has two branches: the ordinary one hands back the region the pool popped, and the retirement
/// one — taken when that region has less than `capacity()/16` free words — puts it back and
/// continues in a fresh region **twice as wide**. The rewind leaves the first branch as the only
/// one an ordinary container workload ever sees, so this case drives the second through the one
/// **refused** copy: [`refused_result`] builds a three-word tuple on `ctx.heap()` and answers with
/// the C ABI's non-value, so the copier refuses it, both re-home arms answer `None`, and the call
/// does not rewind — its words accumulate exactly as a NIF result's (never copied out, see
/// `try_call_nif`) or an escaping `DispatchHooks` arm's (no rewind at all, see `safe_to_rewind`)
/// would. Those two are out of reach for a unit case of this size: a NIF result needs a
/// `dlopen`ed library (`tests/nif.rs`), and an escaping arm needs the process rig the
/// `context::tests` dictionary case builds.
///
/// The retirement is arithmetic, not luck: three words per refused call (a `{1,2}` tuple: a
/// header plus two elements) against a [`REGION_WORDS`] region whose free-word floor is
/// `REGION_WORDS / 16 = 128`, so the 642nd call — the first to start with fewer than 128 free
/// words (`2048 - 1923 = 125`) — retires the region and continues on a 4096-word frontier.
/// Nothing is relocated: the retired region goes back to the pool **as it is** — never rewound,
/// never freed — so the words its callers still own stay valid and at their own address, and only
/// the allocation rotation moves to the fresh, wider frontier.
#[test]
fn an_exhausted_pooled_region_is_retired_and_replaced_by_a_wider_frontier() {
    let mut rt = small_region_runtime();
    {
        let registry = Rc::get_mut(&mut rt.registry).expect("the runtime holds the only handle");
        let _ = registry.register("pool_fixture", "refused", 0, refused_result);
    }
    let mut calls = 0usize;
    // One refused call per round: each leaves its own words on the frontier, so the offset marches
    // toward the region's capacity until a call finds less than the threshold free.
    while rt.bif_heaps.len() == 1 {
        {
            let mut ctx = RuntimeContext::new(&mut rt);
            let _ = ctx
                .call_bif("pool_fixture", "refused", 0, &[])
                .expect("the fixture BIF returns");
        }
        calls += 1;
        assert!(
            calls < 10_000,
            "the frontier never filled up, so the retirement branch was not driven: {:?}",
            pool_shape(&rt)
        );
    }
    assert_eq!(
        calls, 642,
        "the retirement lands on the first call that starts below the `capacity()/16` floor"
    );
    assert_eq!(
        pool_shape(&rt),
        vec![(REGION_WORDS, 1_923), (REGION_WORDS * 2, 3)],
        "the exhausted region is RETIRED with every word its callers still own (641 calls × 3 \
         words) and the next call runs on a fresh, twice-as-wide frontier"
    );
    // The floor is `capacity() / 16` free words — 128 here — and the boundary is exact: the 641st
    // call started with exactly 128 free words and was served from the region, the 642nd with 125.
    assert!(
        REGION_WORDS - 1_920 >= REGION_WORDS / 16 && REGION_WORDS - 1_923 < REGION_WORDS / 16,
        "the retirement happened at the free-word floor and not before it"
    );
    assert_eq!(
        rt.eval_heap.used(),
        0,
        "every copy was refused, so the store received nothing: the words really accumulated on \
         the pool"
    );

    // The rotation moved: the next refused call allocates on the fresh frontier, and the retired
    // region is left exactly as it was.
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        let _ = ctx
            .call_bif("pool_fixture", "refused", 0, &[])
            .expect("the fixture BIF returns");
    }
    assert_eq!(
        pool_shape(&rt),
        vec![(REGION_WORDS, 1_923), (REGION_WORDS * 2, 6)],
        "the frontier serves the next call; the retired region keeps its words untouched"
    );
}
