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
//! store, and the two cases in which the pool must stay exactly where the call left it.

use std::rc::Rc;

use beam_bifs::BifContext;
use beam_common::Result;
use beam_engine::ExecutionContext;
use beam_term::make_small;
use beam_term::prelude::make_header;
use beam_term::Eterm;

use super::*;
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// A runtime whose pooled BIF-heap region **and** long-lived store are deliberately tiny
/// ([`crate::config::RuntimeConfig::with_heap_words`]), so a single ordinary result crosses a
/// region boundary and the pool's rewind/growth behaviour is observable on a small workload.
fn small_region_runtime(words: usize) -> Runtime {
    Runtime::new(crate::config::RuntimeConfig::default().with_heap_words(words))
        .expect("a runtime with a small BIF-heap region")
}

/// The frontier region's `used` offset — the pool's allocation position after a call returns.
fn frontier_used(rt: &Runtime) -> usize {
    rt.bif_heaps.last().expect("the pool keeps a region").used()
}

/// The address range a pooled region currently covers: a [`beam_alloc::Heap`] hands out words
/// from its base upward, and `hp() == base + used`, so the base is recoverable from the region's
/// own offset.
fn region_bounds(heap: &beam_alloc::Heap) -> (usize, usize) {
    let hp = heap.hp() as usize;
    let base = hp - heap.used() * core::mem::size_of::<Eterm>();
    (base, heap.hend() as usize)
}

/// Whether `term` — or anything it points at, one cons cell deep — lands inside a pooled region.
///
/// The premise of the rewind is that the *whole* result left the pool, so the test checks the
/// term's own word and its first two words (a cons cell's `CAR`/`CDR` are the words right after
/// the tagged pointer).
fn lies_in_the_pool(rt: &Runtime, term: Eterm) -> bool {
    let mut words = vec![term.raw()];
    if beam_term::prelude::is_list(term) || beam_term::prelude::is_boxed(term) {
        let ptr = beam_term::prelude::ptr_val(term) as usize;
        // The cons cell / boxed object begins at the tagged pointer: its header (a boxed word) or
        // its `CAR` is the first word there.
        for offset in 0..2 {
            // SAFETY: the term is a live list/boxed pointer the pool or the store owns, so the
            // words at it are readable. Nothing is written.
            words.push(unsafe { *(ptr as *const usize).add(offset) });
        }
    }
    rt.bif_heaps.iter().any(|heap| {
        let (base, end) = region_bounds(heap);
        words
            .iter()
            .any(|word| (*word >= base && *word < end) || (ptr_in(*word, base, end)))
    })
}

/// Whether `word` is a tagged pointer that lands in `[base, end)`.
fn ptr_in(word: usize, base: usize, end: usize) -> bool {
    // Every heap pointer is word-aligned and the low two bits carry the primary tag, so masking
    // them off recovers the address; an immediate's masked value is nowhere near the region.
    let address = word & !0b11;
    address >= base && address < end
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
    let mut rt = small_region_runtime(1024);
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
    assert!(
        !lies_in_the_pool(&rt, result),
        "the returned value must not point into the pool the caller just rewound"
    );
    assert_eq!(
        rt.format_term_w(result, None).expect("the copy renders"),
        "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29|"
            .split('|')
            .next()
            .expect("prefix")
            .to_string()
            + "30,31,32,33,34,35,36,37,38,39,40,41,42,43,44,45,46,47,48,49,50,51,52,53,54,55,"
            + "56,57,58,59,60,61,62,63,64,65,66,67,68,69,70,71,72,73,74,75,76,77,78,79,80,81,"
            + "82,83,84,85,86,87,88,89,90,91,92,93,94,95,96,97,98,99,100]"
    );
}

/// **A result kept in the evaluator's environment survives many later calls, each of which rewinds
/// the pool past it.**
///
/// This is the test the rewind has to pass: the kept list is built by the pool, so a rewind that
/// recycled a *live* result would overwrite its words with the next calls' results. The region is
/// deliberately tiny (`DEFAULT_...` = a few hundred words here), so the 300-element list is far
/// larger than one region and dozens of regions are retired around it.
#[test]
fn a_kept_result_survives_later_calls_that_rewind_the_pool() {
    let mut rt = small_region_runtime(256);
    let kept = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(300)])
            .expect("lists:seq(1, 300)")
    };
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for round in 0..40 {
            let tuple = ctx
                .call_bif(
                    "erlang",
                    "list_to_tuple",
                    1,
                    &[beam_engine::terms::make_list(&mut ctx, &[make_small(round)]).expect("a list")],
                )
                .expect("list_to_tuple/1");
            assert!(
                !lies_in_the_pool(ctx.runtime, tuple),
                "round {round}: the result is a copy, not a pool word"
            );
        }
    }
    assert!(
        rt.bif_heaps.len() > 1,
        "the workload really did cross region boundaries ({} regions)",
        rt.bif_heaps.len()
    );
    let rendered = rt.format_term_w(kept, None).expect("the kept list renders");
    assert!(
        rendered.starts_with("[1,2,3,") && rendered.ends_with(",298,299,300]"),
        "the kept list is intact after 40 rewinds: {rendered}"
    );
}

/// A store region too small for a result is **replaced** (never moved), and everything the
/// earlier region holds stays valid — [`Runtime::grow_eval_heap`]'s documented contract.
#[test]
fn the_store_grows_by_region_replacement_and_earlier_terms_stay_valid() {
    let mut rt = small_region_runtime(256);
    let first = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(300)])
            .expect("lists:seq(1, 300)")
    };
    assert_eq!(
        rt.eval_heap_retired.len(),
        1,
        "a 300-element list does not fit a 256-word store, so exactly one region was replaced"
    );
    assert!(
        rt.eval_heap.capacity() >= 600,
        "the fresh region is at least the copier's own shortfall ({} words)",
        rt.eval_heap.capacity()
    );
    let second = {
        let mut ctx = RuntimeContext::new(&mut rt);
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(400)])
            .expect("lists:seq(1, 400)")
    };
    assert_eq!(
        rt.eval_heap_retired.len(),
        1,
        "the second result fits the grown region: no second replacement"
    );
    for (term, last) in [(first, "300"), (second, "400")] {
        let rendered = rt.format_term_w(term, None).expect("a stored term renders");
        assert!(
            rendered.starts_with("[1,2,3,") && rendered.ends_with(&format!(",{last}]")),
            "a term in the retired region is still intact: {rendered}"
        );
    }
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
    let mut rt = small_region_runtime(1024);
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
