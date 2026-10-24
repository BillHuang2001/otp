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
//!
//! Every case is driven through **both** mature strategies (the copying collector and the
//! mark-region promotion): the nursery is the same barrier-free copying space in both, so the
//! rule must be strategy-independent.

use beam_gc::prelude::{
    AnyCollector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::prelude::{make_small, ptr_val, Eterm};

/// Both mature strategies, so every pin below is strategy-independent.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

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
