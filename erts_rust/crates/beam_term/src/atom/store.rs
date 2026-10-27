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
//! * **name → index** (which index does `"ok"` have?) — needs a hash map, but a *single* map is
//!   the wrong shape for a copy-on-write table: one append would have to clone the entire bucket
//!   array. [`IndexShards`] splits it into [`INDEX_SHARDS`] independent maps, so an append copies
//!   one shard (about `len / INDEX_SHARDS` entries) instead of the whole index.
//!
//! Both directions sit behind one `Arc` each, which is what makes [`super::AtomTable::fork`] an
//! O(1) operation that allocates **nothing**: it clones two `Arc`s. Every mutating path goes
//! through `Arc::make_mut`, so the first append after a fork materializes at most one chunk and
//! one shard — O(1) in the number of newly interned names, never O(table).
//!
//! Neither half can mutate a shared container in place (`Arc::make_mut` clones instead), which is
//! exactly the isolation the two sides of a fork are promised: a name interned after the fork is
//! invisible to the other side, and both land on the same index.

use std::collections::HashMap;
use std::sync::Arc;

use super::Atom;

/// Names per name-storage chunk (`pub` so the allocation pins can derive their budget).
pub const NAMES_PER_CHUNK: usize = 128;

/// Hash shards in the name→index direction (`pub`, same reason). MUST be a power of two.
pub const INDEX_SHARDS: usize = 128;

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

/// One hash shard of the name→index direction.
///
/// `Clone` is required (not incidental): [`IndexShards::insert`] detaches a shared shard with
/// `Arc::make_mut`, which clones the map instead of mutating one a fork can still see.
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexShard {
    map: HashMap<Arc<str>, Atom>,
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
    pub(crate) fn get(&self, name: &str) -> Option<Atom> {
        let shards = self.shards.as_ref()?;
        shards[shard_of(name)].as_ref()?.map.get(name).copied()
    }

    /// Record `name → atom` in its shard — the **only** mutating path.
    pub(crate) fn insert(&mut self, name: Arc<str>, atom: Atom) {
        let slot = shard_of(&name);
        let shards = Arc::make_mut(
            self.shards
                .get_or_insert_with(|| Arc::new(std::array::from_fn(|_| None))),
        );
        let shard =
            Arc::make_mut(shards[slot].get_or_insert_with(|| Arc::new(IndexShard::default())));
        shard.map.insert(name, atom);
    }

    /// The total number of name→index entries (Σ over the shards).
    ///
    /// O([`INDEX_SHARDS`]) by construction — used only by the debug invariant that the index holds
    /// exactly one entry per interned name, never on the interning hot path.
    pub(crate) fn entries(&self) -> usize {
        match self.shards.as_deref() {
            None => 0,
            Some(shards) => shards.iter().flatten().map(|shard| shard.map.len()).sum(),
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

/// The shard a `name` lives in: FNV-1a over the UTF-8 bytes, then the splitmix64 finalizer (an
/// avalanche, so short names with a shared prefix still spread out), masked to the shard count.
///
/// It is a **pure function of the name bytes** — no seed, no table state, no allocation — which is
/// what lets a fork and its parent (and any number of generations of forks) agree on a name's
/// shard without coordinating.
fn shard_of(name: &str) -> usize {
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
    (h as usize) & (INDEX_SHARDS - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arc(name: &str) -> Arc<str> {
        Arc::from(name)
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
