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

//! The **build-once flag table** behind [`hook_sets`](crate::hook_sets)'s fourteen
//! `needs_*` seam predicates.
//!
//! # Why it exists
//!
//! A host asks the fourteen predicates about the *same* `(module, function, arity)`
//! key, back to back, on every registered-BIF dispatch
//! (`beam_rt`'s `DispatchHooks::of` asks twelve of them). Each predicate used to be a
//! `binary_search` over its own `&'static` table, so one dispatch paid fourteen table
//! walks — roughly fifty non-inlined `memcmp` calls. `perf` on a
//! `bench run --quick --runtime rust --workloads copy_bound` run attributed **4.62 %**
//! of cycles to `<[(&str, &str, u32)]>::binary_search_by` and much of the **7.59 %**
//! `__memcmp_avx2_movbe` share to these tables.
//!
//! This module answers the same fourteen questions with **one** probe: [`flags`] hashes
//! the key once and reads a `u16` whose bits are the membership answers, so each
//! `needs_*` predicate is a single bit test.
//!
//! # The table
//!
//! [`FlagTable::build`] runs at most once (a [`OnceLock`]) and is derived
//! **exclusively** from the fourteen `pub const` tables in
//! [`hook_sets`](crate::hook_sets) — the roster [`ROSTER`] names each `(bit, table)`
//! pair, and every key of every table contributes its bit to that key's entry
//! (OR-ing, so a key listed by two tables gets both bits).
//!
//! The table is **two arrays**, laid out so that the array a lookup walks stays in L1:
//!
//! * [`BUCKETS`] open-addressed, linearly probed probe slots (`[Bucket; 1024]`, **8
//!   KiB**): a [`Bucket`] is `{ tag: u32, flags: u16, key: u16 }`, i.e. eight bytes and
//!   so eight buckets per cache line, and `key == EMPTY_KEY` marks a free slot, which
//!   also ends a probe sequence; and
//! * `keys`, the identity array: one [`Key`] (`{ module, function, arity }`) per
//!   distinct key — at most [`ROSTER_KEYS`] = 226 entries, and read **only** when a
//!   bucket's [`tag`](fingerprint_tag) matches the queried fingerprint, so the common
//!   absent-key probe never touches it.
//!
//! The tables hold 226 raw keys (222 distinct), so the load factor is at most 0.22 and
//! a probe sequence almost always ends at its first bucket.
//!
//! # The fingerprint
//!
//! [`fingerprint`] is a **bounded-load** fingerprint, not a byte fold. It mixes the
//! two names' lengths, the arity, and the leading and trailing eight-byte window of
//! each name ([`head_word`]/[`tail_word`]: at most four `u64` loads per key, each a
//! safe length-guarded read, zero-padded when a name is shorter than its window), then
//! avalanches the accumulator **once** ([`avalanche`]: two xor-shifts around one
//! multiply). That is two multiplies and four loads for a whole key, against the one
//! dependent `imul` per name byte a byte-wise FNV-1a fold paid.
//!
//! The low [`BUCKETS`] bits of the result select the starting slot; the high 32 bits
//! are the slot's stored tag.
//!
//! The windows are what makes the cost bounded, and the price is that two names that
//! agree in length *and* in their leading and trailing eight bytes can share a
//! fingerprint — a name longer than sixteen bytes differing only in its middle, say.
//! That is only ever a collision: see below.
//!
//! # Correctness: identical by construction
//!
//! The flags are built *from the same tables the predicates used to search*, so
//!
//! ```text
//! flags(m, f, a) & FLAG_X != 0   <=>   (m, f, a) ∈ TABLE_X     (by construction)
//! ```
//!
//! for every key, present or absent: a key absent from all fourteen tables has
//! `flags == 0`, so all fourteen predicates answer `false` exactly as the
//! `binary_search`es did. Nothing here re-derives a consumer set — [`ROSTER`] is the
//! only input, and the module keeps no copy of any table.
//!
//! Two compile-time guards keep the roster honest:
//!
//! * [`ROSTER`] is a fixed-length array that [`FlagTable::build`] destructures
//!   **exhaustively**, so a table added to (or removed from) the roster fails to
//!   compile instead of silently losing its bit; and
//! * [`ALL_MASK`] is the fold of [`ALL`] and is asserted (at compile time) to be
//!   `(1 << 14) - 1`, i.e. the fourteen bits are pairwise distinct and fill bits
//!   `0..14`, while [`ROSTER_KEYS`] is asserted to fit in [`BUCKETS`] — which is also
//!   what makes a probe sequence terminate (at least one bucket stays empty).
//!
//! The equivalence is then pinned executably: the test module sweeps every key of
//! `crate::registry::default_registry()`, every one of those keys at `arity + 1`, and
//! a set of near-missing names, and asserts each of the fourteen bits against a
//! **linear scan** of the corresponding table
//! (`the_flags_agree_with_a_linear_scan_of_every_table`).
//!
//! # Collisions: the fingerprint only picks a starting slot
//!
//! **Soundness never depends on the fingerprint.** A lookup compares the full
//! `(module, function, arity)` of every slot it walks past before that slot's flags are
//! trusted, so two keys that share a fingerprint — the same tag *and* the same starting
//! slot — stay distinct: the second insert probes past the first, and every later
//! lookup for either walks the shared sequence and matches on the full key. A key that
//! is not in the table is answered `0` when the walk reaches a free slot. The
//! fingerprint only chooses where the walk starts, and the stored tag is only a
//! pre-filter that keeps a slot which is not ours from touching the identity array.
//!
//! # Why there is no cache in front of it
//!
//! A last-key or per-thread memo was deliberately **not** added. Comparing the names
//! by **pointer** is unsound across calls — a name string freed and reallocated at the
//! same address with the same length would produce a false hit — so a memo would have
//! to compare the bytes anyway, which is what this table already does once per
//! dispatch. (The host that owns a stable key — `beam_rt`'s dispatch memo — caches
//! this module's whole answer and that of the registry lookup together, per resolved
//! key; the correctness of *that* cache is the host's, and it is documented there.)
//!
//! # The per-call path is inlined
//!
//! [`flags`], [`FlagTable::lookup`] and [`fingerprint`] all carry `#[inline]`, and the
//! fourteen `needs_*` predicates in [`hook_sets`](crate::hook_sets) already do, so a
//! host's dispatch check carries the probe in its own body: no call, no prologue and
//! no register spills per predicate — a few loads, two multiplies and a `u16` bit test.

use std::sync::OnceLock;

use crate::hook_sets::{
    DICT_HOOK_BIFS, EXPORT_GATE_BIFS, FUN_CALLER_BIFS, LOCAL_FUN_IDENTITY_BIFS, LOCAL_NODE_BIFS,
    MEMORY_HOOK_BIFS, MODULE_INFO_BIFS, NIF_LOADER_BIFS, PERSISTENT_TERM_BIFS, PORT_HOOK_BIFS,
    PROCESS_HOOK_BIFS, RECORD_SOURCE_BIFS, RUNTIME_HOOKS_BIFS, SEND_HOOK_BIFS,
};

/// The [`MEMORY_HOOK_BIFS`] bit.
pub(crate) const MEMORY: u16 = 1 << 0;

/// The [`LOCAL_FUN_IDENTITY_BIFS`] bit.
pub(crate) const LOCAL_FUN_IDENTITY: u16 = 1 << 1;

/// The [`DICT_HOOK_BIFS`] bit.
pub(crate) const DICT: u16 = 1 << 2;

/// The [`PROCESS_HOOK_BIFS`] bit.
pub(crate) const PROCESS_HOOK: u16 = 1 << 3;

/// The [`PORT_HOOK_BIFS`] bit.
pub(crate) const PORT_HOOK: u16 = 1 << 4;

/// The [`SEND_HOOK_BIFS`] bit.
pub(crate) const SEND_HOOK: u16 = 1 << 5;

/// The [`NIF_LOADER_BIFS`] bit.
pub(crate) const NIF_LOADER: u16 = 1 << 6;

/// The [`EXPORT_GATE_BIFS`] bit.
pub(crate) const EXPORT_GATE: u16 = 1 << 7;

/// The [`RECORD_SOURCE_BIFS`] bit.
pub(crate) const RECORD_SOURCE: u16 = 1 << 8;

/// The [`MODULE_INFO_BIFS`] bit.
pub(crate) const MODULE_INFO: u16 = 1 << 9;

/// The [`PERSISTENT_TERM_BIFS`] bit.
pub(crate) const PERSISTENT_TERM: u16 = 1 << 10;

/// The [`FUN_CALLER_BIFS`] bit.
pub(crate) const FUN_CALLER: u16 = 1 << 11;

/// The [`LOCAL_NODE_BIFS`] bit.
pub(crate) const LOCAL_NODE: u16 = 1 << 12;

/// The [`RUNTIME_HOOKS_BIFS`] bit.
pub(crate) const RUNTIME_HOOKS: u16 = 1 << 13;

/// Every named bit, in declaration order — the one list [`ALL_MASK`] folds and the
/// compile-time witness that no bit is missing or aliased.
const ALL: [u16; 14] = [
    MEMORY,
    LOCAL_FUN_IDENTITY,
    DICT,
    PROCESS_HOOK,
    PORT_HOOK,
    SEND_HOOK,
    NIF_LOADER,
    EXPORT_GATE,
    RECORD_SOURCE,
    MODULE_INFO,
    PERSISTENT_TERM,
    FUN_CALLER,
    LOCAL_NODE,
    RUNTIME_HOOKS,
];

/// The fold of [`ALL`]: every named bit, built from the list rather than written out
/// as a literal.
const ALL_MASK: u16 = {
    let mut mask = 0;
    let mut i = 0;
    while i < ALL.len() {
        mask |= ALL[i];
        i += 1;
    }
    mask
};

/// The bits are pairwise distinct and together fill the low fourteen, so a future bit
/// cannot alias an existing one and no gap is left for it to fall into.
const _: () = assert!(ALL_MASK == (1 << ALL.len() as u16) - 1);

/// The probe-slot count: a power of two, and `>= 512` so that the 226 raw keys of the
/// fourteen tables (222 distinct) load it to at most 0.22 and leave most slots free for
/// the linear probe. 1024 also leaves room for a table to grow without a re-tune; at
/// [`Bucket`]'s eight bytes the whole probe array is 8 KiB, so it fits an L1 data cache
/// on its own and leaves room for the identity array beside it.
const BUCKETS: usize = 1024;

/// A `(module, function, arity)` key as the hook tables spell it.
type Key3 = (&'static str, &'static str, u32);

/// One roster entry: a seam's bit and the table of keys that read that seam.
type RosterEntry = (u16, &'static [Key3]);

/// The fourteen `(bit, table)` pairs — the flag table's **only** input.
///
/// The order and length mirror [`ALL`]; [`FlagTable::build`] destructures this array
/// exhaustively, so a table added, removed or replaced without touching the roster is
/// a compile error rather than a silently missing bit.
const ROSTER: [RosterEntry; 14] = [
    (MEMORY, MEMORY_HOOK_BIFS),
    (LOCAL_FUN_IDENTITY, LOCAL_FUN_IDENTITY_BIFS),
    (DICT, DICT_HOOK_BIFS),
    (PROCESS_HOOK, PROCESS_HOOK_BIFS),
    (PORT_HOOK, PORT_HOOK_BIFS),
    (SEND_HOOK, SEND_HOOK_BIFS),
    (NIF_LOADER, NIF_LOADER_BIFS),
    (EXPORT_GATE, EXPORT_GATE_BIFS),
    (RECORD_SOURCE, RECORD_SOURCE_BIFS),
    (MODULE_INFO, MODULE_INFO_BIFS),
    (PERSISTENT_TERM, PERSISTENT_TERM_BIFS),
    (FUN_CALLER, FUN_CALLER_BIFS),
    (LOCAL_NODE, LOCAL_NODE_BIFS),
    (RUNTIME_HOOKS, RUNTIME_HOOKS_BIFS),
];

/// The roster's raw key count (226: 222 distinct, four listed by two tables).
const ROSTER_KEYS: usize = {
    let mut keys = 0;
    let mut i = 0;
    while i < ROSTER.len() {
        keys += ROSTER[i].1.len();
        i += 1;
    }
    keys
};

/// The roster fits the bucket array, which is what guarantees that a probe sequence
/// ([`FlagTable::lookup`] and [`FlagTable::insert`]) always reaches an empty bucket
/// instead of walking the array forever.
const _: () = assert!(ROSTER_KEYS <= BUCKETS);

/// The roster's bits equal [`ALL_MASK`]: every named bit is carried by exactly one
/// roster entry (the pairwise-distinctness of [`ALL`] turns "the fold matches" into
/// "each bit appears once"), so no table can lose its bit.
const _: () = {
    let mut mask = 0;
    let mut i = 0;
    while i < ROSTER.len() {
        mask |= ROSTER[i].0;
        i += 1;
    }
    assert!(mask == ALL_MASK);
};

/// The arity salt: folded in with one multiply so that `f/1`, `f/2` and `f/3` — which
/// share both names and lengths — cannot land on one fingerprint.
const ARITY_SALT: u64 = 0x9e37_79b9_7f4a_7c15;

/// The avalanche multiplier (`murmur3`'s `fmix64` constant): the one multiply that
/// spreads the mixed accumulator's high bits into the low bits the slot mask reads.
const AVALANCHE: u64 = 0xff51_afd7_ed55_8ccd;

/// Up to the first eight bytes of a name as a little-endian word, zero padded — one
/// length-guarded load, no `unsafe`.
#[inline]
fn head_word(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    let n = bytes.len().min(8);
    word[..n].copy_from_slice(&bytes[..n]);
    u64::from_le_bytes(word)
}

/// Up to the last eight bytes of a name as a little-endian word, zero padded.
///
/// A name shorter than the window is covered by [`head_word`] alone and contributes
/// nothing here, which is the second of the fingerprint's two length regimes.
#[inline]
fn tail_word(bytes: &[u8]) -> u64 {
    match bytes.len().checked_sub(8) {
        Some(start) => head_word(&bytes[start..]),
        None => 0,
    }
}

/// The final avalanche: two xor-shifts around a multiply, so every accumulator bit
/// reaches the low bits the slot mask reads.
#[inline]
fn avalanche(mut mixed: u64) -> u64 {
    mixed ^= mixed >> 33;
    mixed = mixed.wrapping_mul(AVALANCHE);
    mixed ^= mixed >> 29;
    mixed
}

/// The fingerprint of a key: the two names' lengths, the arity, and the leading and
/// trailing eight-byte window of each name — at most four `u64` loads and two
/// multiplies, however long the names are.
///
/// The fingerprint is a **slot selector only**; [`Key::matches`] verifies the full key
/// before any flags are trusted, so a shared fingerprint costs a probe, never a wrong
/// answer.
#[inline]
fn fingerprint(module: &str, function: &str, arity: u32) -> u64 {
    let module = module.as_bytes();
    let function = function.as_bytes();
    // The lengths first: a name that is a prefix of another must not borrow its
    // window words' meaning.
    let mut mixed = ((module.len() as u64) << 32) | function.len() as u64;
    mixed ^= u64::from(arity).wrapping_mul(ARITY_SALT);
    mixed ^= head_word(module);
    mixed ^= tail_word(module).rotate_left(43);
    mixed ^= head_word(function).rotate_left(29);
    mixed ^= tail_word(function).rotate_left(11);
    avalanche(mixed)
}

/// The value of [`Bucket::key`] on a free slot.
///
/// `u16::MAX` is never a key index — the identity array holds at most [`ROSTER_KEYS`]
/// entries, and the assertion below turns that bound into "every index fits a `u16`
/// and [`EMPTY_KEY`] always means free".
const EMPTY_KEY: u16 = u16::MAX;

/// Every key index fits [`EMPTY_KEY`], so a bucket can hold the index of any key and
/// the sentinel stays distinguishable from all of them.
const _: () = assert!(ROSTER_KEYS < EMPTY_KEY as usize);

/// One probe slot: a fingerprint tag, the seam flags, and the index of the full key.
///
/// Eight bytes, so a cache line holds eight slots and the whole 1024-slot probe array
/// is 8 KiB — the layout choice that keeps a lookup's first (and, at load factor 0.22,
/// usually only) touch inside one L1 line.
#[derive(Clone, Copy)]
struct Bucket {
    /// The fingerprint's high half: a pre-filter, so a slot that is not ours costs one
    /// `u32` compare instead of a trip to the identity array.
    tag: u32,
    /// The OR of every bit whose table lists the key at [`Self::key`].
    flags: u16,
    /// The index of the key's identity in [`FlagTable::keys`], or [`EMPTY_KEY`].
    key: u16,
}

/// A free probe slot — the shape a table under construction leaves where it filled
/// nothing, and the shape that ends a probe sequence.
const FREE: Bucket = Bucket {
    tag: 0,
    flags: 0,
    key: EMPTY_KEY,
};

impl Bucket {
    /// Whether this slot is free (which ends a probe sequence).
    #[inline]
    fn is_free(&self) -> bool {
        self.key == EMPTY_KEY
    }
}

/// One distinct key's identity: the `(module, function, arity)` a lookup verifies in
/// full before it trusts any [`Bucket::flags`].
#[derive(Clone, Copy)]
struct Key {
    /// The key's module name (a `&'static str` straight from the table).
    module: &'static str,
    /// The key's function name (a `&'static str` straight from the table).
    function: &'static str,
    /// The key's arity.
    arity: u32,
}

impl Key {
    /// Whether this identity *is* the queried key — the full-key comparison that makes
    /// a shared fingerprint (or a shared slot) harmless.
    #[inline]
    fn matches(&self, module: &str, function: &str, arity: u32) -> bool {
        // The arity first: it is the one-word test that rejects most near misses.
        self.arity == arity && self.module == module && self.function == function
    }
}

/// The build-once open-addressed flag table ([`BUCKETS`] slots, linear probing).
struct FlagTable {
    /// The probe array: `[Bucket; BUCKETS]`, [`FREE`] where no key hashes there.
    buckets: Box<[Bucket]>,
    /// The identity array, one entry per distinct key, indexed by [`Bucket::key`].
    keys: Box<[Key]>,
}

impl FlagTable {
    /// Build the table from [`ROSTER`] and nothing else.
    ///
    /// The roster is destructured **exhaustively** so that a table added to (or
    /// removed from) [`ROSTER`] fails to compile here: the pattern's length is the
    /// array's length.
    fn build() -> FlagTable {
        let [
            (_memory_hook, _),
            (_local_fun_identity, _),
            (_dict_hook, _),
            (_process_hook, _),
            (_port_hook, _),
            (_send_hook, _),
            (_nif_loader, _),
            (_export_gate, _),
            (_record_source, _),
            (_module_info, _),
            (_persistent_term, _),
            (_fun_caller, _),
            (_local_node, _),
            (_runtime_hooks, _),
        ] = ROSTER;

        FlagTable::build_from(&ROSTER)
    }

    /// Build a table from an arbitrary roster — [`ROSTER`] in production, a hand-made
    /// one in the collision tests. The one build path, so a test exercises exactly the
    /// code a lookup runs in production.
    fn build_from(roster: &[RosterEntry]) -> FlagTable {
        let mut buckets = vec![FREE; BUCKETS].into_boxed_slice();
        let mut keys = Vec::with_capacity(ROSTER_KEYS);
        for &(bit, table) in roster {
            for &(module, function, arity) in table {
                insert(&mut buckets, &mut keys, bit, module, function, arity);
            }
        }
        FlagTable {
            buckets,
            keys: keys.into_boxed_slice(),
        }
    }

    /// The seam flags of `(module, function, arity)`, or `0` when no table lists it.
    ///
    /// `#[inline]` so the fourteen `needs_*` predicates carry the probe in their own
    /// body: no call, prologue or spills per predicate. The body is a few loads, two
    /// multiplies and (on a hit) one `u16` bit-set read.
    #[inline]
    fn lookup(&self, module: &str, function: &str, arity: u32) -> u16 {
        let hash = fingerprint(module, function, arity);
        let tag = fingerprint_tag(hash);
        let mut slot = bucket(hash);
        loop {
            let probe = self.buckets[slot];
            if probe.is_free() {
                return 0;
            }
            if probe.tag == tag
                && self.keys[usize::from(probe.key)].matches(module, function, arity)
            {
                return probe.flags;
            }
            // A shared tag or slot (or a probe from one): costs a probe, never a wrong
            // answer, because the next slot's key is compared in full.
            slot = next_bucket(slot);
        }
    }
}

/// OR `bit` into the entry for `(module, function, arity)` of a table under
/// construction, appending the key's identity to `keys` when it is new.
fn insert(
    buckets: &mut [Bucket],
    keys: &mut Vec<Key>,
    bit: u16,
    module: &'static str,
    function: &'static str,
    arity: u32,
) {
    let hash = fingerprint(module, function, arity);
    let tag = fingerprint_tag(hash);
    let mut slot = bucket(hash);
    loop {
        let probe = buckets[slot];
        if probe.is_free() {
            // The index fits a `u16`: `ROSTER_KEYS < EMPTY_KEY` is asserted at compile
            // time, so no key can be appended past the sentinel's range.
            let key = keys.len() as u16;
            keys.push(Key {
                module,
                function,
                arity,
            });
            buckets[slot] = Bucket {
                tag,
                flags: bit,
                key,
            };
            return;
        }
        if keys[usize::from(probe.key)].matches(module, function, arity) {
            buckets[slot].flags |= bit;
            return;
        }
        // Another key occupies this slot: continue the probe. The array always has a
        // free slot left (see `ROSTER_KEYS`).
        slot = next_bucket(slot);
    }
}

/// The slot a fingerprint selects (`BUCKETS` is a power of two, so the mask suffices).
#[inline]
fn bucket(hash: u64) -> usize {
    (hash as usize) & (BUCKETS - 1)
}

/// The tag a slot keeps of its key's fingerprint: the high half, which the slot mask
/// does not read, so the tag and the slot select on disjoint bits.
#[inline]
fn fingerprint_tag(hash: u64) -> u32 {
    (hash >> 32) as u32
}

/// The next slot in a probe sequence: linear probing with wrap-around.
#[inline]
fn next_bucket(slot: usize) -> usize {
    (slot + 1) & (BUCKETS - 1)
}

/// The flag table, built on first use (its construction is the only cost this module
/// ever pays beyond a probe).
static TABLE: OnceLock<FlagTable> = OnceLock::new();

/// The seam flags of `(module, function, arity)`: bit `n` set means the key is listed
/// by the roster entry whose bit is `1 << n` (see [`MEMORY`] … [`RUNTIME_HOOKS`]).
///
/// This is **the single membership engine**: every `needs_*` predicate in
/// [`hook_sets`](crate::hook_sets) is `flags(module, function, arity) & FLAG != 0`, so
/// a host that asks all fourteen questions about one key pays one fingerprint and one
/// probe instead of fourteen table walks. A key no table lists answers `0`, which keeps
/// all fourteen predicates `false` — exactly what their `binary_search`es answered.
#[inline]
pub fn flags(module: &str, function: &str, arity: u32) -> u16 {
    TABLE
        .get_or_init(FlagTable::build)
        .lookup(module, function, arity)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::registry::default_registry;

    /// The fourteen `(bit, table, name)` triples as the tests walk them — the
    /// test-side mirror of [`ROSTER`], named so a failure says which seam it is.
    const TABLES: &[(u16, &str, &[Key3])] = &[
        (MEMORY, "MEMORY_HOOK_BIFS", MEMORY_HOOK_BIFS),
        (
            LOCAL_FUN_IDENTITY,
            "LOCAL_FUN_IDENTITY_BIFS",
            LOCAL_FUN_IDENTITY_BIFS,
        ),
        (DICT, "DICT_HOOK_BIFS", DICT_HOOK_BIFS),
        (PROCESS_HOOK, "PROCESS_HOOK_BIFS", PROCESS_HOOK_BIFS),
        (PORT_HOOK, "PORT_HOOK_BIFS", PORT_HOOK_BIFS),
        (SEND_HOOK, "SEND_HOOK_BIFS", SEND_HOOK_BIFS),
        (NIF_LOADER, "NIF_LOADER_BIFS", NIF_LOADER_BIFS),
        (EXPORT_GATE, "EXPORT_GATE_BIFS", EXPORT_GATE_BIFS),
        (RECORD_SOURCE, "RECORD_SOURCE_BIFS", RECORD_SOURCE_BIFS),
        (MODULE_INFO, "MODULE_INFO_BIFS", MODULE_INFO_BIFS),
        (PERSISTENT_TERM, "PERSISTENT_TERM_BIFS", PERSISTENT_TERM_BIFS),
        (FUN_CALLER, "FUN_CALLER_BIFS", FUN_CALLER_BIFS),
        (LOCAL_NODE, "LOCAL_NODE_BIFS", LOCAL_NODE_BIFS),
        (RUNTIME_HOOKS, "RUNTIME_HOOKS_BIFS", RUNTIME_HOOKS_BIFS),
    ];

    /// The reference semantics: a linear scan of the table, i.e. what the predicate
    /// answered before the flag table existed.
    fn listed(table: &[(&str, &str, u32)], module: &str, function: &str, arity: u32) -> bool {
        table
            .iter()
            .any(|&(m, f, a)| m == module && f == function && a == arity)
    }

    /// Every table key, deduplicated, with the bits of the tables that list it (four
    /// keys are listed by two tables, so their expected mask has two bits).
    fn table_keys() -> std::collections::HashMap<(&'static str, &'static str, u32), u16> {
        let mut keys: std::collections::HashMap<(&'static str, &'static str, u32), u16> =
            std::collections::HashMap::new();
        for (bit, _, table) in TABLES {
            for &key in *table {
                *keys.entry(key).or_insert(0) |= bit;
            }
        }
        keys
    }

    /// The keys the equivalence sweep covers: every key of the default registry, every
    /// one of those at `arity + 1`, and names that are table keys in every respect but
    /// one byte (or none at all).
    fn sweep() -> Vec<(String, String, u32)> {
        let registry = default_registry();
        let registered = registry.names();
        assert_eq!(registered.len(), registry.len());
        let mut keys = registered.clone();
        keys.extend(
            registered
                .iter()
                .map(|(module, function, arity)| (module.clone(), function.clone(), arity + 1)),
        );
        for (module, function) in [
            ("erlang", "no_such_bif_xyz"),
            ("nosuchmod", "f"),
            ("erlan", "memory"),
            ("erlang", "memoryy"),
            ("erlang", "Memory"),
            ("Erlang", "memory"),
            ("", "memory"),
            ("erlang", ""),
        ] {
            for arity in [0, 1, 2, 3] {
                keys.push((module.to_string(), function.to_string(), arity));
            }
        }
        for arity in [0, 1, u32::MAX] {
            keys.push((String::new(), String::new(), arity));
        }
        keys
    }

    /// **The gate.** For every swept key, each of the fourteen bits equals a linear
    /// scan of that bit's table — so the flag table is a faithful replacement for the
    /// `binary_search`es it replaced, on present *and* absent keys, and any hash,
    /// collision or build bug answers differently here.
    #[test]
    fn the_flags_agree_with_a_linear_scan_of_every_table() {
        let keys = sweep();
        for (module, function, arity) in &keys {
            let actual = flags(module, function, *arity);
            let mut expected_mask = 0;
            for (bit, name, table) in TABLES {
                let expected = listed(table, module, function, *arity);
                let got = actual & bit != 0;
                assert_eq!(
                    got, expected,
                    "{name} disagrees with a linear scan on {module}:{function}/{arity}"
                );
                if expected {
                    expected_mask |= bit;
                }
            }
            assert_eq!(
                actual, expected_mask,
                "the flag word of {module}:{function}/{arity} carries a bit no table justifies"
            );
        }
        // The sweep really does cover the whole registered surface plus its near
        // misses — not just the tables.
        assert!(
            keys.len() > default_registry().len(),
            "the sweep must cover the registry and its absent-key variants"
        );
    }

    /// Every key listed by any table carries **exactly** the bits of the tables that
    /// list it — all fourteen for a key in no table (zero), one bit for the ordinary
    /// case, and two for the four external-term encoders `LOCAL_FUN_IDENTITY_BIFS` and
    /// `LOCAL_NODE_BIFS` share.
    #[test]
    fn every_table_key_carries_exactly_the_bits_of_the_tables_that_list_it() {
        let keys = table_keys();
        let shared = keys.values().filter(|&&bits| bits.count_ones() == 2).count();
        assert_eq!(
            shared, 4,
            "four keys (erlang:term_to_binary/1,2 and external_size/1,2) are listed by two tables"
        );
        for ((module, function, arity), expected) in &keys {
            assert_ne!(*expected, 0, "{module}:{function}/{arity} is listed by no table");
            assert_eq!(
                flags(module, function, *arity),
                *expected,
                "{module}:{function}/{arity} carries the wrong bit set"
            );
        }
        // The probe sequences terminate because the distinct key count fits the
        // bucket array (`ROSTER_KEYS <= BUCKETS`, asserted at compile time).
        assert!(
            keys.len() <= BUCKETS,
            "the distinct key count must fit the bucket array or a probe never ends"
        );
    }

    /// A key no table lists answers **zero**, which is what keeps all fourteen
    /// predicates `false` for it — the same answer the fourteen `binary_search`es gave.
    #[test]
    fn an_unlisted_key_has_no_bits_at_all() {
        for (module, function, arity) in [
            ("erlang", "no_such_bif_xyz", 0),
            ("nosuchmod", "f", 1),
            ("", "", 0),
            ("erlang", "memory", 2),
            ("erlang", "term_to_binary", 0),
            ("erts_internal", "port_close", 2),
        ] {
            assert_eq!(
                flags(module, function, arity),
                0,
                "{module}:{function}/{arity} is in no table, so it must carry no bit"
            );
        }
    }

    /// The fourteen bits are pairwise distinct and fill `0..14`, and the roster covers
    /// each of them — the invariants the compile-time assertions rely on, pinned where
    /// a reader can see them fail.
    #[test]
    fn the_fourteen_bits_are_distinct_and_fill_the_low_fourteen() {
        assert_eq!(ALL.len(), 14);
        assert_eq!(ROSTER.len(), ALL.len());
        assert_eq!(ALL_MASK, 0x3FFF);
        let mut seen = 0u16;
        for bit in ALL {
            assert_eq!(bit.count_ones(), 1, "{bit:#x} is not a single bit");
            assert_eq!(seen & bit, 0, "{bit:#x} aliases an earlier bit");
            seen |= bit;
        }
        assert_eq!(seen, ALL_MASK);
        for (i, (bit, _)) in ROSTER.iter().enumerate() {
            assert_eq!(*bit, ALL[i], "roster entry {i} carries the wrong bit");
        }
    }

    /// The table is built once and answers identically on every later call, and two
    /// keys that differ in one name byte never share an answer.
    #[test]
    fn the_table_is_stable_and_distinguishes_one_byte_neighbours() {
        let first = flags("erlang", "term_to_binary", 1);
        assert_eq!(first, LOCAL_FUN_IDENTITY | LOCAL_NODE);
        assert_eq!(first, flags("erlang", "term_to_binary", 1));
        assert_ne!(flags("erlang", "term_to_binarx", 1), first);
        assert_eq!(
            flags("erlang", "term_to_binary", 2),
            LOCAL_FUN_IDENTITY | LOCAL_NODE
        );
        assert_ne!(
            flags("erlang", "term_to_binary", 3),
            LOCAL_FUN_IDENTITY | LOCAL_NODE
        );
    }

    /// The fingerprint has two length regimes, and a key of *each* probes to exactly
    /// the bits its tables justify: a name shorter than the eight-byte window
    /// contributes its length plus a zero-padded head word and no tail word, while a
    /// longer name contributes a real tail window.
    #[test]
    fn a_short_name_and_a_long_name_both_probe_correctly() {
        // The raw words first: under eight bytes there is no tail word at all.
        assert_eq!(head_word(b""), 0);
        assert_eq!(tail_word(b""), 0);
        assert_eq!(head_word(b"ets"), u64::from_le_bytes(*b"ets\0\0\0\0\0"));
        assert_eq!(
            tail_word(b"ets"),
            0,
            "a name under eight bytes contributes no tail word"
        );
        assert_eq!(head_word(b"abcdefgh"), u64::from_le_bytes(*b"abcdefgh"));
        assert_eq!(tail_word(b"abcdefgh"), u64::from_le_bytes(*b"abcdefgh"));
        assert_eq!(
            head_word(b"erts_internal"),
            u64::from_le_bytes(*b"erts_int")
        );
        assert_eq!(
            tail_word(b"erts_internal"),
            u64::from_le_bytes(*b"internal"),
            "the tail window is the name's last eight bytes"
        );

        // Both regimes reach the table: the short key (3- and 5-byte names)…
        assert_eq!(flags("ets", "foldl", 3), FUN_CALLER, "short-name regime");
        // …and the long one (13- and 9-byte names), whose windows differ.
        assert_eq!(
            flags("erts_internal", "port_info", 2),
            PORT_HOOK,
            "long-name regime"
        );
        // Neither regime answers for a name that is not in any table, including one
        // that differs only in the *last* byte of a long name (which the tail window
        // sees) and one truncated to exactly the head window.
        assert_eq!(flags("erts_internal", "port_infox", 2), 0);
        assert_eq!(flags("erts_interna", "port_info", 2), 0);
        assert_eq!(flags("ets", "folds", 3), 0);
    }

    /// Two keys that share a fingerprint — same tag, same starting slot — still answer
    /// their own bits, and a third key sharing that fingerprint but listed by no table
    /// still answers `0`: the walk verifies each full key, so a collision costs a probe.
    #[test]
    fn a_deliberate_fingerprint_collision_still_answers_correctly() {
        // Seventeen bytes each: the same length, the same first eight bytes and the
        // same last eight bytes, differing only in the middle — invisible to the two
        // bounded windows, so the fingerprints coincide by construction.
        const FIRST: Key3 = ("abcdefghMabcdefgh", "f", 2);
        const SECOND: Key3 = ("abcdefghXabcdefgh", "f", 2);
        const LISTED_BY_NEITHER: Key3 = ("abcdefghZabcdefgh", "f", 2);
        const FIRST_ENTRY: RosterEntry = (MEMORY, &[FIRST]);
        const SECOND_ENTRY: RosterEntry = (DICT, &[SECOND]);

        let first = fingerprint(FIRST.0, FIRST.1, FIRST.2);
        let second = fingerprint(SECOND.0, SECOND.1, SECOND.2);
        assert_eq!(first, second, "the collision must be real for this test");
        assert_eq!(bucket(first), bucket(second), "…so both start at one slot");
        assert_eq!(fingerprint_tag(first), fingerprint_tag(second));

        let table = FlagTable::build_from(&[FIRST_ENTRY, SECOND_ENTRY]);
        assert_eq!(table.keys.len(), 2, "both keys were inserted");
        // The second insert probed past the first rather than overwriting it.
        let start = bucket(first);
        assert_eq!(table.buckets[start].key, 0);
        assert_eq!(table.buckets[next_bucket(start)].key, 1);

        assert_eq!(table.lookup(FIRST.0, FIRST.1, FIRST.2), MEMORY);
        assert_eq!(table.lookup(SECOND.0, SECOND.1, SECOND.2), DICT);
        assert_eq!(
            table.lookup(
                LISTED_BY_NEITHER.0,
                LISTED_BY_NEITHER.1,
                LISTED_BY_NEITHER.2
            ),
            0,
            "a shared fingerprint is not a match: the walk reaches a free slot"
        );
        assert_eq!(
            table.lookup(FIRST.0, FIRST.1, 3),
            0,
            "the arity still separates"
        );
        // The production table lists neither name.
        assert_eq!(flags(FIRST.0, FIRST.1, FIRST.2), 0);
    }

    /// The probe array is the L1-sized one the module doc claims — eight bytes per
    /// slot, 8 KiB in all — and every occupied slot indexes the identity array.
    #[test]
    fn the_probe_array_is_eight_kilobytes_and_its_indices_are_in_range() {
        assert_eq!(core::mem::size_of::<Bucket>(), 8);
        assert_eq!(BUCKETS * core::mem::size_of::<Bucket>(), 8 * 1024);
        let table = TABLE.get_or_init(FlagTable::build);
        assert_eq!(table.buckets.len(), BUCKETS);
        assert_eq!(table.keys.len(), table_keys().len());
        assert!(table.keys.len() < usize::from(EMPTY_KEY));
        let mut occupied = 0;
        for (slot, probe) in table.buckets.iter().enumerate() {
            if probe.is_free() {
                continue;
            }
            occupied += 1;
            assert!(
                usize::from(probe.key) < table.keys.len(),
                "slot {slot} indexes past the identity array"
            );
        }
        assert_eq!(
            occupied,
            table.keys.len(),
            "one slot per distinct key, no duplicate entries"
        );
    }
}
