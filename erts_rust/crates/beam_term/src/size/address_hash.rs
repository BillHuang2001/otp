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

//! The sizing walk's path-set hasher: a multiplicative **address mix**, not SipHash.
//!
//! The walk ([`crate::size`]) keeps one set of `usize` object addresses — the compound
//! objects on the current DFS path — and consults it only for **exact membership**
//! (insert on enter, remove on exit, membership for the acyclicity pass). Nothing about
//! the answer depends on the hash: a collision costs one extra probe, and a bad hash
//! can only make the table slower, never wrong. So the quality bar is "spread a set of
//! heap addresses across buckets", which the standard library's `RandomState`
//! (SipHash-1-3, keyed, ~2 multiply-heavy rounds on the whole 64-bit word) massively
//! overshoots: it is a *hash-flooding* defence for attacker-chosen keys, and these keys
//! are addresses the walk itself produced.
//!
//! What is used instead is the classic `FxHash` shape (rustc's `FxHasher`): one
//! multiply by an odd 64-bit constant, then one fold of the high half into the low half.
//!
//! ```text
//! h = (h ^ v) * 0x51_7c_c1_b7_27_22_0a_95;   // odd multiplier: bijective in v
//! h = h ^ (h >> 32);                          // fold high bits down
//! ```
//!
//! The fold is not decoration: heap terms are at least word-aligned, so the low three
//! bits of every address are zero, and an odd multiply preserves trailing zeros. Without
//! the fold the bucket index (`hashbrown` takes it from the *low* bits) would only ever
//! select every eighth bucket, turning a nice uniform table into a clustered one. The
//! multiply pushes that entropy into the high half; the shift brings it back.
//!
//! This is deliberately **not** on any security boundary: the keys are heap addresses in
//! this process, so there is no untrusted input to flood the table with, and the walk is
//! bounded by the same `limit`/cycle rules either way.

use std::hash::{BuildHasher, Hasher};

/// The odd multiplier, rustc's `FxHasher` constant (`0x51_7c_c1_b7_27_22_0a_95`).
const SEED: u64 = 0x517c_c1b7_2722_0a95;

/// [`BuildHasher`] for [`AddressHasher`]: a zero-sized type, so building one is free.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(super) struct AddressHashBuilder;

impl BuildHasher for AddressHashBuilder {
    type Hasher = AddressHasher;

    #[inline]
    fn build_hasher(&self) -> AddressHasher {
        AddressHasher(0)
    }
}

/// The one-round multiplicative address mix described in the [module docs](self).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct AddressHasher(u64);

impl AddressHasher {
    /// Folds one 64-bit value into the accumulator.
    #[inline]
    fn mix(&mut self, v: u64) {
        let mut h = (self.0 ^ v).wrapping_mul(SEED);
        h ^= h >> 32;
        self.0 = h;
    }
}

impl Hasher for AddressHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    /// Total over any byte run so the hasher stays usable for any `Hash` impl; the walk
    /// only ever hashes a `usize`, which takes [`write_usize`](Hasher::write_usize).
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.mix(u64::from(b));
        }
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.mix(n as u64);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.mix(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mix is a hash, not a state machine: it must not depend on call order beyond
    /// the accumulator, and it must actually spread word-aligned addresses.
    #[test]
    fn word_aligned_addresses_spread_over_the_full_bucket_range() {
        // The lowest byte the mix derives is exactly what `hashbrown` masks into a
        // bucket index, so this is the property the fold exists for.
        let mut seen = std::collections::HashSet::new();
        for i in 0..1024u64 {
            let addr = ((0x7f00_0000_0000u64 + i * 8) * 8) as usize; // 64-byte aligned
            let mut h = AddressHashBuilder.build_hasher();
            h.write_usize(addr);
            seen.insert((h.finish() as u32) & 0xff);
        }
        assert!(
            seen.len() >= 128,
            "a multiplicative mix without the high-to-low fold would land in ~32 of 256 \
             low-byte buckets; got {}",
            seen.len()
        );
    }

    /// Distinct addresses must (overwhelmingly) hash apart: the set only ever compares
    /// hashes to *find candidates*, so a degenerate mix would not be wrong — but it would
    /// be O(n) per probe, which is the cost this module exists to avoid.
    #[test]
    fn distinct_addresses_do_not_collide() {
        let mut collisions = 0usize;
        let mut seen = std::collections::HashSet::new();
        for i in 0..4096u64 {
            let mut h = AddressHashBuilder.build_hasher();
            h.write_usize((0x7f00_0000_0000u64 + i * 8) as usize);
            if !seen.insert(h.finish()) {
                collisions += 1;
            }
        }
        assert_eq!(collisions, 0, "the address mix collided on {collisions} addresses");
    }

    #[test]
    fn the_hasher_is_deterministic() {
        let mut a = AddressHashBuilder.build_hasher();
        let mut b = AddressHashBuilder.build_hasher();
        a.write_usize(0xdead_beef);
        b.write_usize(0xdead_beef);
        assert_eq!(a.finish(), b.finish());
    }
}
