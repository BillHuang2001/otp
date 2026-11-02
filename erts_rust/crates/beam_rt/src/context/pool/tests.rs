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

//! Units for the **table-less** route's BIF-result store (see [`super`]): the hand-through by
//! reference that replaced the copy onto [`Runtime::eval_heap`] and the pool rewind it licensed,
//! the region-replacement growth of the store when a copy *is* still needed, and the cases in which
//! the pool must stay exactly where the call left it.
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

/// **A container result is handed through by reference: it stays on the pool, and nothing is copied.**
///
/// On the table-less route there is no process heap to move a result onto, so the result the BIF
/// built is handed to the caller **where it stands** on the pooled region — the pool's frontier keeps
/// the words, and the long-lived store receives none of them. This is the hand-through the fix
/// introduces: the deep copy and the rewind it licensed are gone, so the caller reads the BIF's own
/// words instead of a structural twin. The invariant under test is that the handed-through word is
/// still the whole, correct container.
#[test]
fn a_container_result_is_handed_through_by_reference_and_stays_on_the_pool() {
    let mut rt = small_region_runtime();
    assert_eq!(frontier_used(&rt), 0, "the pool starts clean");
    let result = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(100)])
            .expect("lists:seq(1, 100)")
    };
    assert_eq!(
        frontier_used(&rt),
        200,
        "the 100 cons cells (200 words) the call handed out stay on the pool, handed through by reference"
    );
    assert_eq!(
        rt.eval_heap.used(),
        0,
        "no copy ran: the long-lived store received nothing"
    );
    assert_eq!(
        rt.eval_heap_retired.len(),
        0,
        "a hand-through replaces no store region"
    );
    assert!(
        lies_in_the_pool(&rt, result),
        "the returned value is the pool's own word, not a copy off it"
    );
    // `format_term_w` is `~w~n`, so the rendering carries the format's trailing newline.
    let rendered = rt.format_term_w(result, None).expect("the handed-through word renders");
    let rendered = rendered.trim_end();
    assert!(
        rendered.starts_with("[1,2,3,4,5,") && rendered.ends_with(",97,98,99,100]"),
        "the handed-through word is the whole list, not a recycled one: {rendered}"
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

/// **A result kept in the evaluator's environment survives many later calls, each of which allocates
/// above it.**
///
/// This is the test the hand-through has to pass: the kept list was built *on the pool* and handed to
/// the caller by reference, so the next call's mark sits **above** its words. A pool that rewound
/// (re-bumped) below a live result would overwrite its words with the next calls' results. It also
/// pins the fix's whole point on this route: 500 container calls copy nothing onto the store, and the
/// frontier only ever advances — the kept list's 600 words stay exactly where they were built.
#[test]
fn a_handed_through_result_survives_later_calls_that_allocate_above_it() {
    let mut rt = small_region_runtime();
    let kept = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(300)])
            .expect("lists:seq(1, 300)")
    };
    assert_eq!(
        rt.eval_heap.used(),
        0,
        "the kept 300-element list is handed through: no store words"
    );
    assert_eq!(
        frontier_used(&rt),
        600,
        "the kept list's own 600 words stay on the pool"
    );
    assert!(
        lies_in_the_pool(&rt, kept),
        "the kept list is the pool's own word"
    );
    let before = rt.format_term_w(kept, None).expect("the kept list renders");
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for round in 0..500 {
            let args = beam_engine::terms::make_list(&mut ctx, &[make_small(round)]).expect("a list");
            let tuple = ctx
                .call_bif("erlang", "list_to_tuple", 1, &[args])
                .expect("list_to_tuple/1");
            assert!(
                lies_in_the_pool(ctx.runtime, tuple),
                "round {round}: the result is handed through, not a copy"
            );
            assert_eq!(
                terms::tuple_arity(tuple).expect("the BIF built a tuple"),
                1,
                "round {round}: a one-element tuple"
            );
        }
    }
    // The kept list must still read byte-identically: its 600 words were never overwritten, even
    // after the later calls' results marched the frontier (and possibly retired the first region).
    let after = rt.format_term_w(kept, None).expect("the kept list still renders");
    assert_eq!(
        before, after,
        "the kept list is byte-identical after 500 later handed-through calls"
    );
    let trimmed = before.trim_end();
    assert!(
        trimmed.starts_with("[1,2,3,") && trimmed.ends_with(",298,299,300]"),
        "the kept list is intact: {trimmed}"
    );
}

/// **A store region too small for a result is never replaced now: the hand-through fires instead.**
///
/// Both results here are legal BIF calls on a [`REGION_WORDS`] pool; the second one is larger than
/// the store's remaining headroom, which is exactly the shape that used to force a store region
/// replacement. The hand-through removes the copy, so the store grows **nothing** and retires
/// **nothing** — while the pool retires its own region to hold both results, and both keep their
/// words valid (a retired pool region is kept alive, never moved or re-bumped below its offset).
/// The first result — whose words are in the now-retired pool region — still renders.
#[test]
fn a_handed_through_result_needs_no_store_region_and_earlier_pool_terms_stay_valid() {
    let mut rt = small_region_runtime();
    assert_eq!(
        rt.eval_heap.capacity(),
        REGION_WORDS,
        "the store starts at the configured size"
    );
    assert_eq!(rt.eval_heap_retired.len(), 0);

    // 900 elements is 1800 words: it fits the 2048-word pool, and would previously have been copied
    // onto the store. Now it is handed through — the store receives nothing.
    let first = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(900)])
            .expect("lists:seq(1, 900)")
    };
    assert_eq!(rt.eval_heap.used(), 0, "the hand-through copies nothing onto the store");
    assert_eq!(rt.eval_heap_retired.len(), 0, "a hand-through retires no store region");
    assert!(lies_in_the_pool(&rt, first), "the 900-element list is handed through on the pool");

    // 700 elements is 1400 more words: the pool's own region cannot hold both, so the pool retires
    // its first region and continues on a wider one. The store is still untouched.
    let second = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(700)])
            .expect("lists:seq(1, 700)")
    };
    assert_eq!(rt.eval_heap.used(), 0, "still no store words after the second hand-through");
    assert_eq!(rt.eval_heap_retired.len(), 0, "still no store region retired");
    assert!(lies_in_the_pool(&rt, second), "the 700-element list is handed through on the pool");

    for (term, last) in [(first, "900"), (second, "700")] {
        let rendered = rt.format_term_w(term, None).expect("a handed-through term renders");
        let rendered = rendered.trim_end();
        assert!(
            rendered.starts_with("[1,2,3,") && rendered.ends_with(&format!(",{last}]")),
            "a handed-through term stays intact across later pool activity: {rendered}"
        );
    }
}

/// **A table-less container result is handed through by reference, and the pool keeps each result.**
///
/// The `-eval`/single-pass route has no scheduled process, so
/// [`RuntimeContext::call_registered`]'s copy-out takes the **store** arm — and now, for a result
/// whose whole graph is pointer-stable on the pool, hands it through as `Rehomed::KeepPool` rather
/// than deep-copying it. Three consequences, all pinned here per round:
///
/// * the caller gets a **correct, readable** container back — the load-bearing half, since a
///   hand-through that lost a word shows up here;
/// * the pooled region is **not** rewound: the frontier advances by the tuple's own words each
///   round, and the store receives **nothing**;
/// * every surviving result word stays readable at its original address — the first round's tuple is
///   read back at the end, out of a region the pool may since have retired (a retired pool region is
///   kept alive, never moved or re-bumped below its offset).
#[test]
fn a_table_less_container_result_is_handed_through_and_the_pool_keeps_each_result() {
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
            assert!(
                lies_in_the_pool(ctx.runtime, tuple),
                "round {round}: handed through, not copied"
            );
            assert_eq!(
                ctx.runtime.eval_heap.used(),
                0,
                "round {round}: the store receives nothing"
            );
        }
    }
    // The store never grew: no copy ran, so no store region was replaced.
    assert_eq!(rt.eval_heap.used(), 0, "the store holds no result word");
    assert_eq!(rt.eval_heap_retired.len(), 0, "no store region was retired");
    // The pool accumulated each result in place: ROUNDS × TUPLE_WORDS words, never rewound.
    assert_eq!(
        pool_words_touched(&rt),
        ROUNDS * TUPLE_WORDS,
        "the pool's high-water is every result's own words, never rewound"
    );
    // The first round's tuple — possibly in a region the pool retired — still reads.
    assert_eq!(
        (
            terms::tuple_arity(kept).expect("the first round's container is intact"),
            terms::tuple_element(kept, ELEMENTS - 1).expect("in range"),
        ),
        (ELEMENTS, make_small(0)),
        "a result whose words live in a retired pool region still reads"
    );
    assert!(lies_in_the_pool(&rt, kept), "the kept result is still the pool's own word");
}

/// **A table-less container workload keeps each result in the pool, so a `+hmax`-capped region is
/// eventually exhausted.**
///
/// This is the honest memory half of the hand-through: because the result is kept by reference and
/// the pool is **not** rewound, a run of fresh-container calls accumulates one result's words per
/// call in the pool. Without a cap the pool grows by region replacement to hold them all (see
/// [`a_table_less_container_result_is_handed_through_and_the_pool_keeps_each_result`]); with a
/// `+hmax` cap the pool cannot grow, so the accumulation ends in `error:system_limit` — a configured
/// limit, never a wrong result. Every result that *did* complete is still whole and readable.
///
/// The exhaustion point is arithmetic, not luck: each call hands out [`TUPLE_WORDS`] words and never
/// gives them back, so exactly `REGION_WORDS / TUPLE_WORDS` calls fit the capped region before the
/// next one overflows it.
#[test]
fn a_table_less_container_workload_keeps_each_result_and_exhausts_a_capped_pool() {
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
    let mut successes = 0usize;
    let mut exhaustion: Option<beam_common::BeamError> = None;
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for round in 0..ROUNDS {
            match ctx.call_bif(
                "erlang",
                "make_tuple",
                2,
                &[make_small(ELEMENTS as i64), make_small(0)],
            ) {
                Ok(tuple) => {
                    assert_eq!(
                        terms::tuple_arity(tuple).expect("the BIF built a tuple"),
                        ELEMENTS,
                        "round {round}: a complete container, not a truncated one"
                    );
                    assert!(
                        lies_in_the_pool(ctx.runtime, tuple),
                        "round {round}: handed through, kept in the pool"
                    );
                    assert_eq!(
                        ctx.runtime.eval_heap.used(),
                        0,
                        "round {round}: the store receives nothing"
                    );
                    successes += 1;
                }
                Err(error) => {
                    exhaustion = Some(error);
                    break;
                }
            }
        }
    }
    // The pool accumulated one result per successful call and never gave a word back.
    assert!(
        pool_words_touched(&rt) >= successes * TUPLE_WORDS,
        "the pool's high-water is at least every successful result's own words — the accumulation \
         the hand-through introduces"
    );
    assert_eq!(rt.eval_heap.used(), 0, "the store held nothing the whole time");
    // The exhaustion is the arithmetic the test is built on, and it is a configured-limit refusal,
    // not a corruption.
    let expected_successes = REGION_WORDS / TUPLE_WORDS;
    assert_eq!(
        successes, expected_successes,
        "exactly floor({REGION_WORDS} / {TUPLE_WORDS}) fresh containers fit the capped pool before \
         the next overflows it"
    );
    let error = exhaustion.expect("the capped pool really did exhaust");
    assert_eq!(
        error.as_atom(),
        Some("system_limit"),
        "the overflow is the emulator's system_limit, never a wrong result"
    );
}

/// A registered BIF that allocates words on the pooled heap and returns a **container the copier
/// refuses**: a real three-word tuple whose second element is `make_header(0, 0)`, the C ABI's
/// non-value (a bare header word, not a term).
///
/// The **root** deliberately stands on the pool: that is what keeps the copy-out's "nothing to
/// move" short-circuit out of the way ([`super::super::calls::dest::pool_holds`] answers `true`),
/// so the copier really runs — and refuses — and the refusal is what leaves the pool un-rewound.
fn refused_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.heap().tuple(&[make_small(1), make_header(0, 0)])
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
    let (used, still_on_the_pool) = {
        let mut ctx = RuntimeContext::new(&mut rt);
        let result = ctx
            .call_bif("pool_fixture", "refused", 0, &[])
            .expect("the fixture BIF returns");
        let heap = ctx.runtime.bif_heaps.last().expect("a region");
        let held = crate::context::calls::dest::pool_holds(ctx.runtime, heap, result);
        (heap.used(), held)
    };
    assert!(
        still_on_the_pool,
        "the un-re-homed result is handed to the caller still standing on the pool"
    );
    assert!(
        still_on_the_pool && used == 3,
        "the result is the container the BIF built, still inside the call's own three words"
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
/// one — taken when that region can no longer serve the reservation
/// [`frontier::need`](super::super::calls::frontier::need) asks for — puts it back and
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
/// header plus two elements) against a [`REGION_WORDS`] region whose reservation is a quarter of
/// itself (`512` words), so the 514th call — the first to start with fewer than `512` free words
/// (`2048 - 1539 = 509`) — retires the region and continues on a 4096-word frontier. The figures
/// are **derived in the test** from [`frontier::need`](super::super::calls::frontier::need) and
/// the fixture's own three words per call, never typed in.
/// Nothing is relocated: the retired region goes back to the pool **as it is** — neither rewound
/// nor freed, and outside the reach of the store move (`src/runtime/reclaim/compact.rs` evacuates
/// only `eval_heap`/`eval_heap_retired`) — so the words its callers still own stay valid and at
/// their own address, and only the allocation rotation moves to the fresh, wider frontier.
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
    // THE PIN IS DERIVED, NEVER RE-TYPED: the taker hands the popped region back only when it can
    // still serve `frontier::need` free words — the entitlement (a quarter of the region, capped in
    // absolute words) raised to whatever a call at this depth has already demanded. Every refused
    // call below hands out exactly `WORDS_PER_CALL` words and learns exactly that, so the learned
    // figure stays below the entitlement and the boundary is arithmetic:
    //
    //   * a call is served while `used <= capacity - need`;
    //   * the region's `used` at the start of call `k` is `(k - 1) * WORDS_PER_CALL`,
    //     so `served = ceil((capacity - need) / WORDS_PER_CALL) + 1` calls fit (the one that starts
    //     at `used == 0` included);
    //   * the next call starts below the reservation, retires the region and runs on a fresh one.
    let need = crate::context::calls::frontier::need(&rt, REGION_WORDS);
    const WORDS_PER_CALL: usize = 3;
    let served = (REGION_WORDS - need) / WORDS_PER_CALL + 1;
    assert_eq!(
        calls,
        served + 1,
        "the retirement lands on the first call that starts with fewer than {need} free words \
         (the reservoir's own reservation), not before it"
    );
    assert!(
        REGION_WORDS - WORDS_PER_CALL * (served - 1) >= need
            && REGION_WORDS - WORDS_PER_CALL * served < need,
        "the boundary is exact: the last served call started with {} free words (>= {need}), the \
         retiring one with {} (< {need})",
        REGION_WORDS - WORDS_PER_CALL * (served - 1),
        REGION_WORDS - WORDS_PER_CALL * served
    );
    assert_eq!(
        pool_shape(&rt),
        vec![
            (REGION_WORDS, WORDS_PER_CALL * served),
            (REGION_WORDS * 2, WORDS_PER_CALL)
        ],
        "the exhausted region is RETIRED with every word its callers still own ({served} calls × \
         {WORDS_PER_CALL} words) and the next call runs on a fresh, twice-as-wide frontier"
    );
    // The copy-out moved nothing here — which is what the loop's own exit proves: a successful
    // copy-out would have licensed the rewind and the region could never fill. Nothing is asserted
    // about `Runtime::eval_heap` itself: on the table-less route the returned container is also
    // **stabilized** by an independent door (`Runtime::stabilize_term`'s store), so a store figure
    // cannot tell a copy-out from that stabilization.

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
        vec![
            (REGION_WORDS, WORDS_PER_CALL * served),
            (REGION_WORDS * 2, WORDS_PER_CALL * 2)
        ],
        "the frontier serves the next call; the retired region keeps its words untouched"
    );
}

/// **A non-rewindable table-less call is NOT moved: the store stays out of it.**
///
/// On the table-less route the copy exists for exactly one purpose — to license the pool
/// rewind — so the move is gated by `safe_to_rewind` itself: a call whose hooked seam could have
/// retained a heap word skips the copy **and** the rewind. The case drives the `dict` arm
/// (`erlang:get_keys/1` builds its key list on `ctx.heap()`) with **no** scheduled process, and
/// pins both halves of that decision over 20 rounds:
///
/// * the store receives **nothing** — [`Runtime::eval_heap`]'s offset stays `0` — because a copy
///   into a region that nothing resets accumulates one result per call until a region-reclaim site
///   runs (`src/runtime/reclaim.rs`), and no such site runs inside a single `-eval` statement or a
///   bare `call_bif`: a long run of such calls exhausts the term-heap arena
///   (`error:system_limit`) for a reason that has nothing to do with the workload's live set;
/// * the caller is handed the pool's own word ([`lies_in_the_pool`]), with the correct value, and
///   the frontier stays **above** the call's mark — the words the seam could have retained are
///   never handed out again.
#[test]
fn a_non_rewindable_table_less_call_is_not_moved_and_the_store_stays_empty() {
    const ROUNDS: usize = 20;
    let mut rt = small_region_runtime();
    // One entry in the runtime's **own** process's dictionary — the process `RuntimeContext::new`
    // reports as `self` — so `get_keys/1` really builds a one-element list on the call's heap.
    let pid = rt.self_pid;
    rt.processes
        .get_mut(pid)
        .expect("the runtime always owns its own process")
        .dict_mut()
        .put(make_small(7), make_small(11));
    for round in 0..ROUNDS {
        let keys = {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.call_bif("erlang", "get_keys", 1, &[make_small(11)])
                .unwrap_or_else(|e| panic!("round {round}: get_keys(11): {e}"))
        };
        assert_eq!(
            (
                terms::cons_head(keys).expect("a cons cell"),
                terms::cons_tail(keys).expect("a cons cell"),
            ),
            (make_small(7), NIL),
            "round {round}: the key list is exactly `[7]`"
        );
        assert!(
            lies_in_the_pool(&rt, keys),
            "round {round}: the `dict` arm is not rewindable, so the move is skipped and the \
             caller keeps the pool's own word"
        );
        assert_eq!(
            rt.eval_heap.used(),
            0,
            "round {round}: the store must receive nothing — a copy here is never reset and only \
             accumulates"
        );
        assert!(
            frontier_used(&rt) > 0,
            "round {round}: the call's words stay on the pool, so the frontier is above its mark"
        );
    }
}

/// **The one host-memory mechanism the table-less route now leaves open is the engine arena.**
///
/// A long in-process `compile:file/2` (`-eval`, i.e. no scheduled process) used to retain host
/// memory in exactly two places — the engine arena and the `-eval` store's copy-out volume. The
/// hand-through closes the second: a container result the pool owns is handed through by reference,
/// so the store receives **nothing** and retires **no** region, however many container results a
/// single statement builds.
///
/// This case pins the one remaining rule with a fixture whose demand is arithmetic rather than luck
/// — no wall-clock, no compile, no allocation policy involved:
///
/// * arena: 1 word reserves the first region, `unit − 1` words fill it, and the next word pushes a
///   fresh **doubled** region. `used_words` is then exactly the words requested, `reserved_words`
///   is the geometric ladder (`unit · (2^regions − 1)`), and the mark taken in the first region no
///   longer matches — the arena has no state it can be rewound to;
/// * store: [`COPIES`] fresh container results are handed through by reference, so the store's
///   retention counters stay `0` — the copy-out volume that used to be the second term of the bound
///   is gone, and the store route still adds **not one** arena word.
#[test]
fn the_open_host_memory_bound_is_the_arena_demand_with_no_store_copy_out() {
    /// 900 cons cells: a legal `lists:seq/2` result on a [`REGION_WORDS`] pool.
    const ELEMENTS: usize = 900;
    /// Enough handed-through results to prove the store never accumulates.
    const COPIES: usize = 4;

    let mut rt = small_region_runtime();
    assert_eq!(
        REGION_WORDS, 2_048,
        "this case's arena arithmetic assumes the suite's region"
    );

    let (unit, demand, arena) = {
        let mut ctx = RuntimeContext::new(&mut rt);
        // ---- (a) the arena: a known demand, region after region ----
        assert_eq!(
            ctx.runtime.arena.heap_stats().regions,
            0,
            "a fresh runtime has reserved nothing on the engine arena: the region heap grows \
             lazily, so every word it holds was requested by a run"
        );
        assert_eq!(
            ctx.runtime.arena.heap_mark(),
            beam_engine::HeapMark::EMPTY,
            "no region, so no mark"
        );

        // One word reserves the first region — the geometric unit the whole chain is built from.
        let _ = ctx.alloc_words(1).expect("one arena word");
        let first = ctx.runtime.arena.heap_stats();
        assert_eq!(
            (first.regions, first.used_words, first.frontier_used),
            (1, 1, 1),
            "the first request reserves the first region"
        );
        let unit = first.reserved_words;
        let first_mark = ctx.runtime.arena.heap_mark();

        // The rest of the first reservation, exactly: one region, filled to its last word.
        let _ = ctx
            .alloc_words(unit - 1)
            .expect("the rest of the first reservation");
        let filled = ctx.runtime.arena.heap_stats();
        assert_eq!(
            (filled.regions, filled.used_words, filled.frontier_available),
            (1, unit, 0),
            "the first reservation is exactly `unit` words and is now exhausted"
        );

        // One word past it cannot come from that reservation: the arena reserves a fresh,
        // doubled region and keeps the old one alive and unmoved.
        let _ = ctx.alloc_words(1).expect("a word past the first reservation");
        let demand = unit + 1;
        let grown = ctx.runtime.arena.heap_stats();
        assert_eq!(grown.regions, 2, "the exhausted reservation is replaced");
        assert_eq!(
            grown.used_words, demand,
            "`used_words` is the arena's CUMULATIVE demand: exactly the words requested"
        );
        assert_eq!(
            grown.reserved_words,
            unit * ((1 << grown.regions) - 1),
            "the reservations obey the documented geometric rule: each fresh region doubles the \
             previous one, so their sum is `unit · (2^regions − 1)`"
        );
        assert!(
            grown.committed_words < grown.reserved_words,
            "a reservation is committed lazily, so the arena's address space is looser than its \
             committed words ({} < {})",
            grown.committed_words,
            grown.reserved_words
        );
        assert!(
            !ctx.runtime.arena.heap_mark_matches(first_mark),
            "the first region is no longer the frontier and never becomes it again — the arena has \
             no state a rewind could restore, which is why its retention is its total demand"
        );
        assert_eq!(
            ctx.runtime.retention_stats().arena_words,
            grown.used_words as u64,
            "the retention counter and the census are one quantity, not two estimates"
        );

        // ---- (b) the store: the hand-through writes nothing, so there is no copy-out volume ----
        assert_eq!(
            ctx.runtime.retention_stats().eval_heap_retired_regions,
            0,
            "no copy has happened yet"
        );
        for round in 0..COPIES {
            let handed = ctx
                .call_bif(
                    "lists",
                    "seq",
                    2,
                    &[make_small(1), make_small(ELEMENTS as i64)],
                )
                .unwrap_or_else(|error| panic!("round {round}: lists:seq(1, {ELEMENTS}): {error}"));
            assert_eq!(
                terms::cons_head(handed).expect("a cons cell"),
                make_small(1),
                "round {round}: every result is a complete list"
            );
            assert!(
                lies_in_the_pool(ctx.runtime, handed),
                "round {round}: handed through, not copied"
            );
        }
        let stats = ctx.runtime.retention_stats();
        assert_eq!(
            stats.eval_heap_retired_regions,
            0,
            "the store retires no region for a hand-through"
        );
        assert_eq!(
            stats.eval_heap_retired_words,
            0,
            "no store region was ever retired"
        );
        assert_eq!(
            (stats.eval_heap_words, ctx.runtime.eval_heap.used()),
            (REGION_WORDS as u64, 0),
            "the store keeps its one configured, empty region"
        );

        // The store route adds not one arena word, and its copy-out volume is now zero: the peak is
        // the arena demand alone.
        assert_eq!(
            ctx.runtime.arena.heap_stats(),
            grown,
            "the store route adds not one arena word"
        );
        (unit, demand, grown)
    };

    let stats = rt.retention_stats();
    eprintln!(
        "bound pin: arena unit={unit} demand={demand} regions={} reserved={} used={} | store \
         retired_regions={} retained={}",
        arena.regions,
        arena.reserved_words,
        arena.used_words,
        stats.eval_heap_retired_regions,
        stats.eval_heap_words + stats.eval_heap_retired_words,
    );
}
