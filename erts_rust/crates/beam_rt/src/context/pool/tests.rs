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

/// Whether `term`'s own pointer lands inside a pooled region — i.e. whether the caller was handed
/// a value the pool owns rather than a copy of it.
///
/// The check is on the **term's own address only** (no dereference): an immediate carries no
/// address, a literal lives in the frozen literal area, and both are outside every region, so the
/// question this test asks — "did the result leave the pool?" — is answered for the term itself.
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
    let rendered = rt.format_term_w(result, None).expect("the copy renders");
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
            let args = beam_engine::terms::make_list(&mut ctx, &[make_small(round)]).expect("a list");
            let tuple = ctx
                .call_bif("erlang", "list_to_tuple", 1, &[args])
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
