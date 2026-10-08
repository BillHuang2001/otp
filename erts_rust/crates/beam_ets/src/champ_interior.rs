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

//! CHAMP (compressed hash-array mapped prefix-tree) open-addressing interior for
//! `set`.
//!
//! A HAMT variant: every tree level consumes [`WIDTH`] bits of the 64-bit
//! `beam_term::hashing::term_hash` of the key and each node stores its children
//! **compressed** into two bitmaps — `datamap` for inline `(key, object)` slots
//! and `nodemap` for sub-nodes — so a node with few children occupies only a few
//! words (no per-level 32-entry array). Because `set` keeps at most one object per
//! key, a slot is a single key→object pair (a genuine *open-addressing* layout for
//! the common depths), unlike the chained [`HashInterior`](crate::hash_interior)
//! that stores a `Vec` of objects per key.
//!
//! The interior shape is **not** externally pinned (workspace `CONTEXT.md` §2);
//! only the `set` **semantics** are contract and they are identical to the chained
//! interior's: one object per key (last insert wins), `size` counts objects,
//! `insert_new` reports whether the key was absent, `update_counter`/
//! `update_element` mutate the stored object, and lookup/iteration return the
//! stored objects. Navigation order is unspecified for `set` (like the chained
//! interior), so the `Interior` trait defaults answer `first`/`last`/`next`/`prev`
//! over this interior's depth-first [`keys`](Interior::keys) order.
//!
//! Public for testing and reuse, but **not** the cross-crate seam: dependents go
//! through [`crate::Table`]. Nothing here re-implements term order or hashing.
//!
//! ## Hash exhaustion
//!
//! Two distinct keys always have distinct 64-bit hashes *unless* `term_hash`
//! collides. Keys whose full hash is equal are kept in a [`CollisionNode`] (the
//! standard CHAMP fallback), so the recursion never consumes more than the 64
//! hash bits; [`index_bit`] additionally guards a shift past the hash width so a
//! pathological hash can never panic.

use core::mem::size_of;

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::key_of;
use crate::table::Interior;

/// Bits of the hash consumed per tree level (32-way branching).
const WIDTH: u32 = 5;
/// The low-[`WIDTH`]-bit mask.
const MASK: u32 = (1 << WIDTH) - 1;

/// One stored entry: the key, its object, and the key's full 64-bit hash (kept so
/// a node can tell a genuine full-hash collision from a mere index collision
/// without recomputing the hash).
#[derive(Clone, Copy)]
struct Slot {
    hash: u64,
    key: Eterm,
    object: Eterm,
}

/// A bitmap node: `datamap` marks the occupied inline slots, `nodemap` the
/// occupied sub-node slots. `data` and `nodes` hold **only** the set bits, in
/// ascending bit order (the CHAMP compression): the slot for a set bit is found
/// by counting the set bits below it.
struct BitmapNode {
    datamap: u32,
    nodemap: u32,
    data: Vec<Slot>,
    nodes: Vec<Box<Node>>,
}

impl BitmapNode {
    fn empty() -> BitmapNode {
        BitmapNode {
            datamap: 0,
            nodemap: 0,
            data: Vec::new(),
            nodes: Vec::new(),
        }
    }
}

/// A collision node: entries that share the same full 64-bit hash. (Genuine
/// 64-bit collisions of `term_hash` are vanishingly rare; the node exists so the
/// interior stays correct when they happen.)
struct CollisionNode {
    entries: Vec<Slot>,
}

/// A CHAMP node.
enum Node {
    Bitmap(BitmapNode),
    Collision(CollisionNode),
}

/// The result of an insert/upsert.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Upsert {
    /// A new key was added.
    Inserted,
    /// An existing key's object was replaced.
    Replaced,
}

/// The bitmap bit selecting `hash`'s slot at `shift`: `1 << (WIDTH bits)`.
///
/// A shift past the hash width yields bit 0 (never reached with distinct hashes —
/// see the module docs); the guard keeps the shift a defined operation.
fn index_bit(hash: u64, shift: u32) -> u32 {
    let bits = if shift >= 64 {
        0
    } else {
        (hash >> shift) as u32 & MASK
    };
    1u32 << bits
}

/// The compressed slot index of `bit` within `bitmap`: the number of set bits
/// strictly below it. `bitmap` must have `bit` set (or `bit == 1`).
fn slot_index(bitmap: u32, bit: u32) -> usize {
    (bitmap & (bit - 1)).count_ones() as usize
}

/// The `index`-th (0-based) set bit of `bitmap`. `index` must be in range.
fn bit_at(bitmap: u32, index: usize) -> u32 {
    let mut bits = bitmap;
    for _ in 0..index {
        bits &= !(bits & bits.wrapping_neg());
    }
    bits & bits.wrapping_neg()
}

/// Whether a node holds no entry at all (the parent then drops its slot).
fn node_is_empty(node: &Node) -> bool {
    match node {
        Node::Collision(col) => col.entries.is_empty(),
        Node::Bitmap(bm) => bm.datamap == 0 && bm.nodemap == 0,
    }
}

/// The single entry a node holds, if it holds exactly one.
fn single_entry(node: &Node) -> Option<Slot> {
    match node {
        Node::Collision(col) if col.entries.len() == 1 => Some(col.entries[0]),
        Node::Bitmap(bm) if bm.nodemap == 0 && bm.data.len() == 1 => Some(bm.data[0]),
        _ => None,
    }
}

/// Restore the CHAMP canonical form: a sub-node that holds a single entry is
/// **inlined** into its parent as an inline slot (which never changes routing —
/// all of the sub-node's entries share the parent's index bit, so the entry's
/// parent-level index is exactly the bit it moves to). Applied bottom-up after a
/// removal, so a chain of pass-through nodes collapses level by level.
///
/// Promoting a *multi-entry* sub-node wholesale would be wrong: its children are
/// indexed by the next 5 hash bits, so a promoted node would be probed with the
/// wrong bits. Only single-entry inlining preserves the routing invariant.
fn canonicalize(node: &mut Node) {
    if let Node::Bitmap(bm) = node {
        let mut i = 0;
        while i < bm.nodes.len() {
            match single_entry(&bm.nodes[i]) {
                Some(slot) => {
                    let bit = bit_at(bm.nodemap, i);
                    bm.nodes.remove(i);
                    bm.nodemap &= !bit;
                    let di = slot_index(bm.datamap, bit);
                    bm.data.insert(di, slot);
                    bm.datamap |= bit;
                    // The child that shifted into position `i` is re-examined.
                }
                None => i += 1,
            }
        }
    }
}

/// Insert/upsert `(key, object)` into `node`.
///
/// No canonicalization is needed: an insert only ever *adds* entries, so no node
/// is reduced to a single sub-node child (the split path puts two entries into the
/// new sub-node).
fn node_insert(
    node: &mut Node,
    hash: u64,
    shift: u32,
    key: Eterm,
    object: Eterm,
    atoms: &AtomTable,
) -> Upsert {
    match node {
        Node::Collision(col) => match col.entries.iter_mut().find(|s| eq(s.key, key, atoms)) {
            Some(slot) => {
                slot.object = object;
                Upsert::Replaced
            }
            None => {
                col.entries.push(Slot { hash, key, object });
                Upsert::Inserted
            }
        },
        Node::Bitmap(bm) => bitmap_insert(bm, hash, shift, key, object, atoms),
    }
}

/// The bitmap-node half of [`node_insert`].
fn bitmap_insert(
    bm: &mut BitmapNode,
    hash: u64,
    shift: u32,
    key: Eterm,
    object: Eterm,
    atoms: &AtomTable,
) -> Upsert {
    let bit = index_bit(hash, shift);
    if bm.datamap & bit != 0 {
        let i = slot_index(bm.datamap, bit);
        let same_hash = bm.data[i].hash == hash;
        if same_hash && eq(bm.data[i].key, key, atoms) {
            bm.data[i].object = object;
            return Upsert::Replaced;
        }
        // The inline slot must become a sub-node holding both entries: the old one
        // and the new one. A shared full hash (or an exhausted hash) becomes a
        // collision node; otherwise the two descend one level.
        let old = bm.data.remove(i);
        bm.datamap &= !bit;
        let child = if same_hash || shift + WIDTH >= 64 {
            Node::Collision(CollisionNode {
                entries: vec![old, Slot { hash, key, object }],
            })
        } else {
            let mut child = Node::Bitmap(BitmapNode::empty());
            node_insert(&mut child, old.hash, shift + WIDTH, old.key, old.object, atoms);
            node_insert(&mut child, hash, shift + WIDTH, key, object, atoms);
            child
        };
        let ni = slot_index(bm.nodemap, bit);
        bm.nodes.insert(ni, Box::new(child));
        bm.nodemap |= bit;
        Upsert::Inserted
    } else if bm.nodemap & bit != 0 {
        let ni = slot_index(bm.nodemap, bit);
        node_insert(&mut bm.nodes[ni], hash, shift + WIDTH, key, object, atoms)
    } else {
        let i = slot_index(bm.datamap, bit);
        bm.data.insert(i, Slot { hash, key, object });
        bm.datamap |= bit;
        Upsert::Inserted
    }
}

/// Look `key` up in `node`, returning its object.
fn node_lookup(node: &Node, hash: u64, shift: u32, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
    match node {
        Node::Collision(col) => col
            .entries
            .iter()
            .find(|s| eq(s.key, key, atoms))
            .map(|s| s.object),
        Node::Bitmap(bm) => {
            let bit = index_bit(hash, shift);
            if bm.datamap & bit != 0 {
                let slot = &bm.data[slot_index(bm.datamap, bit)];
                if eq(slot.key, key, atoms) {
                    Some(slot.object)
                } else {
                    None
                }
            } else if bm.nodemap & bit != 0 {
                node_lookup(
                    &bm.nodes[slot_index(bm.nodemap, bit)],
                    hash,
                    shift + WIDTH,
                    key,
                    atoms,
                )
            } else {
                None
            }
        }
    }
}

/// Remove `key` from `node` (canonicalizing `node` on success), returning the
/// removed object.
fn node_remove(
    node: &mut Node,
    hash: u64,
    shift: u32,
    key: Eterm,
    atoms: &AtomTable,
) -> Option<Eterm> {
    let removed = match node {
        Node::Collision(col) => match col.entries.iter().position(|s| eq(s.key, key, atoms)) {
            Some(i) => Some(col.entries.remove(i).object),
            None => None,
        },
        Node::Bitmap(bm) => {
            let bit = index_bit(hash, shift);
            if bm.datamap & bit != 0 {
                let i = slot_index(bm.datamap, bit);
                if eq(bm.data[i].key, key, atoms) {
                    let object = bm.data.remove(i).object;
                    bm.datamap &= !bit;
                    Some(object)
                } else {
                    None
                }
            } else if bm.nodemap & bit != 0 {
                let ni = slot_index(bm.nodemap, bit);
                let removed = node_remove(&mut bm.nodes[ni], hash, shift + WIDTH, key, atoms);
                if removed.is_some() && node_is_empty(&bm.nodes[ni]) {
                    bm.nodes.remove(ni);
                    bm.nodemap &= !bit;
                }
                removed
            } else {
                None
            }
        }
    };
    if removed.is_some() {
        canonicalize(node);
    }
    removed
}

/// Depth-first walk of `node`, emitting every slot in a deterministic order
/// (ascending bit position at each level).
fn collect_into(node: &Node, out: &mut Vec<Slot>) {
    match node {
        Node::Collision(col) => out.extend_from_slice(&col.entries),
        Node::Bitmap(bm) => {
            let mut bits = bm.datamap | bm.nodemap;
            while bits != 0 {
                let bit = bits & bits.wrapping_neg();
                bits &= !bit;
                if bm.datamap & bit != 0 {
                    out.push(bm.data[slot_index(bm.datamap, bit)]);
                } else {
                    collect_into(&bm.nodes[slot_index(bm.nodemap, bit)], out);
                }
            }
        }
    }
}

/// The heap words a node owns (its own footprint plus every child's).
fn heap_words(node: &Node) -> usize {
    match node {
        Node::Collision(col) => col.entries.capacity() * words_of::<Slot>(),
        Node::Bitmap(bm) => {
            bm.data.capacity() * words_of::<Slot>()
                + bm.nodes.capacity() * words_of::<Box<Node>>()
                + bm.nodes
                    .iter()
                    .map(|child| words_of::<Node>() + heap_words(child))
                    .sum::<usize>()
        }
    }
}

/// The number of `Eterm`-sized words a value of type `T` occupies (rounded up).
fn words_of<T>() -> usize {
    size_of::<T>().div_ceil(size_of::<Eterm>())
}

/// The CHAMP open-addressing interior backing a `set` table.
pub struct ChampInterior {
    /// The tree root (an empty bitmap for an empty table).
    root: Node,
    /// The number of stored objects (== keys, `set` holds one object per key).
    len: usize,
}

impl ChampInterior {
    /// A new, empty interior.
    pub fn new() -> ChampInterior {
        ChampInterior {
            root: Node::Bitmap(BitmapNode::empty()),
            len: 0,
        }
    }

    /// Every stored slot in depth-first order.
    fn collect(&self) -> Vec<Slot> {
        let mut out = Vec::with_capacity(self.len);
        collect_into(&self.root, &mut out);
        out
    }
}

impl Default for ChampInterior {
    fn default() -> ChampInterior {
        ChampInterior::new()
    }
}

impl Interior for ChampInterior {
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let hash = term_hash(key, atoms);
        match node_insert(&mut self.root, hash, 0, key, object, atoms) {
            Upsert::Inserted => self.len += 1,
            Upsert::Replaced => {}
        }
        // A `set` always keeps the last object written for a key.
        Ok(true)
    }

    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let hash = term_hash(key, atoms);
        if node_lookup(&self.root, hash, 0, key, atoms).is_some() {
            return Ok(false);
        }
        match node_insert(&mut self.root, hash, 0, key, object, atoms) {
            Upsert::Inserted => {
                self.len += 1;
                Ok(true)
            }
            // Unreachable: the probe above proved the key absent. Answer `false`
            // rather than a wrong "inserted" if it ever happened.
            Upsert::Replaced => Ok(false),
        }
    }

    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        let hash = term_hash(key, atoms);
        if node_remove(&mut self.root, hash, 0, key, atoms).is_some() {
            self.len -= 1;
        }
        Ok(())
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let hash = term_hash(key, atoms);
        if let Some(stored) = node_lookup(&self.root, hash, 0, key, atoms) {
            if eq(stored, object, atoms)
                && node_remove(&mut self.root, hash, 0, key, atoms).is_some()
            {
                self.len -= 1;
            }
        }
        Ok(())
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        let hash = term_hash(key, atoms);
        match node_lookup(&self.root, hash, 0, key, atoms) {
            Some(object) => vec![object],
            None => Vec::new(),
        }
    }

    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        let hash = term_hash(key, atoms);
        node_lookup(&self.root, hash, 0, key, atoms).is_some()
    }

    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        let hash = term_hash(key, atoms);
        let object = node_lookup(&self.root, hash, 0, key, atoms).ok_or_else(badarg)?;
        // `pos` is a 0-based element index; `key_of` takes a 1-based position and
        // rejects a non-tuple object / out-of-range position with `badarg`.
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        key_of(object, keypos)
    }

    fn size(&self) -> usize {
        self.len
    }

    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        Box::new(self.collect().into_iter().map(|slot| slot.object))
    }

    fn keys(&self) -> Vec<Eterm> {
        self.collect().into_iter().map(|slot| slot.key).collect()
    }

    fn memory(&self) -> usize {
        // The root is stored inline in the interior; every other node and every
        // node's vectors are heap-allocated and counted by `heap_words`.
        words_of::<Node>() + heap_words(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_term::heap::Heap;

    /// A `{Key, Value}` object tuple on `heap` (keypos 1).
    fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    #[test]
    fn insert_lookup_member_delete_roundtrip() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = ChampInterior::new();
        let k = make_small(1);

        assert!(!t.member(k, &atoms));
        assert!(t.lookup(k, &atoms).is_empty());

        let o1 = object(&mut heap, 1, 10);
        assert!(t.insert(k, o1, &atoms).unwrap());
        assert_eq!(t.lookup(k, &atoms), vec![o1]);
        assert!(t.member(k, &atoms));
        assert_eq!(t.size(), 1);

        // A `set` replaces the object of an existing key.
        let o2 = object(&mut heap, 1, 20);
        t.insert(k, o2, &atoms).unwrap();
        assert_eq!(t.lookup(k, &atoms), vec![o2]);
        assert_eq!(t.size(), 1);

        // `insert_new` is a no-op on an existing key, inserts a fresh one.
        assert!(!t.insert_new(k, object(&mut heap, 1, 30), &atoms).unwrap());
        assert_eq!(t.lookup(k, &atoms), vec![o2]);
        let k2 = make_small(2);
        let o4 = object(&mut heap, 2, 40);
        assert!(t.insert_new(k2, o4, &atoms).unwrap());
        assert_eq!(t.lookup(k2, &atoms), vec![o4]);
        assert_eq!(t.size(), 2);

        t.delete(k, &atoms).unwrap();
        assert!(!t.member(k, &atoms));
        assert_eq!(t.size(), 1);
        // Deleting an absent key is a no-op.
        t.delete(make_small(99), &atoms).unwrap();
        assert_eq!(t.size(), 1);

        // `delete_object` removes only a matching object.
        t.delete_object(k2, o4, &atoms).unwrap();
        assert_eq!(t.size(), 0);
        assert!(t.iter_objects().next().is_none());
    }

    #[test]
    fn delete_object_keeps_a_non_matching_object() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = ChampInterior::new();
        let k = make_small(7);
        let stored = object(&mut heap, 7, 1);
        t.insert(k, stored, &atoms).unwrap();
        t.delete_object(k, object(&mut heap, 7, 2), &atoms).unwrap();
        assert_eq!(t.lookup(k, &atoms), vec![stored]);
        assert_eq!(t.size(), 1);
    }

    #[test]
    fn many_keys_stay_lookupable_and_count_correctly() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(8192);
        let mut t = ChampInterior::new();

        const N: i64 = 2000;
        let keys: Vec<Eterm> = (0..N).map(make_small).collect();
        let objs: Vec<Eterm> = (0..N).map(|i| object(&mut heap, i, i * 2)).collect();
        for i in 0..N as usize {
            t.insert(keys[i], objs[i], &atoms).unwrap();
        }
        assert_eq!(t.size(), N as usize);
        for i in 0..N as usize {
            assert_eq!(t.lookup(keys[i], &atoms), vec![objs[i]], "key {i}");
        }
        // Iteration covers every key exactly once.
        assert_eq!(t.iter_objects().count(), N as usize);
        assert_eq!(t.keys().len(), N as usize);

        // Deletion still locates every key.
        for i in 0..N as usize {
            t.delete(keys[i], &atoms).unwrap();
        }
        assert_eq!(t.size(), 0);
        assert!(t.iter_objects().next().is_none());
    }

    #[test]
    fn fully_colliding_hashes_use_a_collision_node() {
        // Drive the collision path directly: two distinct keys sharing a full
        // hash must both be found. `node_insert` with the *same* `hash` but
        // different keys exercises the `same_hash` branch regardless of what
        // `term_hash` returns.
        let atoms = AtomTable::new();
        let mut node = Node::Bitmap(BitmapNode::empty());
        let k1 = make_small(1);
        let k2 = make_small(2);
        let o1 = make_small(11);
        let o2 = make_small(22);
        assert_eq!(
            node_insert(&mut node, 0xABCD, 0, k1, o1, &atoms),
            Upsert::Inserted
        );
        assert_eq!(
            node_insert(&mut node, 0xABCD, 0, k2, o2, &atoms),
            Upsert::Inserted
        );
        assert_eq!(node_lookup(&node, 0xABCD, 0, k1, &atoms), Some(o1));
        assert_eq!(node_lookup(&node, 0xABCD, 0, k2, &atoms), Some(o2));
        // A repeat insert of k1 replaces its object.
        assert_eq!(
            node_insert(&mut node, 0xABCD, 0, k1, o2, &atoms),
            Upsert::Replaced
        );
        assert_eq!(node_lookup(&node, 0xABCD, 0, k1, &atoms), Some(o2));

        // Removal empties the collision node; both keys are gone.
        assert_eq!(node_remove(&mut node, 0xABCD, 0, k1, &atoms), Some(o2));
        assert_eq!(node_remove(&mut node, 0xABCD, 0, k2, &atoms), Some(o2));
        assert!(node_is_empty(&node));
        assert_eq!(node_remove(&mut node, 0xABCD, 0, k1, &atoms), None);
    }

    #[test]
    fn identical_hash_with_an_index_collision_descends() {
        // Two hashes that differ only above the first level: they share the low
        // 5 bits, so the second insert turns the data slot into a sub-node.
        let atoms = AtomTable::new();
        let mut node = Node::Bitmap(BitmapNode::empty());
        let a = 0b0_00001u64; // low 5 bits = 1
        let b = 0b0_00100_00001u64; // low 5 bits = 1, differs at level 1
        assert_eq!(
            node_insert(&mut node, a, 0, make_small(1), make_small(10), &atoms),
            Upsert::Inserted
        );
        assert_eq!(
            node_insert(&mut node, b, 0, make_small(2), make_small(20), &atoms),
            Upsert::Inserted
        );
        assert_eq!(
            node_lookup(&node, a, 0, make_small(1), &atoms),
            Some(make_small(10))
        );
        assert_eq!(
            node_lookup(&node, b, 0, make_small(2), &atoms),
            Some(make_small(20))
        );
        assert_eq!(node_lookup(&node, b, 0, make_small(1), &atoms), None);
    }

    #[test]
    fn canonicalization_inlines_a_single_entry_chain() {
        // Two hashes identical except for the very top bit: they descend the full
        // depth. After removing the top-bit-1 entry, the surviving entry's chain of
        // single-entry nodes must collapse back into one inline slot.
        let atoms = AtomTable::new();
        let mut node = Node::Bitmap(BitmapNode::empty());
        let lo = 0u64;
        let hi = 1u64 << 63;
        node_insert(&mut node, lo, 0, make_small(1), make_small(11), &atoms);
        node_insert(&mut node, hi, 0, make_small(2), make_small(22), &atoms);
        assert_eq!(
            node_remove(&mut node, hi, 0, make_small(2), &atoms),
            Some(make_small(22))
        );
        assert_eq!(
            node_lookup(&node, lo, 0, make_small(1), &atoms),
            Some(make_small(11))
        );
        // The chain collapsed: the root is now a bare inline slot.
        match &node {
            Node::Bitmap(bm) => {
                assert_eq!(bm.nodemap, 0, "the chain must have collapsed");
                assert_eq!(bm.data.len(), 1);
            }
            Node::Collision(_) => panic!("the root must be a bitmap node"),
        }
        // Removing the survivor empties the interior.
        assert_eq!(
            node_remove(&mut node, lo, 0, make_small(1), &atoms),
            Some(make_small(11))
        );
        assert!(node_is_empty(&node));
    }

    #[test]
    fn lookup_element_reads_a_tuple_slot() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = ChampInterior::new();
        let k = make_small(3);
        t.insert(k, object(&mut heap, 3, 99), &atoms).unwrap();
        assert_eq!(t.lookup_element(k, 1, &atoms).unwrap(), make_small(99));
        // Out-of-range and absent keys are `badarg`.
        assert!(matches!(
            t.lookup_element(k, 5, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(4), 0, &atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn memory_is_a_non_negative_honest_word_count() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(8192);
        let mut t = ChampInterior::new();

        let empty = t.memory();
        assert!(empty > 0);
        let mut prev = empty;
        for i in 0..500i64 {
            t.insert(make_small(i), object(&mut heap, i, i), &atoms).unwrap();
            let now = t.memory();
            assert!(now >= prev, "count shrank on insert {i} ({prev} -> {now})");
            prev = now;
        }
        assert!(t.memory() >= t.size());
        assert_eq!(t.size(), 500);
        // Emptying the interior returns it to (at most) its fresh footprint.
        for i in 0..500i64 {
            t.delete(make_small(i), &atoms).unwrap();
        }
        assert!(t.memory() <= prev);
    }
}
