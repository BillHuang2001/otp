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

//! **A growth request is a request for *room*, sized from the space's occupancy plus the
//! request** — and one growth satisfies it.
//!
//! `grow_nursery(need)` is the no-relocation growth seam, and `need` is a room request, not a
//! capacity: the space a growth installs must leave `need` words **free**, so it is sized from
//! the occupancy it inherits plus the request —
//!
//! ```text
//! want     = used + need                                    (the C's `need_after`)
//! one_step = policy.next_words(want, 0)                     (the curated table, floored by …)
//! target   = max(one_step_or_doubling, policy.next_words_above(capacity))
//! ```
//!
//! — which is exactly the C's sizing of a minor's next young block,
//! `grow_new_heap(p, next_heap_size(p, need_after, 0), …)` with
//! `need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + stack_size + S_RESERVED`
//! (`erts/emulator/beam/erl_gc.c:1503-1509`, `:1553-1555`). Sizing from `need` **alone** would
//! install a space with only `capacity - used` words of new room: the request would be
//! "granted" and the very next allocation in the same window would have to grow again, retiring
//! another space and manufacturing collection debt for no progress.
//!
//! # The contract (asserted below)
//!
//! 1. **One growth satisfies a request against a partly filled space.** Afterwards
//!    `young().available_words() >= need`, the chain grew by exactly one space, and a second
//!    identical request is a **no-op** (same base, same chain length, same capacity).
//! 2. **The installed capacity is the one the rule derives**:
//!    `policy.next_words(used + need, 0)` floored by `policy.next_words_above(old_capacity)`.
//! 3. **The occupancy term is really in the figure.** A request of the same `need` against an
//!    **empty** space installs `policy.next_words(need, 0)`, which is strictly smaller than the
//!    capacity the partly filled space needs — so a seam that ignored the occupancy would be
//!    caught here, not in production.
//! 4. **A `max_heap_words` ceiling spends what the budget affords and refuses only when it can
//!    do nothing.** A budget that reaches `used + need` grants the request in **one** growth; a
//!    budget that cannot reach it still installs the largest space it affords — a *partial*
//!    grant, strictly larger than the space it retires, with `capacity_words() <= max_heap_words`
//!    — because a sound, strictly larger space beats no space at all (the caller's own
//!    refusal → grow → retry loop finishes the copy; a `beam_rt` copy-out asks for headroom and
//!    then for the bare shortfall). Only a budget that cannot enlarge the space at all is the
//!    typed refusal.
//!
//! Every case is driven through **both** mature strategies (the copying collector and the
//! mark-region promotion): the nursery is the same barrier-free copying space in both, so the
//! rule must be strategy-independent.

use beam_gc::prelude::{AnyCollector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind};
use beam_term::prelude::{make_small, ptr_val, Eterm};

/// Both mature strategies, so every pin below is strategy-independent.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// The summed-capacity ceiling the ceiling cases install on a policy whose nursery is the crate
/// default's 233 words: `233` installed + **367** of budget.
const CEILING: usize = 600;
/// A collector for `policy` with `kind` **pinned** (an explicit override of the automatic rule).
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// The policy every case uses: a default 233-word nursery/seed, so the figures below are the
/// crate's own defaults and not a hand-picked geometry.
fn policy() -> HeapPolicy {
    HeapPolicy::default()
}

/// Allocates `count` 3-word 2-tuples in `space`, returning the roots so the terms stay live.
fn fill_with_tuples(space: &mut HeapSpace, count: usize) -> Vec<Eterm> {
    let mut roots = Vec::with_capacity(count);
    for i in 0..count {
        let tuple = space
            .heap_mut()
            .tuple(&[make_small(i as i64), make_small(1)])
            .expect("the tuple fits the space");
        roots.push(tuple);
    }
    roots
}

/// The capacity the growth rule derives for a `used`-word occupancy and a `need`-word request.
fn derived_capacity(policy: &HeapPolicy, used: usize, need: usize, old_capacity: usize) -> usize {
    // `used.saturating_add(need)` is the C's `need_after`; `next_words` applies the curated table
    // (and any `min_heap_words` floor / `max_heap_words` clamp), and the strict-progress step
    // guarantees the fresh space is never the size of the one it retires.
    policy
        .next_words(used.saturating_add(need), 0)
        .max(policy.next_words_above(old_capacity))
}

/// (1) + (2): one growth answers a 4 096-word request against a partly filled 233-word nursery,
/// and installs exactly the capacity `next_words(used + need)` derives; the same request a second
/// time is a no-op.
#[test]
fn a_request_against_a_partly_filled_space_is_one_growth_and_real_room() {
    /// Far beyond the 233-word seed space, so the growth branch (not the fast path) runs.
    const NEED: usize = 4_096;
    /// 60 three-word tuples = 180 of the seed space's 233 words.
    const TUPLES: usize = 60;

    let policy = policy();
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());

        let roots = fill_with_tuples(c.young_mut(), TUPLES);
        let used = c.young().used_words();
        assert_eq!(used, 3 * TUPLES, "{kind:?}: the fixture's occupancy");
        let old_capacity = c.young().capacity_words();
        assert_eq!(
            old_capacity,
            policy.initial_heap_words(),
            "{kind:?}: the seed nursery is the policy's initial heap"
        );
        assert!(
            c.young().available_words() < NEED,
            "{kind:?}: the space cannot serve the request as it stands"
        );

        c.grow_nursery(NEED).expect("the growth is granted");

        // (1) Real room, and exactly one new space on the chain.
        assert!(
            c.young().available_words() >= NEED,
            "{kind:?}: the request is for room, so {} words must be free ({} are)",
            NEED,
            c.young().available_words()
        );
        assert_eq!(
            c.young_space_count(),
            2,
            "{kind:?}: one growth satisfies the request (the filled space is retained)"
        );
        assert_eq!(
            c.young().used_words(),
            0,
            "{kind:?}: nothing is moved — the fresh space starts empty"
        );
        assert_eq!(
            c.heap_size_words(),
            old_capacity + c.young().capacity_words(),
            "{kind:?}: the chain sum is the retained space plus the fresh one"
        );

        // (2) The installed capacity is the rule's, occupancy term included.
        let expected = derived_capacity(&policy, used, NEED, old_capacity);
        assert_eq!(
            c.young().capacity_words(),
            expected,
            "{kind:?}: the fresh space is next_words(used + need) = next_words({used} + {NEED}) \
             floored by next_words_above({old_capacity})"
        );
        assert_eq!(
            c.young().capacity_words(),
            policy.next_words(used + NEED, 0),
            "{kind:?}: and on this fixture the schedule term dominates the strict-progress floor"
        );
        // The retained space still holds the mutator's words at their original addresses.
        for root in &roots {
            assert!(
                c.young_contains(ptr_val(*root) as *const Eterm),
                "{kind:?}: a growth never moves a term"
            );
        }

        // A second identical request is a no-op: no space retired, no capacity installed.
        let (base, capacity, spaces) = (
            c.young().base(),
            c.young().capacity_words(),
            c.young_space_count(),
        );
        c.grow_nursery(NEED).expect("the request already fits");
        assert_eq!(c.young().base(), base, "{kind:?}: no base change");
        assert_eq!(
            c.young().capacity_words(),
            capacity,
            "{kind:?}: no capacity change"
        );
        assert_eq!(
            c.young_space_count(),
            spaces,
            "{kind:?}: no space retired — the request is already met"
        );
    }
}

/// (3): the occupancy term is really part of the figure — the same request against an **empty**
/// space installs `next_words(need)`, strictly smaller than what the partly filled space needs.
#[test]
fn a_request_against_an_empty_space_sizes_from_the_request_alone() {
    const NEED: usize = 4_096;
    const TUPLES: usize = 60;

    let policy = policy();
    for kind in KINDS {
        // Empty: nothing was ever allocated in the seed space.
        let mut empty = any_for(kind, policy.clone());
        let old_capacity = empty.young().capacity_words();
        empty.grow_nursery(NEED).expect("the growth is granted");
        let empty_capacity = empty.young().capacity_words();
        assert_eq!(
            empty_capacity,
            derived_capacity(&policy, 0, NEED, old_capacity),
            "{kind:?}: with no occupancy the rule reduces to next_words(need)"
        );
        assert_eq!(
            empty_capacity,
            policy.next_words(NEED, 0),
            "{kind:?}: literally next_words(need)"
        );
        assert!(
            empty.young().available_words() >= NEED,
            "{kind:?}: and the free room is the request"
        );

        // Partly filled: the same request needs the occupancy on top.
        let mut filled = any_for(kind, policy.clone());
        let _roots = fill_with_tuples(filled.young_mut(), TUPLES);
        let used = filled.young().used_words();
        assert_eq!(used, 3 * TUPLES, "{kind:?}");
        filled.grow_nursery(NEED).expect("the growth is granted");

        assert!(
            filled.young().capacity_words() > empty_capacity,
            "{kind:?}: the occupancy term must enlarge the figure ({} vs {empty_capacity}) — a \
             seam sizing from `need` alone would install the same space twice",
            filled.young().capacity_words()
        );
        assert_eq!(
            filled.young().capacity_words(),
            policy.next_words(used + NEED, 0),
            "{kind:?}: the figure is next_words(used + need)"
        );
    }
}

// ---------------------------------------------------------------------------
// (4) Under a `max_heap_words` ceiling: spend what the budget affords, and refuse only when the
//     budget can no longer make the space any larger
// ---------------------------------------------------------------------------

/// A policy with the default 233-word nursery capped at [`CEILING`] words of **summed** chain
/// capacity — the `+hmax`/`spawn_opt` `{max_heap_size,…}` shape `beam_rt`'s copy-out meets.
fn capped() -> HeapPolicy {
    HeapPolicy::from_min_max(0, CEILING)
}

/// The budget a one-space chain of the default nursery leaves: `CEILING - 233`.
fn budget() -> usize {
    CEILING - HeapPolicy::default().initial_heap_words()
}

/// 33 three-word tuples = 99 words of the 233-word seed space, so the space a growth retires is
/// **not** provably empty (a re-adoption could otherwise serve the request for free).
const OCCUPANCY_TUPLES: usize = 33;

/// A cap that cannot reach the request still grows by **what it affords**: the fresh space is the
/// whole remaining budget, strictly larger than the one it retires and inside the ceiling —
/// a partial grant, never a refusal.
///
/// The fixture is the `beam_rt` regression shape: `grow_nursery(need)` is asked for more room
/// than `max_heap_words` has left, and the answer is the largest space the budget can still buy.
/// A refusal here turns a copy-out that only needed one growth into a hard failure (the copier's
/// own refusal → grow → retry loop is what finishes the copy), which is exactly what this arm
/// exists to prevent.
#[test]
fn a_ceiling_that_cannot_reach_the_request_still_grows_by_what_it_affords() {
    /// Occupancy (99) + request (400) is **inside** the growth table's ceiling for this policy, so
    /// the refusal cannot come from there — only from the chain budget (367).
    const NEED: usize = 400;

    let policy = capped();
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        let _roots = fill_with_tuples(c.young_mut(), OCCUPANCY_TUPLES);
        let used = c.young().used_words();
        assert_eq!(used, 99, "{kind:?}: the fixture's occupancy");
        let before = c.young().capacity_words();
        assert_eq!(before, 233, "{kind:?}: the default seed nursery");
        assert!(
            c.young().available_words() < NEED,
            "{kind:?}: the space cannot serve the request as it stands"
        );
        assert!(
            used + NEED > budget(),
            "{kind:?}: the fixture must ask for more than the {}-word budget leaves",
            budget()
        );

        c.grow_nursery(NEED)
            .expect("a ceiling that can still enlarge the space grants a partial growth");

        // Strict progress, inside the ceiling, and exactly the budget: the partial grant.
        assert!(
            c.young().capacity_words() > before,
            "{kind:?}: the partial growth is a real growth ({} vs {before})",
            c.young().capacity_words()
        );
        assert_eq!(
            c.young().capacity_words(),
            budget(),
            "{kind:?}: the fresh space is the whole remaining budget"
        );
        assert!(
            c.young().capacity_words() <= policy.max_heap_words,
            "{kind:?}: the fresh space alone is inside the ceiling"
        );
        assert_eq!(
            c.heap_size_words(),
            CEILING,
            "{kind:?}: the chain's summed capacity lands exactly on the ceiling"
        );
        assert!(
            c.heap_size_words() <= policy.max_heap_words,
            "{kind:?}: and never exceeds it"
        );
        assert_eq!(
            c.young().available_words(),
            budget(),
            "{kind:?}: the room bought is the room the budget affords"
        );
        assert!(
            c.young().available_words() < NEED,
            "{kind:?}: the request itself stays unsatisfied — the caller grows or collects again"
        );
        assert_eq!(
            c.young_space_count(),
            2,
            "{kind:?}: a partial growth is still exactly one new space"
        );
        assert!(
            c.collection_debt(),
            "{kind:?}: the retired space is the caller's cue to collect"
        );
        assert_eq!(c.young().used_words(), 0, "{kind:?}: nothing is moved");
    }
}

/// A request the budget **can** reach is granted in **one** growth — the room contract holds
/// unchanged under a ceiling.
#[test]
fn a_ceiling_that_reaches_the_request_grants_it_in_one_growth() {
    /// Occupancy (99) + request (267) = 366 ≤ the 367-word budget: reachable.
    const NEED: usize = 267;

    let policy = capped();
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        let _roots = fill_with_tuples(c.young_mut(), OCCUPANCY_TUPLES);
        let used = c.young().used_words();
        let before = c.young().capacity_words();
        assert!(
            used + NEED <= budget(),
            "{kind:?}: the fixture must be reachable by the {}-word budget",
            budget()
        );

        c.grow_nursery(NEED).expect("the growth is granted");

        assert!(
            c.young().available_words() >= NEED,
            "{kind:?}: the request is for room, so {} words must be free ({} are)",
            NEED,
            c.young().available_words()
        );
        assert!(
            c.young().capacity_words() > before,
            "{kind:?}: strict progress ({before} -> {})",
            c.young().capacity_words()
        );
        assert_eq!(
            c.heap_size_words(),
            CEILING,
            "{kind:?}: the clamp is the budget, so the chain lands on the ceiling"
        );
        assert_eq!(
            c.young_space_count(),
            2,
            "{kind:?}: one growth, one new space"
        );

        // The same request a second time is a no-op: the grant really is room.
        let (base, capacity, spaces) = (
            c.young().base(),
            c.young().capacity_words(),
            c.young_space_count(),
        );
        c.grow_nursery(NEED).expect("the request already fits");
        assert_eq!(c.young().base(), base, "{kind:?}: no base change");
        assert_eq!(
            c.young().capacity_words(),
            capacity,
            "{kind:?}: no capacity change"
        );
        assert_eq!(c.young_space_count(), spaces, "{kind:?}: no space retired");
    }
}

/// With an **empty** space the ceiling law reduces to `used + need == need`: the installed size is
/// `min(next_words(need, 0), budget)` — the same reduction contract (3) pins without a ceiling.
#[test]
fn with_an_empty_space_the_ceiling_law_reduces_to_the_request_against_the_budget() {
    /// Beyond the budget but inside the growth table's ceiling for this policy.
    const NEED: usize = 400;

    let policy = capped();
    // The unclamped table figure, so the assertion can name both terms it is the minimum of.
    let table = HeapPolicy::default().next_words(NEED, 0);
    assert!(
        table > budget(),
        "the fixture's table figure ({table}) must exceed the budget ({}) — otherwise the \
         ceiling is not what sizes the space",
        budget()
    );
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        assert_eq!(
            c.young().used_words(),
            0,
            "{kind:?}: nothing was ever allocated in the seed space"
        );
        let before = c.young().capacity_words();
        c.grow_nursery(NEED).expect("the partial growth");

        assert_eq!(
            c.young().capacity_words(),
            table.min(budget()),
            "{kind:?}: an empty space sizes from the request alone, clamped by the budget"
        );
        assert!(
            c.young().capacity_words() > before,
            "{kind:?}: strict progress ({before} -> {})",
            c.young().capacity_words()
        );
        assert_eq!(
            c.young().available_words(),
            budget(),
            "{kind:?}: the whole budget is the room installed"
        );
    }
}

/// A budget that can no longer make the space **any larger** is the typed refusal, decided before
/// anything is allocated or retired: the nursery is byte-for-byte identical (base, capacity, chain
/// length, fill) and the collection debt the caller polls is unchanged.
#[test]
fn a_budget_that_cannot_enlarge_the_space_refuses_and_leaves_the_nursery_identical() {
    const NEED: usize = 400;

    let policy = capped();
    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        // Spend the whole budget on one partial growth, so only an edge of it is left.
        let _roots = fill_with_tuples(c.young_mut(), OCCUPANCY_TUPLES);
        c.grow_nursery(NEED)
            .expect("the budget is spent in one partial growth");
        assert_eq!(
            c.heap_size_words(),
            CEILING,
            "{kind:?}: the fixture is at the ceiling"
        );
        assert!(c.collection_debt(), "{kind:?}: the chain is debt");

        // A request the exhausted budget cannot answer: no room at all is left, so no strict
        // progress is possible and the growth must refuse rather than install anything.
        let request = c.young().available_words().saturating_add(1);
        let (base, capacity, spaces, used, total) = (
            c.young().base(),
            c.young().capacity_words(),
            c.young_space_count(),
            c.heap_used_words(),
            c.heap_size_words(),
        );
        assert_eq!(
            policy.max_heap_words - c.heap_size_words(),
            0,
            "{kind:?}: the fixture leaves no budget"
        );

        let err = c
            .grow_nursery(request)
            .expect_err("a budget that cannot enlarge the space refuses");
        assert_eq!(err.as_atom(), Some("out_of_memory"), "{kind:?}");

        assert_eq!(
            c.young().base(),
            base,
            "{kind:?}: a refusal replaces no space"
        );
        assert_eq!(
            c.young().capacity_words(),
            capacity,
            "{kind:?}: a refusal installs no capacity"
        );
        assert_eq!(
            c.young_space_count(),
            spaces,
            "{kind:?}: a refusal retires nothing"
        );
        assert_eq!(
            c.heap_used_words(),
            used,
            "{kind:?}: a refusal grants no room"
        );
        assert_eq!(
            c.heap_size_words(),
            total,
            "{kind:?}: the chain sum is unchanged"
        );
        assert!(
            c.collection_debt(),
            "{kind:?}: the debt the caller polls is unchanged"
        );

        // And the refusal is repeatable: nothing accumulated across it.
        let again = c
            .grow_nursery(request)
            .expect_err("still nothing to install");
        assert_eq!(again.as_atom(), Some("out_of_memory"), "{kind:?}");
        assert_eq!(
            (
                c.young().base(),
                c.young().capacity_words(),
                c.young_space_count()
            ),
            (base, capacity, spaces),
            "{kind:?}: the second refusal is inert too"
        );
    }
}
