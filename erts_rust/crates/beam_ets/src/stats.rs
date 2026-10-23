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

//! The **interior statistics** behind `ets:info(Tab, stats)`.
//!
//! `ets:info(Tab, stats)` is the one `ets:info/2` item that reports the table
//! *interior's own* shape (`db_get_info`'s `stats` arm,
//! `../../erts/emulator/beam/erl_db.c:5585-5629`):
//!
//! | table | oracle's answer |
//! |---|---|
//! | a hash table (`set`/`bag`/`duplicate_bag`) | `{Nactive, AvgChainLen, StdDevChainLen, StdDevExpected, MinChainLen, MaxChainLen, KeptItems, NLocks}` |
//! | a catree table (a fine-locked `ordered_set`) | `{RouteNodes, BaseNodes, MaxDepth}` |
//! | anything else (a plain `ordered_set`) | the atom `false` |
//!
//! This crate reproduces the **shape** and, where the quantity means the same
//! thing, the **value**; the chain/occupancy *metrics* are this crate's own
//! interiors' measurements (a CHAMP tree, a chained bucket array, a K-ary B+-tree
//! — not ERTS's bucket-chained hash table and catree), so they legitimately differ
//! from the oracle's figures. See [`TableStats`] for the per-element contract and
//! the crate `CONTEXT.md` for the full positional table.
//!
//! The *arithmetic* on those metrics is reproduced exactly: ERTS computes the three
//! floats in `float` (`f32`) fields (`erl_db_hash.h:121-129`), and
//! [`BucketChains::hash_stats`] rounds each result through `f32` the same way, so a
//! given occupancy histogram yields the oracle's own digits.
//!
//! Everything here is heap-free: [`TableStats`] is a plain snapshot of `usize`/
//! `f64` values, and rendering it as terms is the BIF layer's job
//! (`beam_bifs::ets`).

/// The hash family's bucket/chain occupancy figures — the oracle's eight-element
/// `stats` tuple for a `set`/`bag`/`duplicate_bag`.
///
/// The three floats are computed from the interior's own bucket occupancy with
/// the oracle's formulas ([`BucketChains::hash_stats`]): the average chain length,
/// the real standard deviation of the chain lengths, and the standard deviation a
/// *uniform* hash would give (the binomial `sqrt(avg * (1 - 1/buckets))`).
/// [`avg_chain_len`](Self::avg_chain_len) and the two deviations therefore describe
/// **this** interior's spread, which is the quantity ERTS's own tuple reports for
/// its bucket array — the numbers differ, the meaning is the same.
///
/// The three fields are `f64` because that is the Rust representation of a term
/// float, but each **value** is a `float` in the C: `DbHashStats` types all three
/// as `float` (`../../erts/emulator/beam/erl_db_hash.h:121-129`) and
/// `db_calc_stats_hash` stores each result back into that `float` field before
/// `db_get_info` widens it to a double for the tuple
/// (`../../erts/emulator/beam/erl_db.c:5594-5607`). [`BucketChains::hash_stats`]
/// reproduces that rounding exactly, so the arithmetic — not just the formulas —
/// agrees with the oracle.
///
/// [`kept_items`](Self::kept_items) and [`nlocks`](Self::nlocks) are the two
/// elements that **agree with the oracle by value** (see their own docs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HashStats {
    /// The interior's active bucket count — ERTS's `nactive`.
    ///
    /// For the chained interior this is the bucket-array length; for the CHAMP
    /// `set` it is the tree's addressable slot space (32 index slots per bitmap
    /// node, the CHAMP level's hash fan-out). Both are interior freedoms, so the
    /// *value* is not the oracle's, only the meaning is.
    pub active_buckets: usize,
    /// `sum(chain_len) / active_buckets` — the average number of objects per
    /// bucket.
    pub avg_chain_len: f64,
    /// The real standard deviation of the bucket chain lengths,
    /// `sqrt((sum(len^2) - avg * sum(len)) / active_buckets)`.
    pub std_dev_chain_len: f64,
    /// The standard deviation a **uniform** hash function would give for this
    /// average chain length, `sqrt(avg * (1 - 1/active_buckets))`.
    pub std_dev_expected: f64,
    /// The smallest bucket chain length (`0` whenever any bucket is empty, which
    /// is the ordinary case — the oracle's own minimum is `0` for any table whose
    /// bucket array is not full).
    pub min_chain_len: usize,
    /// The largest bucket chain length.
    pub max_chain_len: usize,
    /// How many objects the table is **holding although they are no longer in the
    /// index** because it is fixed (`ets:safe_fixtable/2`) — ERTS's `kept_items`,
    /// the pseudo-deleted `DbTerm`s.
    ///
    /// This **agrees with the oracle by value**: both count one per removed
    /// object whose storage is retained for a live traversal
    /// ([`Table::deferred_objects`](crate::Table::deferred_objects)), and both are
    /// `0` on an unfixed table — which is the ordinary case.
    pub kept_items: usize,
    /// The table's hash lock count — ERTS's `tb->hash.nlocks`.
    ///
    /// This **agrees with the oracle by value**: it is the modelled lock count of
    /// the effective `{write_concurrency, …}` mode
    /// ([`WriteConcurrency::lock_count`](crate::WriteConcurrency::lock_count) —
    /// `1` when not fine-locked, else the normalized count, `64` for the `true` and
    /// `auto` forms), not a measurement of this crate's interior, which takes no
    /// locks (see the crate `CONTEXT.md`, Known Issues).
    pub nlocks: u32,
}

/// The tree family's node/depth figures — the oracle's three-element `stats` tuple
/// for a **catree** (fine-locked `ordered_set`), `db_calc_stats_catree`
/// (`../../erts/emulator/beam/erl_db_catree.c:2471-2506`).
///
/// ERTS's catree is a binary route tree over base nodes that hold the objects. This
/// crate's `ordered_set` is a K-ary B+-tree, so the three figures measure **that**
/// tree: its internal (routing) nodes, its leaves, and the deepest number of routing
/// levels on any root-to-leaf path. The *values* therefore differ from the oracle's
/// — the oracle reports `{0,1,0}` for every catree it builds, because its base node
/// absorbs the whole table — while the shape and meaning are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeStats {
    /// The number of internal (routing) nodes — ERTS's `route_nodes`.
    pub route_nodes: usize,
    /// The number of leaves holding the objects — ERTS's `base_nodes`.
    pub base_nodes: usize,
    /// The largest number of routing levels on a root-to-leaf path — ERTS's
    /// `max_depth` (a root that is itself a leaf is depth `0`).
    pub max_depth: usize,
}

/// The answer to `ets:info(Tab, stats)` for one table: the hash family's
/// occupancy tuple, the tree family's shape tuple, or the atom `false`.
///
/// `Absent` is what the oracle answers for a table whose interior is neither a hash
/// table nor a catree — i.e. an `ordered_set` created **without** an effective
/// `write_concurrency` (`db_get_info`'s `else ret = am_false` arm). A fine-locked
/// `ordered_set` *is* a catree in the oracle, so it reports [`Tree`](TableStats::Tree);
/// this crate's B+-tree fills that shape with its own figures.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TableStats {
    /// A `set`/`bag`/`duplicate_bag`: the eight-element occupancy tuple.
    Hash(HashStats),
    /// A fine-locked `ordered_set`: the three-element node/depth tuple.
    Tree(TreeStats),
    /// The oracle's atom `false`: no statistics for this interior.
    Absent,
}

/// A running **bucket-chain histogram**: the interior's own occupancy measurement,
/// which [`Table::stats`](crate::Table::stats) turns into a [`HashStats`] by adding
/// the two table-level figures (`kept_items`, `nlocks`).
///
/// A chain length is the number of objects stored in one *bucket* — for the chained
/// interior one entry of its bucket array (every object of every key that hashes
/// there), for the CHAMP one index slot of one bitmap node (1 for an inline entry,
/// `0` for a routing or free slot, and the whole list for a full-hash collision
/// node). Feeding every bucket — empty ones included — is what makes
/// [`min_chain_len`](HashStats::min_chain_len) and the deviations match the
/// oracle's accounting, which walks all `nactive` slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BucketChains {
    /// How many buckets were fed (the active bucket count).
    buckets: usize,
    /// `sum(chain_len)` — the number of objects in the measured bucket space.
    sum: usize,
    /// `sum(chain_len^2)`.
    sq_sum: usize,
    /// The smallest chain length fed.
    min: usize,
    /// The largest chain length fed.
    max: usize,
}

impl BucketChains {
    /// An empty histogram (no buckets yet).
    pub(crate) fn new() -> BucketChains {
        BucketChains {
            buckets: 0,
            sum: 0,
            sq_sum: 0,
            min: usize::MAX,
            max: 0,
        }
    }

    /// Record one bucket holding `chain_len` objects.
    ///
    /// `sum`/`sq_sum` are `usize` sums of counts; a table with more than
    /// `usize::MAX` objects is not representable in the first place (the object
    /// count itself is a `usize`), and `sq_sum` stays far below the machine word
    /// limit for any table that fits in memory.
    pub(crate) fn add_chain(&mut self, chain_len: usize) {
        self.buckets += 1;
        self.sum += chain_len;
        self.sq_sum += chain_len.saturating_mul(chain_len);
        if chain_len < self.min {
            self.min = chain_len;
        }
        if chain_len > self.max {
            self.max = chain_len;
        }
    }

    /// Record `count` buckets of `chain_len` objects each — the bulk form the
    /// CHAMP's slot space uses (one call for `n` identical slots instead of `n`).
    pub(crate) fn add_chains(&mut self, count: usize, chain_len: usize) {
        if count == 0 {
            return;
        }
        self.buckets += count;
        self.sum += count.saturating_mul(chain_len);
        self.sq_sum += count.saturating_mul(chain_len.saturating_mul(chain_len));
        if chain_len < self.min {
            self.min = chain_len;
        }
        if chain_len > self.max {
            self.max = chain_len;
        }
    }

    /// How many objects the measured bucket space holds (`sum(chain_len)`); the
    /// interior's own record count when every object was fed exactly once.
    pub(crate) fn objects(&self) -> usize {
        self.sum
    }

    /// Finish the measurement into the observable [`HashStats`], applying the
    /// oracle's derived formulas (`db_calc_stats_hash`,
    /// `../../erts/emulator/beam/erl_db_hash.c:4072-4114`).
    ///
    /// # The precision model is the C's
    ///
    /// `DbHashStats` types `avg_chain_len`, `std_dev_chain_len` and
    /// `std_dev_expected` as **`float`** (`../../erts/emulator/beam/erl_db_hash.h:121-129`),
    /// and `db_calc_stats_hash` assigns each of the three results back into that
    /// `float` field:
    ///
    /// ```text
    /// stats->avg_chain_len    = (float)sum / NACTIVE(tb);
    /// stats->std_dev_chain_len = sqrt((sq_sum - stats->avg_chain_len*sum) / NACTIVE(tb));
    /// stats->std_dev_expected  = sqrt(stats->avg_chain_len * (1 - 1.0/NACTIVE(tb)));
    /// ```
    ///
    /// so every one of the three observables is a `float`, computed from the
    /// *already rounded* `avg`. `db_get_info` then widens each to a double for the
    /// tuple (`../../erts/emulator/beam/erl_db.c:5594-5607`).
    ///
    /// This method reproduces that: the average is rounded through `f32` first, the
    /// two deviations are computed in `f64` from that rounded average and each
    /// result is rounded through `f32` again (`x as f32 as f64`). The double-only
    /// arithmetic would be *more* precise and would therefore print different
    /// digits — for the five-object `set` it gives `0.1383827311243621` /
    /// `0.13948102399497753` where the oracle prints
    /// `0.13838273286819458` / `0.13948102295398712`, the two `f32` roundings.
    ///
    /// Both deviations are a square root of a non-negative quantity; the radicands
    /// are clamped at `0.0` so floating-point rounding can never produce a `NaN`
    /// (the C's `sqrt` of a tiny negative would).
    ///
    /// `kept_items` is the caller's fixed-table retention count
    /// ([`Table::deferred_objects`](crate::Table::deferred_objects)) and `nlocks`
    /// the modelled lock count of the table's effective write-concurrency mode
    /// ([`WriteConcurrency::lock_count`](crate::WriteConcurrency::lock_count)).
    pub(crate) fn hash_stats(&self, kept_items: usize, nlocks: u32) -> HashStats {
        /// Round a derived figure the way the C's `float` field does.
        fn as_c_float(value: f64) -> f64 {
            value as f32 as f64
        }

        let buckets = self.buckets;
        let (avg, std_dev, std_dev_expected) = if buckets == 0 {
            // No bucket space at all: every figure is `0.0` (there is nothing to
            // average). The interiors always have at least one bucket, so this is
            // the empty-input guard rather than a reachable table state.
            (0.0, 0.0, 0.0)
        } else {
            let n = buckets as f64;
            // `sum(chain_len)` — the number of objects the bucket space holds.
            let objects = self.objects() as f64;
            // `avg_chain_len` is a `float` field, so the average is rounded first
            // and the deviations below are derived from that rounded value.
            let avg = as_c_float(objects / n);
            // `sum(len^2) - avg * sum(len)` distributed over the buckets.
            let real = (self.sq_sum as f64 - avg * objects) / n;
            // A uniform hash's binomial deviation for this load.
            let expected = avg * (1.0 - 1.0 / n);
            (
                avg,
                as_c_float(real.max(0.0).sqrt()),
                as_c_float(expected.max(0.0).sqrt()),
            )
        };
        HashStats {
            active_buckets: buckets,
            avg_chain_len: avg,
            std_dev_chain_len: std_dev,
            std_dev_expected,
            min_chain_len: if buckets == 0 { 0 } else { self.min },
            max_chain_len: self.max,
            kept_items,
            nlocks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_histogram_reports_zeroes() {
        let stats = BucketChains::new().hash_stats(0, 1);
        assert_eq!(stats.active_buckets, 0);
        assert_eq!(stats.avg_chain_len, 0.0);
        assert_eq!(stats.std_dev_chain_len, 0.0);
        assert_eq!(stats.std_dev_expected, 0.0);
        assert_eq!(stats.min_chain_len, 0);
        assert_eq!(stats.max_chain_len, 0);
        assert_eq!(stats.kept_items, 0);
        assert_eq!(stats.nlocks, 1);
    }

    /// The oracle's own measured figures for a 5-object `set` — 5 objects spread
    /// one per bucket over 256 buckets — so this pins the *formulas* **and the
    /// precision model** against `db_calc_stats_hash`'s output: the values ERTS
    /// printed are `{256,0.01953125,0.13838273286819458,0.13948102295398712,0,1,0,1}`.
    ///
    /// The two deviations are asserted **exactly**: they are the `f32` roundings of
    /// `0.1383827311243621` / `0.13948102399497753` (the double-only results), and
    /// reproducing the oracle's digits is precisely what
    /// [`BucketChains::hash_stats`]'s `float` model is for.
    #[test]
    fn the_hash_formulas_reproduce_the_oracles_figures() {
        let mut chains = BucketChains::new();
        chains.add_chains(251, 0);
        chains.add_chains(5, 1);
        let stats = chains.hash_stats(0, 1);
        assert_eq!(stats.active_buckets, 256);
        assert_eq!(chains.objects(), 5);
        assert_eq!(stats.avg_chain_len, 0.01953125);
        assert_eq!(stats.min_chain_len, 0);
        assert_eq!(stats.max_chain_len, 1);
        assert_eq!(stats.std_dev_chain_len, 0.138_382_732_868_194_58);
        assert_eq!(stats.std_dev_expected, 0.139_481_022_953_987_12);
    }

    /// The C's `float` model, spelled out: the double-only arithmetic would print
    /// different digits, so the two `f32` roundings are load-bearing.
    #[test]
    fn the_float_rounding_is_what_matches_the_oracle() {
        // The same 5-in-256 histogram, computed without the `f32` roundings.
        let n = 256.0f64;
        let sum = 5.0f64;
        let sq_sum = 5.0f64;
        let avg = sum / n;
        let real = (sq_sum - avg * sum) / n;
        let expected = avg * (1.0 - 1.0 / n);
        assert_eq!(real.sqrt(), 0.138_382_731_124_362_1);
        assert_eq!(expected.sqrt(), 0.139_481_023_994_977_53);
        // ... and each is a different f64 from the oracle's printed figure, which is
        // the `f32` rounding of it.
        assert_ne!(real.sqrt(), 0.138_382_732_868_194_58);
        assert_eq!(real.sqrt() as f32 as f64, 0.138_382_732_868_194_58);
        assert_eq!(expected.sqrt() as f32 as f64, 0.139_481_022_953_987_12);
    }

    /// A uniformly loaded table: one object per bucket gives an average of exactly
    /// `1.0`, a real deviation of `0.0` and the binomial expectation
    /// `sqrt(1 - 1/buckets)` rounded through the C's `float` field (the oracle's
    /// 10000-object `set` prints `{10000,1.0,1.0431681871414185,0.9999499917030334,0,7,0,1}`
    /// — a max of 7 from the hash's own collisions, and the `f32`-rounded
    /// expectation `0.9999499917030334`).
    #[test]
    fn a_uniform_load_has_unit_average_and_zero_real_deviation() {
        let mut chains = BucketChains::new();
        chains.add_chains(10_000, 1);
        let stats = chains.hash_stats(0, 1);
        assert_eq!(stats.avg_chain_len, 1.0);
        assert_eq!(stats.std_dev_chain_len, 0.0);
        assert_eq!(stats.min_chain_len, 1);
        assert_eq!(stats.max_chain_len, 1);
        let expected = ((1.0 - 1.0 / 10_000f64).sqrt()) as f32 as f64;
        assert_eq!(stats.std_dev_expected, expected);
        // The oracle's own printed figure for its 10 000-object table.
        assert_eq!(stats.std_dev_expected, 0.999_949_991_703_033_4);
    }

    /// A skewed distribution: the real deviation must exceed the uniform
    /// expectation (that is what the pair of figures is *for*), and the sums must
    /// account for every object exactly once.
    #[test]
    fn a_skewed_load_has_a_real_deviation_above_the_expected_one() {
        let mut chains = BucketChains::new();
        chains.add_chains(3, 0);
        chains.add_chain(1);
        chains.add_chain(6);
        chains.add_chain(13);
        let stats = chains.hash_stats(2, 64);
        assert_eq!(stats.active_buckets, 6);
        assert_eq!(chains.objects(), 20);
        assert_eq!(stats.min_chain_len, 0);
        assert_eq!(stats.max_chain_len, 13);
        assert!(stats.std_dev_chain_len > stats.std_dev_expected);
        // The two table-level figures are carried through untouched.
        assert_eq!(stats.kept_items, 2);
        assert_eq!(stats.nlocks, 64);
    }
}
