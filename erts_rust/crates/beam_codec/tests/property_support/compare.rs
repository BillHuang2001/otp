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

//! Structural term comparison for the oracle cross-checks.
//!
//! The harness compares terms structurally, not by `beam_term::compare::eq` (whose
//! fallback for maps/identifiers compares the raw heap word): `terms_equal` walks the
//! structure through the safe `beam_codec::term` accessors (atoms by name, floats by
//! bit pattern, bitstrings bit-by-bit) — the crate's `tests/interop.rs::terms_equal`
//! contract, narrowed to the renderable corpus.

use beam_codec::term::{
    big_parts, bitstring_bytes, bitstring_size_bits, cons_head, cons_tail, external_pid_parts,
    external_port_parts, external_ref_parts, float_value, fun_arity_term, fun_kind_term,
    fun_module, tuple_arity, tuple_element,
};
use beam_term::prelude::{
    atom_val, is_atom, is_internal_pid, is_internal_port, is_list, is_nil, is_small, signed_value,
    AtomTable, Eterm,
};

fn bits_equal(ba: &[u8], oa: usize, bb: &[u8], ob: usize, nbits: usize) -> bool {
    let bit = |bytes: &[u8], i: usize| (bytes[i >> 3] >> (7 - (i & 7))) & 1;
    (0..nbits).all(|i| bit(ba, oa + i) == bit(bb, ob + i))
}

/// Structural `=:=` between two terms (atoms by name, floats by bit pattern,
/// bitstrings bit-by-bit) — the crate's `tests/interop.rs::terms_equal` contract,
/// narrowed to the renderable corpus.
pub fn terms_equal(a: Eterm, b: Eterm, atoms: &AtomTable) -> bool {
    if is_nil(a) || is_nil(b) {
        return is_nil(a) && is_nil(b);
    }
    if is_small(a) || is_small(b) {
        return is_small(a) && is_small(b) && signed_value(a) == signed_value(b);
    }
    if is_atom(a) || is_atom(b) {
        return is_atom(a) && is_atom(b) && atoms.name(atom_val(a)) == atoms.name(atom_val(b));
    }
    if is_list(a) || is_list(b) {
        if !(is_list(a) && is_list(b)) {
            return false;
        }
        let (Some(ha), Some(ta), Some(hb), Some(tb)) = (
            cons_head(a).ok(),
            cons_tail(a).ok(),
            cons_head(b).ok(),
            cons_tail(b).ok(),
        ) else {
            return false;
        };
        return terms_equal(ha, hb, atoms) && terms_equal(ta, tb, atoms);
    }
    if let (Ok(x), Ok(y)) = (float_value(a), float_value(b)) {
        return x.to_bits() == y.to_bits();
    }
    if let (Ok((sa, da)), Ok((sb, db))) = (big_parts(a), big_parts(b)) {
        return sa == sb && da == db;
    }
    if let (Ok(ka), Ok(kb)) = (fun_kind_term(a), fun_kind_term(b)) {
        if ka != kb || fun_arity_term(a).ok() != fun_arity_term(b).ok() {
            return false;
        }
        return match (fun_module(a).ok(), fun_module(b).ok()) {
            (Some(ma), Some(mb)) => atoms.name(ma) == atoms.name(mb),
            _ => false,
        };
    }
    if let (Ok(na), Ok(nb)) = (bitstring_size_bits(a), bitstring_size_bits(b)) {
        if na != nb {
            return false;
        }
        let (Ok((ba, oa, _)), Ok((bb, ob, _))) = (bitstring_bytes(a), bitstring_bytes(b)) else {
            return false;
        };
        return bits_equal(ba, oa, bb, ob, na);
    }
    if let (Ok(pa), Ok(pb)) = (
        beam_term::map::map_pairs(a, atoms),
        beam_term::map::map_pairs(b, atoms),
    ) {
        return pa.len() == pb.len()
            && pa.iter().zip(&pb).all(|((ka, va), (kb, vb))| {
                terms_equal(*ka, *kb, atoms) && terms_equal(*va, *vb, atoms)
            });
    }
    if let (Ok(na), Ok(nb)) = (tuple_arity(a), tuple_arity(b)) {
        return na == nb
            && (0..na).all(
                |i| match (tuple_element(a, i).ok(), tuple_element(b, i).ok()) {
                    (Some(x), Some(y)) => terms_equal(x, y, atoms),
                    _ => false,
                },
            );
    }
    if let (Ok(pa), Ok(pb)) = (external_pid_parts(a), external_pid_parts(b)) {
        return terms_equal(pa.0, pb.0, atoms) && pa.1 == pb.1 && pa.2 == pb.2 && pa.3 == pb.3;
    }
    if let (Ok(pa), Ok(pb)) = (external_port_parts(a), external_port_parts(b)) {
        return terms_equal(pa.0, pb.0, atoms) && pa.1 == pb.1 && pa.2 == pb.2;
    }
    if let (Ok(ra), Ok(rb)) = (external_ref_parts(a), external_ref_parts(b)) {
        return terms_equal(ra.0, rb.0, atoms) && ra.1 == rb.1 && ra.2 == rb.2;
    }
    if is_internal_pid(a) || is_internal_pid(b) || is_internal_port(a) || is_internal_port(b) {
        return a.raw() == b.raw();
    }
    panic!("terms_equal: unclassified terms {a:?} / {b:?}");
}
