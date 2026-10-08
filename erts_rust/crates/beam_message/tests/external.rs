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

//! Integration tests for external pid/port/reference node linkage.
//!
//! Copying an external thing **registers its node** into the destination off-heap table, refcounted
//! per node identity `(node atom, creation)` (`erts_ref_node_entry`, `copy.c:882`), and copies the
//! thing's words verbatim.
//!
//! # Why not `beam_term::compare::eq`
//!
//! `compare` has no layout-aware arm for external pid/port/reference: its documented deterministic
//! fallback orders two boxed terms of the same class by **raw term word**, i.e. by heap address, so
//! a copy is never `=:=` its original. Structural equality is therefore asserted through the
//! layout accessors (`external_*_parts`) and the node identity — see the crate `CONTEXT.md`
//! `Known Issues` (fixing `compare` belongs to `beam_term`).

use beam_alloc::Heap;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, size_of_copy, CopyContext};
use beam_term::prelude::*;

#[test]
fn an_external_pid_is_copied_verbatim_and_registers_its_node() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    let mut src = Heap::new(32).expect("source heap");
    // SAFETY: the heap owns the 4-word external pid block.
    let original = unsafe {
        let hp = src.alloc_words(external_pid_size_words()).expect("the pid fits");
        write_external_pid(hp, node, 0x1234, 0x0007, 5)
    };

    let src_off = OffHeapTable::new();
    let mut dst = Heap::new(32).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(original, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    assert_eq!(dst.used(), size_of_copy(original), "the thing's words are copied verbatim");
    assert_ne!(boxed_val(copy), boxed_val(original), "a real copy");

    // SAFETY: both terms are live 4-word external pid blocks.
    let (o_node, o_num, o_ser, o_creation) = unsafe { external_pid_parts(original) };
    let (c_node, c_num, c_ser, c_creation) = unsafe { external_pid_parts(copy) };
    assert_eq!(o_node.raw(), node.raw());
    assert_eq!(c_node.raw(), node.raw(), "the node atom is preserved");
    assert_eq!((c_num, c_ser, c_creation), (o_num, o_ser, o_creation));
    assert_eq!((c_num, c_ser), (0x1234, 0x0007));

    // The node linkage is registered once, with one reference.
    assert_eq!(dst_off.nodes().len(), 1, "one entry per distinct node");
    assert_eq!(dst_off.nodes()[0].node().raw(), node.raw());
    assert_eq!(dst_off.nodes()[0].creation(), 5);
    assert_eq!(dst_off.nodes()[0].refcount(), 1, "one reference for one copied thing");
    assert!(dst_off.node_by_identity(node, 5).is_some());
    assert!(dst_off.node_by_identity(node, 6).is_none(), "a different creation is a different node");
}

#[test]
fn two_references_to_the_same_node_share_one_entry_with_refcount_two() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    let mut src = Heap::new(48).expect("source heap");
    // SAFETY: two 4-word external pid blocks over the same node and creation.
    let (pid_a, pid_b) = unsafe {
        let a = write_external_pid(src.alloc_words(4).expect("fits"), node, 1, 1, 9);
        let b = write_external_pid(src.alloc_words(4).expect("fits"), node, 2, 2, 9);
        (a, b)
    };
    let pair = src.tuple(&[pid_a, pid_b]).expect("the tuple fits");

    let src_off = OffHeapTable::new();
    let mut dst = Heap::new(48).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(pair, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    assert_eq!(dst_off.nodes().len(), 1, "one entry per distinct node");
    assert_eq!(
        dst_off.nodes()[0].refcount(),
        2,
        "one reference per copied external thing"
    );

    // SAFETY: `copy` is a live 2-tuple of copied external pids.
    let (first, second) = unsafe { (tuple_element(copy, 0), tuple_element(copy, 1)) };
    // SAFETY: both are live 4-word external pid blocks.
    let (_, num_a, _, creation_a) = unsafe { external_pid_parts(first) };
    let (_, num_b, _, creation_b) = unsafe { external_pid_parts(second) };
    assert_eq!((num_a, creation_a), (1, 9));
    assert_eq!((num_b, creation_b), (2, 9));

    // Dropping the destination releases both references.
    let held = dst_off.nodes()[0].handle();
    assert_eq!(held.refcount(), 2);
    drop(dst_off);
    assert_eq!(held.refcount(), 0, "dropping the destination releases the node");
}

#[test]
fn external_port_and_reference_nodes_are_registered_separately() {
    let mut atoms = AtomTable::new();
    let node_a = make_atom(atoms.intern("node_a@host"));
    let node_b = make_atom(atoms.intern("node_b@host"));

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `external_port_size_words()` / `external_ref_size_words(3)` words are reserved for
    // each thing, and 3 ids need 5 words.
    let (port, reference) = unsafe {
        let p = write_external_port(
            src.alloc_words(external_port_size_words()).expect("the port fits"),
            node_a,
            0x9999,
            3,
        );
        let r = write_external_ref(
            src.alloc_words(external_ref_size_words(3)).expect("the ref fits"),
            node_b,
            &[10, 20, 30],
            4,
        );
        (p, r)
    };
    let tuple = src.tuple(&[port, reference]).expect("the tuple fits");

    let src_off = OffHeapTable::new();
    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(tuple, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // Two distinct nodes -> two entries.
    assert_eq!(dst_off.nodes().len(), 2, "one entry per distinct node");
    assert!(dst_off.node_by_identity(node_a, 3).is_some());
    assert!(dst_off.node_by_identity(node_b, 4).is_some());
    assert_eq!(dst_off.node_by_identity(node_a, 3).map(|n| n.refcount()), Some(1));
    assert_eq!(dst_off.node_by_identity(node_b, 4).map(|n| n.refcount()), Some(1));

    // SAFETY: `copy` is a live 2-tuple of a copied external port and reference.
    let (c_port, c_ref) = unsafe { (tuple_element(copy, 0), tuple_element(copy, 1)) };
    // SAFETY: `c_port` is a live external port block of the same shape as `port`.
    let (p_node, p_num, p_creation) = unsafe { external_port_parts(c_port) };
    let (o_node, o_num, o_creation) = unsafe { external_port_parts(port) };
    assert_eq!(p_node.raw(), node_a.raw(), "the port node atom is preserved");
    assert_eq!((p_num, p_creation), (o_num, o_creation));
    assert_eq!((p_num, p_creation), (0x9999, 3));
    assert_eq!(o_node.raw(), node_a.raw());

    // SAFETY: `c_ref` is a live external reference block of the same shape as `reference`.
    let (r_node, r_ids, r_creation) = unsafe { external_ref_parts(c_ref) };
    let (or_node, or_ids, or_creation) = unsafe { external_ref_parts(reference) };
    assert_eq!(r_node.raw(), node_b.raw(), "the reference node atom is preserved");
    assert_eq!(r_ids, or_ids);
    assert_eq!(r_ids, &[10, 20, 30]);
    assert_eq!(r_creation, or_creation);
    assert_eq!(r_creation, 4);
    assert_eq!(or_node.raw(), node_b.raw());
}

#[test]
fn a_node_is_registered_once_however_many_times_it_is_copied() {
    let mut atoms = AtomTable::new();
    let node = make_atom(atoms.intern("nonode@nohost"));

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: five 4-word external pid blocks, all over the same node identity.
    let pids: Vec<Eterm> = unsafe {
        (0..5)
            .map(|i| {
                write_external_pid(
                    src.alloc_words(4).expect("fits"),
                    node,
                    i as u32,
                    i as u32,
                    11,
                )
            })
            .collect()
    };
    let list = {
        let mut acc = NIL;
        for &pid in &pids {
            acc = src.cons(pid, acc).expect("each cons fits");
        }
        acc
    };

    let src_off = OffHeapTable::new();
    let mut dst = Heap::new(128).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let _copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(list, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    assert_eq!(dst_off.nodes().len(), 1, "still one entry per distinct node");
    assert_eq!(dst_off.nodes()[0].refcount(), 5, "one reference per copied thing");
}
