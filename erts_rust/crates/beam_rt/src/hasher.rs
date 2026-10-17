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

//! A small **deterministic, non-cryptographic** hasher for the runtime's internal, per-pid/per-id
//! bookkeeping structures — the alternative to [`std::collections::hash_map::RandomState`]'s
//! SipHash-1-3.
//!
//! # Why
//!
//! `RandomState` is seeded per process from the OS, so it costs a SipHash-1-3 round per key plus
//! the per-hasher key setup. That is the right trade for a table exposed to attacker-chosen keys;
//! it is a measurable cost for the runtime's **per-pid** structures, whose keys are *internal
//! dense ids this host itself mints* (`beam_process::ProcessId { number, serial }`), and which are
//! probed on the delivery/admission paths rather than once per program:
//!
//! * the per-pid **foreign-reachability latch** ([`crate::runtime::gc`]) — one `contains` at every
//!   heap-admission point that passes its occupancy short-circuit and one `insert` at every valve
//!   **decline** (`ensure_heap_full`), which a two-process ping-pong reaches on essentially every
//!   allocating instruction (a `perf` profile of `srprof:plain/1` attributes ~6% of the run to
//!   `RandomState::hash_one`/`sip::Hasher::write`/`finish` there).
//!
//! # What
//!
//! FNV-1a's **xor-then-multiply** mixing rule, applied **per machine word** instead of per byte,
//! with an odd 64-bit multiplier and one final avalanche round:
//!
//! ```text
//! hash  = (hash.rotate_left(5) ^ word).wrapping_mul(MIX_MULTIPLIER)      // one round per word
//! word  = every 8 bytes of `write`, every `write_u8..u64`/`write_usize` value, every `write_i*`
//!         value sign-extended (as rustc's own `FxHasher` does)
//! ```
//!
//! The rotate is what makes one round enough: the multiplier alone would only push bits *up*, and
//! the low half is the half `hashbrown` indexes by (`hash & bucket_mask`) while the top seven bits
//! are its control byte — so the rotation folds the incoming high bits back into the low ones.
//!
//! The starting state is FNV-1a 64-bit's offset basis (not zero), so an all-zero key does not hash
//! to an all-zero state, and one more round (`x ^= x >> 32; x *= FINAL_MULTIPLIER; x ^= x >> 29`)
//! avalanches the state in `finish`, which is what makes a *one-word* key — a pid, an index, a
//! `u64` — spread over the whole 64 bits rather than over the low bits of one multiplication.
//!
//! **Two deliberate deviations from plain byte-wise FNV-1a**, both measured, not stylistic:
//!
//! 1. FNV-1a is defined byte-at-a-time, and each byte costs one *dependent* multiply (≈4 cycles on
//!    x86-64). A `ProcessId` derives `Hash`, so one key is **two** `write_u64` calls — 16 bytes,
//!    i.e. sixteen dependent multiplies — where the word form pays two. That matters more here
//!    than key width usually does, because the latch is probed per message/instruction rather than
//!    per lookup of a large table.
//! 2. FNV-1a's own prime (`2^40 + 0x1b3`) multiplies a byte into only the low ~40 bits of the
//!    state, so its low bits — the half `hashbrown` indexes by — mix weakly for *word* inputs.
//!    [`MIX_MULTIPLIER`] (the constant rustc's `FxHasher` uses) is dense in its low half, so one
//!    round diffuses both halves.
//!
//! # Why this is sound for these structures
//!
//! A *set-membership* answer does not depend on the hasher: equal keys still hash equally (a
//! `Hash` impl always uses the same writer for the same type), and two keys that collide are
//! separated by the table's own equality probe, so `contains`/`insert` return exactly what a
//! `RandomState`-backed `HashSet` returns — the verdicts are bit-identical, only their cost and
//! the (unobservable) iteration order differ.
//!
//! The three properties this crate relies on are therefore:
//!
//! * **none of them is iterated for an observable ordered result** — the latch's only iteration is
//!   `Runtime::prune_gc_requests`' `retain` over process liveness, whose result is a *set*, and the
//!   rest are `get`/`entry`/`insert`/`remove` probes or an order-independent scan, so nothing here
//!   reaches a term, a message or a `system_info` value;
//! * **their keys are not attacker-chosen** — a pid is minted by
//!   [`beam_process::ProcessTable`] from its own monotonically growing slot numbers and a timer key
//!   is minted by the runtime itself (its `RefAllocator` or `Timers`' id counter), so there is no
//!   HashDoS surface for a seeded hasher to defend (nothing in this runtime hashes wire-supplied
//!   keys with [`FastBuildHasher`]); and
//! * **determinism is an improvement here, not a regression** — the seed buys unpredictability
//!   that nothing in this runtime wants, and a fixed basis makes each structure's probe order
//!   reproducible across runs (which the structural counters' tests rely on) as rustc's own
//!   `FxHashMap` relies on it.
//!
//! Anything that *does* hash attacker-supplied keys (the atom table, the code cache) keeps
//! `RandomState`; this module exists for the internal, id-keyed, verdict-only cases.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

/// The odd 64-bit mixing multiplier (the constant rustc's `FxHasher` uses): odd, so the multiply
/// is invertible and no input bit is shifted out of reach, and dense in its low half, so one round
/// diffuses a word's bits over the whole state.
const MIX_MULTIPLIER: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The starting state: FNV-1a 64-bit's offset basis, so an all-zero key does not hash to the
/// all-zero state.
const BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// [`FastHasher::finish`]'s avalanche multiplier (the golden-ratio odd constant).
const FINAL_MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

/// The word-mixing hasher: FNV-1a's xor-then-multiply rule per machine word (see the module
/// docs), with one final avalanche round in [`finish`](Hasher::finish).
///
/// It is `Copy` + `Default` (a constant state), which is what lets [`BuildHasherDefault`] be the
/// one-`Default`-impl [`BuildHasher`](std::hash::BuildHasher) the collection aliases are built
/// over.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FastHasher {
    /// The state: [`BASIS`] for a fresh hasher, then one [`Self::mix`] round per key word.
    hash: u64,
}

impl Default for FastHasher {
    /// A hasher in the FNV-1a offset-basis state (the state `build_hasher` hands out per probe —
    /// the aliases' `Default` is this constant, never a seeded or clocked value).
    fn default() -> FastHasher {
        FastHasher { hash: BASIS }
    }
}

impl FastHasher {
    /// One mixing round: xor the word into the state (rotated so the state's high bits re-enter
    /// the low ones) and multiply by [`MIX_MULTIPLIER`].
    #[inline]
    fn mix(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(MIX_MULTIPLIER);
    }
}

impl Hasher for FastHasher {
    /// The state after one avalanche round: `x ^= x >> 32; x *= FINAL_MULTIPLIER; x ^= x >> 29`.
    ///
    /// `hashbrown` takes a key's bucket index from the *low* bits and its 7-bit control byte from
    /// the *top* ones, so a one-word key (the latch's `ProcessId` is two words, but an index or a
    /// `u64` key is one) must be spread over the whole word before that happens.
    #[inline]
    fn finish(&self) -> u64 {
        let mut x = self.hash;
        x ^= x >> 32;
        x = x.wrapping_mul(FINAL_MULTIPLIER);
        x ^= x >> 29;
        x
    }

    /// Mixes `bytes` as 8-byte words, then a 4-/2-/1-byte tail — so `write(&x.to_ne_bytes())` and
    /// `write_u64(x)` agree for every `x` (the property [`write_u64`](Hasher::write_u64) documents).
    fn write(&mut self, bytes: &[u8]) {
        let mut words = bytes.chunks_exact(8);
        for word in &mut words {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(word);
            self.mix(u64::from_ne_bytes(buf));
        }
        let mut rest = words.remainder();
        if rest.len() >= 4 {
            let (head, tail) = rest.split_at(4);
            let mut buf = [0u8; 4];
            buf.copy_from_slice(head);
            self.mix(u64::from(u32::from_ne_bytes(buf)));
            rest = tail;
        }
        if rest.len() >= 2 {
            let (head, tail) = rest.split_at(2);
            let mut buf = [0u8; 2];
            buf.copy_from_slice(head);
            self.mix(u64::from(u16::from_ne_bytes(buf)));
            rest = tail;
        }
        if let Some(&byte) = rest.first() {
            self.mix(u64::from(byte));
        }
    }

    /// One round over the value — the word form of FNV-1a's byte chain (see the module docs).
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.mix(u64::from(i));
    }

    /// One round over the value (identical to [`write_u8`](Hasher::write_u8) for the same bits).
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.mix(u64::from(i));
    }

    /// One round over the value (identical to a 4-byte [`write`](Hasher::write)).
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.mix(u64::from(i));
    }

    /// One round over the value (identical to an 8-byte [`write`](Hasher::write)): this is the
    /// pair `ProcessId`'s derived `Hash` uses, so the latch's probe costs two rounds, not sixteen.
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.mix(i);
    }

    /// One round over the native-endian value (64-bit on every target this runtime builds for).
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.mix(i as u64);
    }

    /// One round over the **sign-extended** value, exactly as rustc's `FxHasher` does: it keeps
    /// `-1i8` distinct from `255u8`. A `Hash` impl uses one writer per field type, so this can
    /// never make two *equal* keys hash differently.
    #[inline]
    fn write_i8(&mut self, i: i8) {
        self.mix(i as u64);
    }

    /// One round over the sign-extended value (see [`write_i8`](Hasher::write_i8)).
    #[inline]
    fn write_i16(&mut self, i: i16) {
        self.mix(i as u64);
    }

    /// One round over the sign-extended value (see [`write_i8`](Hasher::write_i8)).
    #[inline]
    fn write_i32(&mut self, i: i32) {
        self.mix(i as u64);
    }

    /// One round over the sign-extended value (see [`write_i8`](Hasher::write_i8)).
    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.mix(i as u64);
    }

    /// One round over the sign-extended value (see [`write_i8`](Hasher::write_i8)).
    #[inline]
    fn write_isize(&mut self, i: isize) {
        self.mix(i as u64);
    }
}

/// The `BuildHasher` half: a zero-sized, `Default` builder handing out [`FastHasher`]s in the
/// [`BASIS`] state.
pub(crate) type FastBuildHasher = BuildHasherDefault<FastHasher>;

/// A [`HashSet`] over [`FastBuildHasher`] — the convenience alias for an internal, verdict-only
/// key set (see the module docs for when this is the right hasher and when it is not).
///
/// The alias is only spelled out where a *typed* set is needed: the latch's `ForeignLatch` writes
/// the hasher inline at its one definition site, while `Runtime::async_dist` uses this alias, and
/// this module's own tests exercise it.
#[allow(dead_code)]
pub(crate) type FastHashSet<T> = HashSet<T, FastBuildHasher>;

/// A [`HashMap`] over [`FastBuildHasher`] — the convenience alias for an internal map whose
/// iteration order is not observable (see the module docs). It backs the runtime's internal
/// id-keyed bookkeeping — the `Timers` `pending`/`refs` indexes, `RunnerState`'s
/// `engines`/`results`/`exits`/`errors`, `TimerWakes`'s `deliver`/`resume`, `Runtime::mailboxes`
/// and the façade's `group_leaders`/`monitor_nodes`/`timer_resumes` — each only probed by an
/// internal id and never iterated for an observable order.
#[allow(dead_code)]
pub(crate) type FastHashMap<K, V> = HashMap<K, V, FastBuildHasher>;

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::hash::Hash;

    use beam_process::ProcessId;

    use super::*;

    /// A hasher state fed by `write`, finished — the value `HashSet`/`HashMap` would probe with.
    fn hash_with(write: impl FnOnce(&mut FastHasher)) -> u64 {
        let mut hasher = FastHasher::default();
        write(&mut hasher);
        hasher.finish()
    }

    /// A **mixed pid corpus**: small and large slot numbers against small, huge and boundary
    /// serials, so the equivalence below is not checked over a single-shaped key.
    fn pid_corpus() -> Vec<ProcessId> {
        let numbers = [
            0u64,
            1,
            2,
            3,
            7,
            255,
            256,
            65_535,
            65_536,
            1 << 20,
            1 << 32,
            u64::from(u32::MAX),
            u64::MAX - 1,
            u64::MAX,
        ];
        let serials = [0u64, 1, 5, 1 << 16, u64::from(u32::MAX), u64::MAX];
        let mut corpus = Vec::new();
        for number in numbers {
            for serial in serials {
                corpus.push(ProcessId::new(number, serial));
            }
        }
        corpus
    }

    /// **The set answers membership exactly as a `RandomState`-backed one does** over a mixed pid
    /// corpus — every insert verdict, every length, every `contains` verdict — which is the
    /// property the latch's soundness rests on (a membership answer is hasher-independent).
    #[test]
    fn membership_matches_a_randomstate_backed_set_over_a_pid_corpus() {
        let corpus = pid_corpus();
        let mut fast: FastHashSet<ProcessId> = FastHashSet::default();
        let mut seeded: HashSet<ProcessId> = HashSet::new();
        for (index, pid) in corpus.iter().enumerate() {
            // A mixed insertion pattern: every third key repeats its predecessor, so the
            // `insert` verdict (new vs already present) is exercised on both arms.
            let key = if index > 1 && index % 3 == 0 {
                corpus[index - 1]
            } else {
                *pid
            };
            assert_eq!(
                fast.insert(key),
                seeded.insert(key),
                "the insert verdicts agree for {key:?}"
            );
            assert_eq!(fast.len(), seeded.len(), "the lengths agree");
        }
        for pid in &corpus {
            assert_eq!(
                fast.contains(pid),
                seeded.contains(pid),
                "the membership verdicts agree for {pid:?}"
            );
        }
        // A pid in neither set, and a removal: the negative verdicts agree too.
        let absent = ProcessId::new(u64::MAX - 3, 0);
        assert!(!fast.contains(&absent) && !seeded.contains(&absent));
        assert_eq!(fast.remove(&corpus[0]), seeded.remove(&corpus[0]));
        assert_eq!(fast.len(), seeded.len());
        // The two tables really hold the same *set* (they need not iterate in the same order —
        // that is the one thing a hasher change may alter, and nothing in this crate observes it).
        let mut from_fast: Vec<ProcessId> = fast.iter().copied().collect();
        let mut from_seeded: Vec<ProcessId> = seeded.iter().copied().collect();
        from_fast.sort();
        from_seeded.sort();
        assert_eq!(from_fast, from_seeded, "the same keys, whatever the order");
    }

    /// The word writers and the byte writer agree bit-for-bit, which is what makes
    /// "FNV-1a over the bytes any `Hash` impl writes" hold for both spellings of a key.
    #[test]
    fn the_word_writers_and_the_byte_writer_agree() {
        for value in [0u64, 1, 255, 256, u64::from(u32::MAX), u64::MAX, 0xdead_beef] {
            assert_eq!(
                hash_with(|h| h.write_u64(value)),
                hash_with(|h| h.write(&value.to_ne_bytes())),
                "write_u64({value}) is the 8-byte write"
            );
            assert_eq!(
                hash_with(|h| h.write_usize(value as usize)),
                hash_with(|h| h.write_u64(value)),
                "write_usize({value}) is the 64-bit word write"
            );
        }
        for value in [0u32, 1, 255, 65_536, u32::MAX] {
            assert_eq!(
                hash_with(|h| h.write_u32(value)),
                hash_with(|h| h.write(&value.to_ne_bytes())),
                "write_u32({value}) is the 4-byte write"
            );
        }
        for value in [0u8, 1, 127, 255] {
            assert_eq!(
                hash_with(|h| h.write_u8(value)),
                hash_with(|h| h.write(&[value])),
                "write_u8({value}) is the 1-byte write"
            );
        }
        // A signed writer hashes its *sign-extended* bits (rustc's `FxHasher` rule), so the two
        // writers are distinct on purpose — and each is stable.
        assert_ne!(
            hash_with(|h| h.write_i8(-1)),
            hash_with(|h| h.write_u8(255)),
            "the signed writer keeps -1 distinct from 255"
        );
        assert_eq!(
            hash_with(|h| h.write_i64(-1)),
            hash_with(|h| h.write_u64(u64::MAX)),
            "sign extension, not a magic value"
        );
    }

    /// **Deterministic and key-sensitive**: no seed, no clock, and two fresh hashers agree for the
    /// same key while distinct keys separate. Every claim here is a fixed function of the
    /// implementation (a deterministic hasher either separates this corpus or it does not — this
    /// is a structural pin, never a probabilistic hope).
    #[test]
    fn the_hasher_is_deterministic_and_key_sensitive() {
        let corpus = pid_corpus();
        let hashes: Vec<u64> = corpus
            .iter()
            .map(|pid| hash_with(|h| pid.hash(h)))
            .collect();
        // No seed: a second pass over the same keys reproduces the same values exactly.
        let again: Vec<u64> = corpus
            .iter()
            .map(|pid| hash_with(|h| pid.hash(h)))
            .collect();
        assert_eq!(hashes, again, "the hasher carries no per-process seed");
        // Distinct keys land on distinct states for this corpus (so the latch cannot degenerate
        // into one probe chain), and the empty-key state is not a fixed point of the key state.
        let mut distinct: Vec<u64> = hashes.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            corpus.len(),
            "every distinct pid hashes to a distinct value"
        );
        assert_ne!(FastHasher::default().finish(), 0, "the basis is not a zero state");
        assert!(
            hashes.iter().any(|&hash| hash != hashes[0]),
            "and the hash really depends on the key"
        );
    }

    /// The `HashMap` alias is usable and answers like a `RandomState` one (the same equivalence
    /// the set test pins, one level up).
    #[test]
    fn the_map_alias_answers_like_a_randomstate_map() {
        let mut fast: FastHashMap<ProcessId, u32> = FastHashMap::default();
        let mut seeded: HashMap<ProcessId, u32> = HashMap::new();
        for (index, pid) in pid_corpus().into_iter().enumerate() {
            let value = index as u32;
            assert_eq!(
                fast.insert(pid, value),
                seeded.insert(pid, value),
                "the insert verdicts agree for {pid:?}"
            );
        }
        for (pid, value) in &seeded {
            assert_eq!(fast.get(pid), Some(value), "the lookups agree for {pid:?}");
        }
        assert_eq!(fast.get(&ProcessId::new(u64::MAX - 5, 7)), None);
        assert_eq!(fast.len(), seeded.len());
    }
}
