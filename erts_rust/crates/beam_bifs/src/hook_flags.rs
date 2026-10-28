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
//! (OR-ing, so a key listed by two tables gets both bits). [`BUCKETS`] open-addressed
//! buckets (linear probing) hold a `u64` content hash plus the full key plus the
//! `u16` flags; the tables hold 226 raw keys (222 distinct), so the load factor is at
//! most 0.22 and a probe sequence almost always ends at its first bucket.
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
//! # Collisions
//!
//! A hash collision costs **a probe, never a wrong answer**: a lookup compares the
//! full `(module, function, arity)` of every occupied bucket it walks, so two keys
//! that share a bucket stay distinct and a key that is not in the table is answered
//! `0` when the walk reaches an empty bucket. The hash only chooses where the walk
//! starts.
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

/// The bucket count: a power of two, and `>= 512` so that the 226 raw keys of the
/// fourteen tables (222 distinct) load it to at most 0.22 and leave most buckets
/// empty for the linear probe. 1024 also leaves room for a table to grow without a
/// re-tune; memory is one 48-byte bucket per slot, allocated once.
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

/// The 64-bit FNV-1a offset basis and prime (the same fold the host's dispatch memo
/// uses, `beam_rt`'s `dispatch_slot`).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The avalanche multiplier applied after the fold, and the final xor-shift's width.
///
/// The fold leaves the arity in the low bits and mixes the high bits only weakly,
/// while the bucket index is read from the low bits; the xor-shift folds the high
/// bits down and the multiply avalanches what is left, so the mask selects a bucket
/// that depends on every name byte.
const MIX: u64 = 0xa5d2_b6a8_e9c1_9f9f;
const MIX_SHIFT: u32 = 32;

/// The content hash of a key: FNV-1a over `module`, a `0xFF` separator (so
/// `("ab", "c")` cannot fold like `("a", "bc")`), `function`, then the arity — plus
/// the final avalanche described at [`MIX`].
///
/// The hash is a **bucket selector only**; [`Key::matches`] verifies the full key.
fn hash_key(module: &str, function: &str, arity: u32) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in module
        .bytes()
        .chain(core::iter::once(u8::MAX))
        .chain(function.bytes())
    {
        hash = (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
    hash = (hash ^ u64::from(arity)).wrapping_mul(FNV_PRIME);
    (hash ^ (hash >> MIX_SHIFT)).wrapping_mul(MIX)
}

/// One occupied bucket: the full key plus the seam flags it carries.
#[derive(Clone, Copy)]
struct Key {
    /// The key's module name (a `&'static str` straight from the table).
    module: &'static str,
    /// The key's function name (a `&'static str` straight from the table).
    function: &'static str,
    /// The key's arity.
    arity: u32,
    /// The OR of every bit whose table lists this key.
    flags: u16,
}

impl Key {
    /// Whether this bucket holds exactly the queried key — the full-key comparison a
    /// lookup makes before trusting [`Self::flags`].
    #[inline]
    fn matches(&self, module: &str, function: &str, arity: u32) -> bool {
        // The arity first: it is the one-word test that rejects most near misses.
        self.arity == arity && self.module == module && self.function == function
    }
}

/// The build-once open-addressed flag table ([`BUCKETS`] buckets, linear probing).
struct FlagTable {
    /// The buckets; `None` is an empty slot and ends any probe sequence.
    buckets: Box<[Option<Key>]>,
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

        let mut table = FlagTable {
            buckets: vec![None; BUCKETS].into_boxed_slice(),
        };
        for (bit, keys) in ROSTER {
            for &(module, function, arity) in keys {
                table.insert(bit, module, function, arity);
            }
        }
        table
    }

    /// OR `bit` into the entry for `(module, function, arity)`, creating it if absent.
    fn insert(&mut self, bit: u16, module: &'static str, function: &'static str, arity: u32) {
        let mut slot = bucket(hash_key(module, function, arity));
        loop {
            match &mut self.buckets[slot] {
                None => {
                    self.buckets[slot] = Some(Key {
                        module,
                        function,
                        arity,
                        flags: bit,
                    });
                    return;
                }
                Some(key) if key.matches(module, function, arity) => {
                    key.flags |= bit;
                    return;
                }
                // An unrelated key occupies this bucket: continue the probe. The
                // array always has an empty bucket left (see `ROSTER_KEYS`).
                Some(_) => slot = next_bucket(slot),
            }
        }
    }

    /// The seam flags of `(module, function, arity)`, or `0` when no table lists it.
    fn lookup(&self, module: &str, function: &str, arity: u32) -> u16 {
        let mut slot = bucket(hash_key(module, function, arity));
        loop {
            match &self.buckets[slot] {
                None => return 0,
                Some(key) if key.matches(module, function, arity) => return key.flags,
                // A hash collision (or a probe from one): costs a probe, never a
                // wrong answer, because the next bucket's key is compared in full.
                Some(_) => slot = next_bucket(slot),
            }
        }
    }
}

/// The bucket a hash selects (`BUCKETS` is a power of two, so the mask suffices).
#[inline]
fn bucket(hash: u64) -> usize {
    (hash as usize) & (BUCKETS - 1)
}

/// The next bucket in a probe sequence: linear probing with wrap-around.
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
/// a host that asks all fourteen questions about one key pays one hash and one probe
/// instead of fourteen table walks. A key no table lists answers `0`, which keeps all
/// fourteen predicates `false` — exactly what their `binary_search`es answered.
#[inline]
pub(crate) fn flags(module: &str, function: &str, arity: u32) -> u16 {
    TABLE.get_or_init(FlagTable::build).lookup(module, function, arity)
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
}
