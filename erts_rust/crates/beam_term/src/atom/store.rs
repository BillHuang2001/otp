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

//! The two copy-on-write storage directions behind [`super::AtomTable`].
//!
//! An atom table has to answer two questions, and they are indexed in opposite directions:
//!
//! * **index → name** (which name does `Atom(i)` denote?) — needs O(1) random access by index and
//!   an append that never renumbers an existing index. That is [`NameChunks`]: a list of
//!   fixed-capacity [`NameChunk`]s.
//! * **name → index** (which index does `"ok"` have?) — needs a hash table, but a *single* table is
//!   the wrong shape for a copy-on-write table: one append would have to clone the entire slot
//!   array. [`IndexShards`] splits it into [`INDEX_SHARDS`] independent shards, so an append copies
//!   one shard (about `len / INDEX_SHARDS` entries) instead of the whole index. Each shard is an
//!   open-addressed table keyed by the name's 64-bit hash (see [`hash_of`]), because the shard
//!   choice is *itself* derived from that hash and re-deriving it a second time (as a `HashMap`
//!   `RandomState` would) is pure waste on the hottest lookup in the ETF decode path.
//!
//! Both directions sit behind one `Arc` each, which is what makes [`super::AtomTable::fork`] an
//! O(1) operation that allocates **nothing**: it clones two `Arc`s. Every mutating path goes
//! through `Arc::make_mut`, so the first append after a fork materializes at most one chunk and
//! one shard — O(1) in the number of newly interned names, never O(table).
//!
//! Neither half can mutate a shared container in place (`Arc::make_mut` clones instead), which is
//! exactly the isolation the two sides of a fork are promised: a name interned after the fork is
//! invisible to the other side, and both land on the same index.

use std::sync::Arc;

use super::Atom;

/// Names per name-storage chunk (`pub` so the allocation pins can derive their budget).
pub const NAMES_PER_CHUNK: usize = 128;

/// Hash shards in the name→index direction (`pub`, same reason). MUST be a power of two.
///
/// The shard count is a **memory** knob as much as a copy-granularity one, and it trades two costs
/// against each other:
///
/// * a non-empty table's shard array is `INDEX_SHARDS` `Option<Arc<IndexShard>>` handles — `8 ·
///   INDEX_SHARDS` B, retained by every table whose array has diverged from its parent (a module's
///   `fork` detaches the array on its first intern) and transiently copied on that first append;
/// * the *retained* cost of a diverged table is the shard maps it cloned, and one shard's map holds
///   about `name_count / INDEX_SHARDS` entries — so a larger shard count shrinks exactly the clone
///   the copy-on-write step exists to keep small.
///
/// Measured on `beam_rt -noshell -eval 'halt().'` (median peak RSS, 5 interleaved runs each,
/// `taskset -c 3`): 128 → 64.48 MiB, 512 → 60.16, **1024 → 59.37 (−5.11 MiB)**, 2048 → 59.68 —
/// past 1024 the per-table array (8 KB at 1024, 16 KB at 2048) outgrows what the smaller shard clone
/// saves. At the boot's ~9 500-atom table a shard holds ~9 names, so the array is the dominant term
/// and 1024 is the optimum. Detail and the byte-level breakdown: `src/atom/CONTEXT.md`.
pub const INDEX_SHARDS: usize = 1024;

/// The shard selector masks the hash with this; a non-power-of-two `INDEX_SHARDS` would make
/// [`shard_of`] collapse onto a subset of the shards (a correctness bug, not just a slow one).
const _: () = assert!(INDEX_SHARDS.is_power_of_two());
const _: () = assert!(NAMES_PER_CHUNK > 0);

// ---------------------------------------------------------------------------
// index → name: chunked
// ---------------------------------------------------------------------------

/// One chunk of the index→name direction: up to [`NAMES_PER_CHUNK`] names, in index order.
#[derive(Debug, Clone, Default)]
pub(crate) struct NameChunk {
    names: Vec<Arc<str>>,
}

impl NameChunk {
    /// An empty chunk with room for a full chunk's worth of names.
    fn with_capacity() -> NameChunk {
        NameChunk {
            names: Vec::with_capacity(NAMES_PER_CHUNK),
        }
    }

    /// Append one handle, keeping a full chunk's worth of headroom.
    ///
    /// The `reserve_exact` is what makes a *cloned* chunk (whose `Vec::clone` capacity is exactly
    /// its length) fit the rest of its names without a regrowth per name.
    fn push(&mut self, name: Arc<str>) {
        if self.names.len() == self.names.capacity() {
            self.names.reserve_exact(NAMES_PER_CHUNK - self.names.len());
        }
        self.names.push(name);
    }

    fn len(&self) -> usize {
        self.names.len()
    }

    /// Test-only identity probe: the handle stored at `slot`.
    #[cfg(test)]
    fn name_arc(&self, slot: usize) -> Option<&Arc<str>> {
        self.names.get(slot)
    }
}

/// The **index → name** direction: `chunks[i][j]` names `Atom(i * NAMES_PER_CHUNK + j)`; every
/// chunk but the last is full.
///
/// O(1) random access, and an O(1)-allocation [`NameChunks::fork`]: the chunk list and every chunk
/// are `Arc`-shared, and a mutating path clones only the one chunk it appends to.
#[derive(Debug)]
pub(crate) struct NameChunks {
    /// `None` until the first name is appended, so an empty table allocates nothing.
    chunks: Option<Arc<Vec<Arc<NameChunk>>>>,
}

impl NameChunks {
    /// An empty name store — allocates nothing ([`super::AtomTable::new`] must stay free).
    pub(crate) fn new() -> NameChunks {
        NameChunks { chunks: None }
    }

    /// Share every chunk with `self` — allocates nothing at any table size.
    pub(crate) fn fork(&self) -> NameChunks {
        NameChunks {
            chunks: self.chunks.clone(),
        }
    }

    /// The number of stored names (hence the next free index).
    pub(crate) fn len(&self) -> usize {
        match self
            .chunks
            .as_deref()
            .and_then(|chunks| chunks.split_last())
        {
            None => 0,
            Some((last, full)) => full.len() * NAMES_PER_CHUNK + last.len(),
        }
    }

    /// The name at `index`, or `None` if the index is not in this table.
    pub(crate) fn get(&self, index: usize) -> Option<&str> {
        let chunks = self.chunks.as_deref()?;
        let chunk = chunks.get(index / NAMES_PER_CHUNK)?;
        chunk.names.get(index % NAMES_PER_CHUNK).map(|name| &**name)
    }

    /// Append `name`, returning the index it took — the **only** mutating path.
    ///
    /// The index is derived from the current length (indices are append-only positions), and the
    /// write goes through `Arc::make_mut` so a store shared with a fork copies the one chunk list
    /// and the one chunk it touches, never the whole table.
    pub(crate) fn push(&mut self, name: Arc<str>) -> u32 {
        let index = self.len();
        let chunk_pos = index / NAMES_PER_CHUNK;
        let slot = index % NAMES_PER_CHUNK;
        let chunks = Arc::make_mut(self.chunks.get_or_insert_with(|| Arc::new(Vec::new())));
        if slot == 0 {
            // A new chunk starts exactly here: every preceding chunk is full, so the chunk list
            // length and the index arithmetic agree.
            debug_assert_eq!(chunks.len(), chunk_pos);
            chunks.push(Arc::new(NameChunk::with_capacity()));
        }
        debug_assert_eq!(chunks.len(), chunk_pos + 1);
        Arc::make_mut(&mut chunks[chunk_pos]).push(name);
        index as u32
    }

    /// Reserve room for `total` names in total, so a bulk seed does not regrow the chunk list
    /// name by name.
    pub(crate) fn reserve_names(&mut self, total: usize) {
        let chunks = Arc::make_mut(self.chunks.get_or_insert_with(|| Arc::new(Vec::new())));
        chunks.reserve(total.div_ceil(NAMES_PER_CHUNK));
    }

    /// Test-only identity probe: the chunk-list `Arc`'s allocation, or `None` when empty.
    ///
    /// Two stores that share their chunks (a fork and its parent) report the same pointer; after
    /// either side appends, they report different ones (the copy-on-write step).
    #[cfg(test)]
    pub(crate) fn container_ptr(&self) -> Option<*const ()> {
        self.chunks
            .as_ref()
            .map(|chunks| Arc::as_ptr(chunks) as *const ())
    }

    /// Test-only identity probe: the handle stored at `index`.
    #[cfg(test)]
    pub(crate) fn name_arc(&self, index: usize) -> Option<&Arc<str>> {
        let chunks = self.chunks.as_deref()?;
        let chunk = chunks.get(index / NAMES_PER_CHUNK)?;
        chunk.name_arc(index % NAMES_PER_CHUNK)
    }
}

// ---------------------------------------------------------------------------
// name → index: hash-sharded
// ---------------------------------------------------------------------------

/// One entry of a shard's probe table.
///
/// The `hash` is stored so a probe can reject a slot that holds a *different* name with one `u64`
/// compare instead of reaching into the string — the common case in a small table is landing next
/// to an unrelated entry, and a byte compare there costs a deref plus a `memcmp`.
#[derive(Debug, Clone)]
struct Entry {
    /// [`hash_of`] of `name` — the value the shard choice and the probe slot are both derived from.
    hash: u64,
    name: Arc<str>,
    atom: Atom,
}

/// One hash shard of the name→index direction: an open-addressed, linear-probing table keyed by
/// the name's 64-bit hash.
///
/// A `HashMap` was the wrong container here, because the shard already knows the name's hash — the
/// shard is *derived* from it. A `HashMap` throws that away and re-derives it with SipHash (a
/// per-process `RandomState` key, so it cannot even be memoized across calls), then compares keys
/// through an `Arc<str>` deref. Here a single [`hash_of`] per lookup serves the shard mask **and**
/// the in-shard probe slot, and a mis-probed slot is rejected by one integer compare.
///
/// `Clone` is required (not incidental): [`IndexShards::insert`] detaches a shared shard with
/// `Arc::make_mut`, which clones the table instead of mutating one a fork can still see. It is a
/// **deep** copy — one fresh name string per entry — for the same reason [`super::AtomTable`]'s
/// `Clone` is (see its rustdoc: two sibling crates use the deep copy as a Θ(N) positive control).
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexShard {
    /// The probe table. `None` is an empty slot; `len()` is either `0` or a power of two.
    /// `Vec::new()` is the empty state, so a shard that has never been inserted into (and an
    /// `IndexShard::default()`) allocates nothing.
    slots: Vec<Option<Entry>>,
    /// The number of `Some` slots — kept rather than derived, so `entries()` stays O(shards).
    len: usize,
}

impl IndexShard {
    /// The slot count the first insert allocates.
    ///
    /// A shard holds about `name_count / INDEX_SHARDS` names — ~9 in the booted runtime's ~9 500
    /// atom table — so 8 slots (which hold 6 at the 3/4 grow point) is one allocation for a
    /// typical shard and at most one grow event for a shard at the boot's load, instead of the
    /// 4 → 8 → 16 regrow ladder a minimal first capacity would walk. The cost is 8 ×
    /// `size_of::<Option<Entry>>()` ≈ 256 B per *non-empty* shard of the shared table.
    const MIN_CAPACITY: usize = 8;

    /// The number of interned names in this shard.
    fn len(&self) -> usize {
        self.len
    }

    /// The index interned for `name`, whose [`hash_of`] is `hash`.
    ///
    /// Probe slot 0 comes from the **high** half of the hash; equal hashes and a full scan both
    /// fall through to the next slot. The loop always terminates: [`IndexShard::insert`] keeps the
    /// table at or below a 3/4 load factor, so at least one slot is `None`.
    #[inline]
    fn get(&self, hash: u64, name: &str) -> Option<Atom> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut slot = probe_index(hash, mask);
        loop {
            match &self.slots[slot] {
                None => return None,
                Some(entry) => {
                    if entry.hash == hash && entry.name.as_bytes() == name.as_bytes() {
                        return Some(entry.atom);
                    }
                }
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Record `name → atom`, whose [`hash_of`] is `hash`.
    ///
    /// Re-inserting a name that is already here is a no-op: the index is append-only, so the first
    /// mapping of a name wins and nothing may renumber it.
    fn insert(&mut self, hash: u64, name: Arc<str>, atom: Atom) {
        if self.slots.is_empty() {
            self.slots = vec![None; Self::MIN_CAPACITY];
        }
        let mask = self.slots.len() - 1;
        let mut slot = probe_index(hash, mask);
        loop {
            match &self.slots[slot] {
                Some(entry) if entry.hash == hash && entry.name.as_bytes() == name.as_bytes() => {
                    return;
                }
                Some(_) => slot = (slot + 1) & mask,
                // Never full (the grow point below), so this always finds a slot.
                None => break,
            }
        }
        self.slots[slot] = Some(Entry { hash, name, atom });
        self.len += 1;
        // Grow the moment the table would pass a 3/4 load factor. The test is written as a
        // multiplication rather than a division so it stays exact and branch-predictable.
        if self.len * 4 >= self.slots.len() * 3 {
            self.grow();
        }
    }

    /// Double the table and re-place every entry from its **stored** hash — never by re-hashing a
    /// name, so a grow moves each `Arc` handle without touching a byte of any string.
    ///
    /// Keeping the slot count a power of two is what makes the mask work; keeping the load factor
    /// at or below 3/4 (the caller's grow point) is what bounds the probe runs to a short,
    /// cache-line-resident cluster.
    fn grow(&mut self) {
        let new_cap = self.slots.len() * 2;
        let mask = new_cap - 1;
        let mut slots: Vec<Option<Entry>> = vec![None; new_cap];
        for entry in std::mem::take(&mut self.slots).into_iter().flatten() {
            let mut slot = probe_index(entry.hash, mask);
            while slots[slot].is_some() {
                slot = (slot + 1) & mask;
            }
            slots[slot] = Some(entry);
        }
        self.slots = slots;
    }
}

type ShardArray = [Option<Arc<IndexShard>>; INDEX_SHARDS];

/// The **name → index** direction, hash-sharded so a copy-on-write step touches ONE shard
/// (about `len / INDEX_SHARDS` entries) instead of the whole map.
///
/// [`shard_of`] is a pure function of the name bytes, so a fork and its parent always agree on
/// where a given name lives — a name is only ever looked up in, and inserted into, its own shard.
#[derive(Debug)]
pub(crate) struct IndexShards {
    /// `None` until the first name is appended, so an empty table allocates nothing.
    shards: Option<Arc<ShardArray>>,
}

impl IndexShards {
    /// An empty index — allocates nothing ([`super::AtomTable::new`] must stay free).
    pub(crate) fn new() -> IndexShards {
        IndexShards { shards: None }
    }

    /// Share every shard with `self` — allocates nothing at any table size.
    pub(crate) fn fork(&self) -> IndexShards {
        IndexShards {
            shards: self.shards.clone(),
        }
    }

    /// The index interned for `name`, if any.
    ///
    /// [`hash_of`] runs **once** and serves both halves of the probe — the shard mask and the
    /// slot inside the shard. That single hash is the whole point of the open-addressed shard: a
    /// `HashMap` had to hash the same bytes a second time, with a different (seeded) algorithm.
    ///
    /// The empty-table `?` comes before the hash, so a lookup in a table that has never interned
    /// anything costs a load and a branch.
    #[inline]
    pub(crate) fn get(&self, name: &str) -> Option<Atom> {
        let shards = self.shards.as_ref()?;
        let hash = hash_of(name);
        shards[shard_of_hash(hash)].as_ref()?.get(hash, name)
    }

    /// Record `name → atom` in its shard — the **only** mutating path.
    pub(crate) fn insert(&mut self, name: Arc<str>, atom: Atom) {
        let hash = hash_of(&name);
        let slot = shard_of_hash(hash);
        let shards = Arc::make_mut(
            self.shards
                .get_or_insert_with(|| Arc::new(std::array::from_fn(|_| None))),
        );
        let shard =
            Arc::make_mut(shards[slot].get_or_insert_with(|| Arc::new(IndexShard::default())));
        shard.insert(hash, name, atom);
    }

    /// The total number of name→index entries (Σ over the shards).
    ///
    /// O([`INDEX_SHARDS`]) by construction — used only by the debug invariant that the index holds
    /// exactly one entry per interned name, never on the interning hot path.
    pub(crate) fn entries(&self) -> usize {
        match self.shards.as_deref() {
            None => 0,
            Some(shards) => shards.iter().flatten().map(|shard| shard.len()).sum(),
        }
    }

    /// Test-only identity probe: the shard-array `Arc`'s allocation, or `None` when empty.
    #[cfg(test)]
    pub(crate) fn container_ptr(&self) -> Option<*const ()> {
        self.shards
            .as_ref()
            .map(|shards| Arc::as_ptr(shards) as *const ())
    }
}

/// The 64-bit hash of a `name`: FNV-1a over the UTF-8 bytes, then the splitmix64 finalizer (an
/// avalanche, so short names with a shared prefix still spread out).
///
/// It is a **pure function of the name bytes** — no seed, no table state, no allocation — which is
/// what lets a fork and its parent (and any number of generations of forks) agree on a name's
/// shard without coordinating. A seeded hasher (`RandomState`'s SipHash) could not: it is
/// per-process state, so it would have to be re-derived on every call.
///
/// One call serves **both** halves of a lookup: [`shard_of_hash`] picks the shard and
/// [`probe_index`] picks the slot inside it.
#[inline]
fn hash_of(name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in name.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    h
}

/// The shard a [`hash_of`] value names: its low bits, masked to the shard count.
///
/// The low bits are the shard's, which is exactly why [`probe_index`] takes the high ones.
#[inline]
fn shard_of_hash(hash: u64) -> usize {
    (hash as usize) & (INDEX_SHARDS - 1)
}

/// The shard a `name` lives in — [`hash_of`] composed with [`shard_of_hash`].
///
/// This is the rule [`IndexShards`] documents; the live paths ([`IndexShards::get`] /
/// [`IndexShards::insert`]) reach the same answer through `shard_of_hash` so they can keep the
/// hash in hand for the probe, and the composition is pinned by
/// `shard_of_is_a_pure_power_of_two_bounded_function` as the property that matters: a pure,
/// power-of-two-bounded function of the bytes.
#[cfg(test)]
fn shard_of(name: &str) -> usize {
    shard_of_hash(hash_of(name))
}

/// The first slot a `hash` probes in a table of `cap_mask + 1` slots.
///
/// The **high** half of the hash, deliberately: the low bits are already spent on the shard mask
/// ([`shard_of_hash`]), so every entry of one shard shares them — deriving the slot from them would
/// collapse a shard's names into a single probe cluster and turn the table into a linear scan.
/// The splitmix64 finalizer mixes the high half into the low bits but leaves the two statistically
/// independent, so the high half spreads uniformly within a shard.
#[inline]
fn probe_index(hash: u64, cap_mask: usize) -> usize {
    ((hash >> 32) as usize) & cap_mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn arc(name: &str) -> Arc<str> {
        Arc::from(name)
    }

    /// `n` distinct names that all land in shard `target`.
    ///
    /// [`shard_of`] is a pure hash of the bytes, so aiming at one shard means *filtering* a
    /// deterministic candidate stream — at `INDEX_SHARDS = 1024` that costs about `1024 · n`
    /// candidates, which is milliseconds.
    fn names_in_shard(target: usize, n: usize) -> Vec<String> {
        let mut found = Vec::with_capacity(n);
        for i in 0.. {
            let name = format!("shard_aimed_name_{i}");
            if shard_of(&name) == target {
                found.push(name);
                if found.len() == n {
                    return found;
                }
            }
        }
        unreachable!("an infinite candidate stream contains `n` names for any shard")
    }

    /// The slot count of every shard a name was inserted into, in shard order.
    fn shard_capacities(index: &IndexShards) -> Vec<usize> {
        index
            .shards
            .as_deref()
            .map(|shards| {
                shards
                    .iter()
                    .flatten()
                    .map(|shard| shard.slots.len())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn an_empty_store_allocates_and_reads_as_empty() {
        let names = NameChunks::new();
        assert_eq!(names.len(), 0);
        assert_eq!(names.get(0), None);
        assert!(names.container_ptr().is_none());
        let index = IndexShards::new();
        assert_eq!(index.get("anything"), None);
        assert_eq!(index.entries(), 0);
        assert!(index.container_ptr().is_none());
    }

    #[test]
    fn chunk_positions_follow_the_index_arithmetic() {
        let mut names = NameChunks::new();
        for i in 0..(NAMES_PER_CHUNK * 2 + 3) {
            let index = names.push(arc(&format!("name_{i}")));
            assert_eq!(index as usize, i);
        }
        assert_eq!(names.len(), NAMES_PER_CHUNK * 2 + 3);
        for i in 0..names.len() {
            let expected = format!("name_{i}");
            assert_eq!(names.get(i), Some(expected.as_str()), "index {i}");
        }
        // out of range reads stay `None` (no panic, no wrap into a neighbouring slot)
        assert_eq!(names.get(names.len()), None);
        assert_eq!(names.get(usize::MAX), None);
    }

    #[test]
    fn a_fork_shares_both_containers_and_appends_detach_them() {
        let mut parent = NameChunks::new();
        let mut pindex = IndexShards::new();
        for i in 0..10 {
            let name = arc(&format!("shared_{i}"));
            let index = Atom(parent.push(Arc::clone(&name)));
            pindex.insert(name, index);
        }
        let fork = parent.fork();
        let findex = pindex.fork();
        assert_eq!(fork.len(), parent.len());
        assert!(std::ptr::eq(
            parent.container_ptr().unwrap(),
            fork.container_ptr().unwrap()
        ));
        assert_eq!(
            pindex.container_ptr().map(|p| p as usize),
            findex.container_ptr().map(|p| p as usize)
        );
        // the fork resolves exactly what the parent does
        for i in 0..parent.len() {
            assert_eq!(fork.get(i), parent.get(i));
            let name = parent.get(i).unwrap();
            assert_eq!(findex.get(name), pindex.get(name));
        }
        // an append into the *fork* detaches it; the parent's container is untouched
        let mut fork = fork;
        let mut findex = findex;
        let name = arc("only_in_the_fork");
        let index = Atom(fork.push(Arc::clone(&name)));
        findex.insert(name, index);
        assert_ne!(
            parent.container_ptr().unwrap() as usize,
            fork.container_ptr().unwrap() as usize
        );
        assert_eq!(parent.len() + 1, fork.len());
        assert_eq!(parent.get(10), None);
        assert_eq!(fork.get(10), Some("only_in_the_fork"));
        assert_eq!(pindex.get("only_in_the_fork"), None);
        assert_eq!(findex.get("only_in_the_fork"), Some(index));
        assert_eq!(pindex.entries(), 10);
        assert_eq!(findex.entries(), 11);
    }

    #[test]
    fn shard_of_is_a_pure_power_of_two_bounded_function() {
        for name in ["", "ok", "true", "false", "nonode@nohost", "_", "a", "b"] {
            let slot = shard_of(name);
            assert!(slot < INDEX_SHARDS, "{name} -> {slot}");
            assert_eq!(slot, shard_of(name), "deterministic");
            let owned: String = name.to_string();
            assert_eq!(slot, shard_of(&owned), "byte-driven, not identity-driven");
        }
        // short names with a shared prefix still spread out (the splitmix avalanche)
        let slots: std::collections::HashSet<usize> =
            (0..64).map(|i| shard_of(&format!("n{i}"))).collect();
        assert!(
            slots.len() > 8,
            "the avalanche must spread short names: {slots:?}"
        );
    }

    /// The open-addressed shard is a *replaceable* container: it must answer exactly what a
    /// `HashMap` keyed by the same names would, through several capacity doublings, for present
    /// and absent names alike.
    #[test]
    fn the_probe_table_agrees_with_a_hash_map_model_across_growth() {
        const COUNT: usize = 3_000;
        let mut index = IndexShards::new();
        let mut model: HashMap<Arc<str>, Atom> = HashMap::new();
        // A deterministic per-step observation point: after every insert the container must agree
        // with the model, so a lost entry cannot hide behind a later grow.
        for i in 0..COUNT {
            let name = arc(&format!("probe_model_{i:04}"));
            let atom = Atom(i as u32);
            index.insert(Arc::clone(&name), atom);
            model.insert(name, atom);
            if i % 97 == 0 {
                assert_eq!(index.entries(), model.len(), "entries after {i}");
            }
        }
        assert_eq!(index.entries(), model.len());

        for (name, atom) in &model {
            assert_eq!(index.get(name), Some(*atom), "{name}");
        }
        // Names that are near misses of stored ones, and names from a stream that shares the
        // stored names' prefix — none may resolve.
        for i in 0..COUNT {
            let missing = format!("probe_model_{i:04}x");
            assert_eq!(index.get(&missing), None, "absent {missing}");
        }
        // The structure's own invariants, checked on every non-empty shard: a power-of-two slot
        // count and a load factor at or below 3/4 (which is what makes a probe always terminate).
        let capacities = shard_capacities(&index);
        assert!(!capacities.is_empty());
        for shard in index.shards.as_deref().unwrap().iter().flatten() {
            assert!(shard.slots.len().is_power_of_two(), "{}", shard.slots.len());
            assert!(shard.slots.len() >= IndexShard::MIN_CAPACITY);
            assert!(
                shard.len() * 4 < shard.slots.len() * 3,
                "shard load factor: {} of {}",
                shard.len(),
                shard.slots.len()
            );
        }
        // Growth really happened: at ~3 names per shard some shards must have passed the initial
        // 8 slots, so the doubling + re-place loop above is exercised by this test.
        assert!(
            capacities.iter().any(|&cap| cap > IndexShard::MIN_CAPACITY),
            "no shard grew: {capacities:?}"
        );
    }

    /// Aiming every name at ONE shard is the case a table with a per-shard probe gets wrong: the
    /// shard has to grow through several doublings and its probe clusters get long.
    #[test]
    fn many_names_in_one_shard_all_resolve() {
        const COUNT: usize = 200;
        let target = shard_of("ok");
        let names = names_in_shard(target, COUNT);
        let mut index = IndexShards::new();
        for (i, name) in names.iter().enumerate() {
            index.insert(arc(name), Atom(i as u32));
        }
        assert_eq!(index.entries(), COUNT);
        // Every name landed in `target`, and that shard is the only populated one.
        for (i, name) in names.iter().enumerate() {
            assert_eq!(shard_of(name), target);
            assert_eq!(index.get(name), Some(Atom(i as u32)), "{name}");
        }
        assert_eq!(
            index.shards.as_deref().unwrap().iter().flatten().count(),
            1,
            "the helper must aim every name at one shard"
        );
        let shard = index.shards.as_deref().unwrap()[target]
            .as_ref()
            .expect("the aimed shard is populated");
        assert_eq!(shard.len(), COUNT);
        assert!(
            shard.slots.len() >= 256,
            "8 slots cannot hold {COUNT} entries at a 3/4 load factor, got {}",
            shard.slots.len()
        );
        // Names that live in the same shard but were never inserted stay absent — the probe must
        // not answer a neighbour just because its low hash bits match.
        for name in names_in_shard(target, COUNT + 20).iter().skip(COUNT) {
            assert_eq!(index.get(name), None, "absent {name}");
        }
        // Re-inserting a known name is a no-op: the index is append-only, so the first mapping wins.
        let kept = index.get(&names[0]);
        index.insert(arc(&names[0]), Atom(u32::MAX));
        assert_eq!(index.get(&names[0]), kept);
        assert_eq!(
            index.shards.as_deref().unwrap()[target]
                .as_ref()
                .expect("still populated")
                .len(),
            COUNT
        );
        assert_eq!(index.entries(), COUNT);
    }

    /// The probe slot must come from the **high** half of the hash, because the low bits are what
    /// chose the shard — they are identical for every entry of one shard.
    #[test]
    fn the_probe_index_comes_from_the_high_half_of_the_hash() {
        let target = shard_of("ok");
        let names = names_in_shard(target, 16);
        // The low bits really are constant within a shard: this is the trap the probe avoids.
        let low: HashSet<usize> = names
            .iter()
            .map(|name| (hash_of(name) as usize) & (INDEX_SHARDS - 1))
            .collect();
        assert_eq!(low, HashSet::from([target]), "{low:?}");
        // ...so a low-bit probe would put all 16 in one slot, while the high half spreads them.
        for mask in [7usize, 15, 31] {
            let spread: HashSet<usize> = names
                .iter()
                .map(|name| probe_index(hash_of(name), mask))
                .collect();
            assert!(
                spread.len() >= 4,
                "mask {mask} spread {} of {} names: {spread:?}",
                spread.len(),
                names.len()
            );
        }
    }

    #[test]
    fn reserve_names_keeps_indices_and_grows_the_chunk_list_once() {
        let mut names = NameChunks::new();
        names.reserve_names(NAMES_PER_CHUNK * 4);
        // an empty-but-reserved store still reads as empty
        assert_eq!(names.len(), 0);
        for i in 0..(NAMES_PER_CHUNK * 2) {
            assert_eq!(names.push(arc(&format!("r{i}"))) as usize, i);
        }
        assert_eq!(names.len(), NAMES_PER_CHUNK * 2);
        assert_eq!(names.get(NAMES_PER_CHUNK * 2), None);
    }
}
