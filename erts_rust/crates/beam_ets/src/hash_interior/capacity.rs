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
//! The chained interior's bucket-array **growth and release policy**.
//!
//! One responsibility: keep [`HashInterior::buckets`] (and each entry's `objs`
//! vector) proportional to what the interior still **stores**, without churning
//! on a stable-size insert/delete workload. Everything here is driven by the
//! interior's own occupancy (`len`) — no `AtomTable` state is owned, and no term
//! order or hashing is re-implemented (`term_hash` / `eq` come from `beam_term`).
//!
//! * grow: double + rebuild once the object load passes `3/4` ([`LOAD_FACTOR_NUM`]);
//! * release: rebuild at a proportional size once the object load falls below
//!   `1/8` ([`SHRINK_NUM`]) — the two thresholds leave a **6× hysteresis band**;
//! * per-entry: an `objs` vector gives its surplus back past a 4× slack
//!   ([`OBJS_SHRINK_FACTOR`]).
//!
//! Every rebuild goes through [`HashInterior::rehash_into`], which allocates the
//! new bucket array at **exactly** the target count, so the stored capacity
//! tracks the live contents instead of the high-water allocation.

use beam_term::prelude::*;

use super::{Entry, HashInterior};

/// The starting bucket count of a fresh table (a power of two, so the modulo
/// index distributes evenly).
pub(super) const INITIAL_BUCKETS: usize = 8;

/// Load-factor numerator: grow past 3/4 of a bucket per stored object.
const LOAD_FACTOR_NUM: usize = 3;
/// Load-factor denominator.
const LOAD_FACTOR_DEN: usize = 4;

/// The **shrink** threshold's numerator: release bucket capacity while the object
/// load stays below 1/8 of the bucket count.
///
/// The grow threshold is 3/4, so the two leave a **6× band** (1/8 … 3/4) in which
/// neither happens. That hysteresis is what keeps a steady insert/delete churn at
/// a stable size from thrashing the bucket array: one delete cannot undo the array
/// a whole series of inserts paid for, and a rebuild is only ever amortized.
const SHRINK_NUM: usize = 1;
/// The **shrink** threshold's denominator (see [`SHRINK_NUM`]).
const SHRINK_DEN: usize = 8;

/// An entry's object vector is released back to its live length only once it holds
/// fewer than a quarter of the objects it can — the same hysteresis idea as
/// [`SHRINK_NUM`], applied to a single key's `bag`/`duplicate_bag` object list.
const OBJS_SHRINK_FACTOR: usize = 4;

/// The bucket array a fresh interior starts with: [`INITIAL_BUCKETS`] empty
/// buckets (never empty afterwards — the modulo index relies on it).
pub(super) fn initial_buckets() -> Vec<Vec<Entry>> {
    (0..INITIAL_BUCKETS).map(|_| Vec::new()).collect()
}

impl HashInterior {
    /// Double the bucket array and reinsert every entry at its new index.
    fn grow(&mut self, atoms: &AtomTable) {
        self.rehash_into(self.buckets.len() * 2, atoms);
    }

    /// Build a fresh bucket array of exactly `target` buckets and move every entry
    /// into it.
    ///
    /// A rebuild — rather than an in-place resize — is what makes the interior's
    /// stored capacity track its **live** contents: the new bucket vector is
    /// allocated at exactly `target`, and the buckets a stale (now empty) bucket
    /// vector used to hold are dropped entirely instead of lingering as empty
    /// minimum-capacity vectors. It serves both directions: [`grow`](Self::grow)
    /// doubles through it and [`maybe_shrink`](Self::maybe_shrink) halves through it.
    fn rehash_into(&mut self, target: usize, atoms: &AtomTable) {
        let mut rebuilt: Vec<Vec<Entry>> = Vec::with_capacity(target);
        rebuilt.resize_with(target, Vec::new);
        for bucket in self.buckets.drain(..) {
            for entry in bucket {
                let idx = (term_hash(entry.key, atoms) as usize) % target;
                rebuilt[idx].push(entry);
            }
        }
        self.buckets = rebuilt;
    }

    /// Grow once the object load factor passes ~3/4. `len` is the object count
    /// (what `size` reports); for the multi kinds it grows a little earlier than
    /// the entry count alone would, which is harmless.
    pub(super) fn maybe_grow(&mut self, atoms: &AtomTable) {
        if self.len * LOAD_FACTOR_DEN > self.buckets.len() * LOAD_FACTOR_NUM {
            self.grow(atoms);
        }
    }

    /// Release bucket-array capacity once the object load has fallen well below it
    /// (`len / buckets < 1/8`), so a table that grew large and then shrank reports —
    /// and holds — storage proportional to what it still **stores**, exactly like
    /// the system `ets` (whose hash tables give their buckets back on deletion).
    ///
    /// Only a bucket array above [`INITIAL_BUCKETS`] is a candidate, and the
    /// hysteresis band documented on [`SHRINK_NUM`] means a stable-size
    /// insert/delete churn never triggers a rebuild: the array only shrinks after
    /// live occupancy has fallen to an eighth of it.
    pub(super) fn maybe_shrink(&mut self, atoms: &AtomTable) {
        let buckets = self.buckets.len();
        if buckets <= INITIAL_BUCKETS {
            return;
        }
        if self.len * SHRINK_DEN >= buckets * SHRINK_NUM {
            return;
        }
        // Halve while the load stays under the threshold, so one long teardown
        // reaches a bucket count proportional to the surviving objects instead of
        // paying a rebuild per deletion.
        let mut target = buckets / 2;
        while target > INITIAL_BUCKETS && self.len * SHRINK_DEN < target * SHRINK_NUM {
            target /= 2;
        }
        self.rehash_into(target, atoms);
    }
}

/// Release an entry's surplus object-vector capacity once it holds fewer than a
/// quarter of the objects it has room for — the per-key counterpart of
/// [`HashInterior::maybe_shrink`], with the same hysteresis intent
/// ([`OBJS_SHRINK_FACTOR`]). Returns whether capacity was released.
pub(super) fn trim_objs(objs: &mut Vec<Eterm>) -> bool {
    if objs.capacity() > OBJS_SHRINK_FACTOR * objs.len() {
        objs.shrink_to_fit();
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_interior::tests::object;
    use crate::kind::TableKind;
    use crate::table::Interior;
    use beam_term::heap::Heap;

    /// The bucket array gives capacity back as objects leave: a table that grew to
    /// thousands of objects and then shrank to one rebuilds its buckets down to the
    /// starting count (never below it), so `ets:info(T, memory)` reports — and the
    /// interior holds — storage proportional to what it still **stores**.
    #[test]
    fn the_bucket_array_shrinks_back_to_the_live_object_count() {
        let atoms = AtomTable::new();
        const N: i64 = 3_000;
        let mut heap = Heap::new(8 * N as usize);
        let mut t = HashInterior::new(TableKind::Set);
        let empty = t.memory(&atoms);
        let objs: Vec<Eterm> = (0..N).map(|i| object(&mut heap, i, i * 2)).collect();
        for i in 0..N as usize {
            t.insert(make_small(i as i64), objs[i], &atoms).unwrap();
        }
        let populated = t.memory(&atoms);
        assert!(
            t.bucket_count() > INITIAL_BUCKETS,
            "the bucket array grew past its starting count ({})",
            t.bucket_count()
        );
        // Remove all but the last key: the load falls far below 1/8, so the array is
        // rebuilt at a proportional size.
        for i in 0..N - 1 {
            t.delete(make_small(i), &atoms).unwrap();
        }
        assert_eq!(t.size(), 1);
        let one_left = t.memory(&atoms);
        assert_eq!(
            t.bucket_count(),
            INITIAL_BUCKETS,
            "the bucket array stops at its starting count, never below it"
        );
        assert!(
            one_left < 10_000,
            "one object left must not keep the {N}-object index ({one_left} words, \
             populated {populated})"
        );
        assert!(
            one_left * 100 < populated,
            "the one-object figure is far below the loaded one ({one_left} vs {populated})"
        );
        assert!(one_left > empty, "the surviving entry still owns storage");
        // The surviving key is still reachable — a rebuild must not lose an entry.
        assert_eq!(t.lookup(make_small(N - 1), &atoms), vec![objs[N as usize - 1]]);
        // Emptying the interior releases the rest: it is back at (or below) its
        // starting footprint, with the starting bucket count.
        t.delete(make_small(N - 1), &atoms).unwrap();
        assert_eq!(t.bucket_count(), INITIAL_BUCKETS);
        // The emptied interior is back at its starting allocation (the bucket array
        // plus whatever the now-empty bucket vectors reserved); the **exact** empty
        // floor is `Table::release_empty_interior`'s fresh rebuild, pinned by
        // `tests/ets_capacity.rs`.
        assert!(
            t.memory(&atoms) <= empty + 32,
            "an emptied interior owns no surplus ({} words, fresh floor {empty})",
            t.memory(&atoms)
        );
    }

    /// A stable-size insert/delete churn must neither grow the interior nor make its
    /// bucket array shrink and re-grow: the grow (3/4 load) and shrink (1/8 load)
    /// thresholds leave a 6× hysteresis band the churn never crosses.
    #[test]
    fn a_stable_size_churn_neither_grows_nor_thrashes_the_bucket_array() {
        let atoms = AtomTable::new();
        const KEYS: i64 = 500;
        const ROUNDS: usize = 200;
        let mut heap = Heap::new(4 * KEYS as usize);
        let mut t = HashInterior::new(TableKind::Set);
        let objs: Vec<Eterm> = (0..KEYS).map(|i| object(&mut heap, i, i)).collect();
        let keys: Vec<Eterm> = (0..KEYS).map(make_small).collect();
        for i in 0..KEYS as usize {
            t.insert(keys[i], objs[i], &atoms).unwrap();
        }
        let base = t.memory(&atoms);
        let base_buckets = t.bucket_count();
        let (mut min, mut max) = (base, base);
        for _ in 0..ROUNDS {
            // Remove and immediately re-insert every other key: the occupancy never
            // leaves its steady state, so neither threshold may fire.
            for (i, key) in keys.iter().enumerate().step_by(2) {
                t.delete(*key, &atoms).unwrap();
                t.insert(*key, objs[i], &atoms).unwrap();
            }
            let now = t.memory(&atoms);
            min = min.min(now);
            max = max.max(now);
            assert_eq!(t.size(), KEYS as usize, "the round ends at a stable size");
        }
        assert!(
            max <= base,
            "the churn grew the interior above its warm-up figure (base {base}, max {max})"
        );
        assert!(
            min * 8 >= base * 7,
            "the churn shrank the interior by more than an eighth (base {base}, min {min})"
        );
        assert_eq!(
            t.bucket_count(),
            base_buckets,
            "the bucket array must not shrink/re-grow during a stable-size churn"
        );
    }

}
