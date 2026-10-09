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

//! The ERTS `make_hash2` port (`erlang:phash2/1,2`).
//!
//! Bit-exact u32-wrapping port of `make_hash2`/`make_hash2_helper` in
//! `erts/emulator/beam/erl_term_hashing.c` (the non-trapping path only — there is
//! no process/reds machinery here). Unlike [`super::term_hash`], **these values
//! are a compatibility contract**: the system ERTS produces them, so a Rust term
//! must hash identically (Level-1 drop-in).
//!
//! The reachable typed failure is a `FunKind::Local` fun (a `make_fun3` closure),
//! whose inlined 2-word descriptor carries no module and no `old_uniq`; it is
//! reported as `TermError::FunIdentityUnavailable` — see the `common` submodule
//! for the shared error-handling note.

use crate::atom::AtomTable;
use crate::bitstring::float_bits;
use crate::boxed::{
    arityval, boxed_val, header_arity, header_subtag, internal_ref_numbers, is_arity_value,
    is_boxed_term, make_header, map_header_val,
};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::fun::{
    fun_env, fun_function, fun_index, fun_kind_of_header, fun_module, fun_num_free, fun_old_uniq,
    fun_true_arity, FunKind,
};
use crate::immediate::{
    atom_val, internal_port_data, is_byte, is_small, pid_number, signed_value, unsigned_value,
};
use crate::list::{is_list, list_val};
use crate::record::{
    record_declaration_keys, record_definition, record_definition_hash,
    record_definition_is_exported, record_definition_module, record_definition_name,
    record_field_count, record_values,
};
use crate::tag::{
    primary_tag, PrimaryTag, ARITYVAL_SUBTAG, BIN_REF_SUBTAG, EXTERNAL_PID_SUBTAG,
    EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG, FLOAT_SUBTAG, FUN_SUBTAG, HAMT_SUBTAG_HEAD_ARRAY,
    HAMT_SUBTAG_HEAD_BITMAP, HAMT_SUBTAG_HEAD_FLATMAP, HAMT_SUBTAG_NODE_BITMAP,
    HEADER_MAP_SUBTAG_MASK, HEAP_BITS_SUBTAG, MAP_SUBTAG, NEG_BIG_SUBTAG, POS_BIG_SUBTAG,
    RECORD_SUBTAG, REF_SUBTAG, SUB_BITS_SUBTAG, TAG_IMMED1_IMMED2, TAG_IMMED1_MASK, TAG_IMMED1_PID,
    TAG_IMMED1_PORT, TAG_IMMED1_SMALL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK, TAG_IMMED2_NIL,
};
use crate::thing::{
    big_parts, external_pid_parts, external_port_parts, external_ref_parts, flatmap_parts,
};
use crate::tuple::tuple_val;

use super::common::{
    base_bits, bitstring_parts_any, block_hash, is_ssmall28, not_ssmall28_hash, sint32_hash,
    uint32_hash, uint32_hash_2, HCONST, HCONST_10, HCONST_11, HCONST_12, HCONST_13, HCONST_14,
    HCONST_15, HCONST_16, HCONST_19, HCONST_2, HCONST_23, HCONST_3, HCONST_4, HCONST_5, HCONST_6,
    HCONST_7, HCONST_9,
};

/// `NIL_DEF` (`erl_term.h:1443`) — the virtual tag mixed for `nil`.
const NIL_DEF: u32 = 2;
/// The `hash == 0` fast-path value for `nil` (`erl_term_hashing.c:1522`).
const NIL_HASH: u32 = 3_468_870_702;

/// `HASH_MAP_TAIL` (`erl_term_hashing.c:890`) — the map xor-protocol sentinel.
const HASH_MAP_TAIL: Eterm = make_header(1, REF_SUBTAG);
/// `HASH_MAP_PAIR` (`erl_term_hashing.c:891`).
const HASH_MAP_PAIR: Eterm = make_header(2, REF_SUBTAG);

/// `make_hash2(term)` (`erl_term_hashing.c:1597-1601`, `erlang:phash2/1,2`).
///
/// Bit-exact for every term whose fun kind is not `make_fun3`-local; see the
/// module docs for the error contract.
pub fn make_hash2(t: Eterm, atoms: &AtomTable) -> TermResult<u32> {
    // "Optimization. Simple cases before declaration of estack."
    if primary_tag(t) == PrimaryTag::Immed1 {
        let m1 = t.raw() & TAG_IMMED1_MASK;
        if m1 == TAG_IMMED1_IMMED2 && (t.raw() & TAG_IMMED2_MASK) == TAG_IMMED2_ATOM {
            // `return atom_tab(...)->hvalue;`
            return Ok(atoms.hvalue(atom_val(t)));
        }
        if m1 == TAG_IMMED1_SMALL {
            let small = signed_value(t);
            return Ok(if !is_ssmall28(small) {
                not_ssmall28_hash(0, small)
            } else {
                sint32_hash(0, small as i32, HCONST)
            });
        }
    }
    make_hash2_from(t, 0, atoms)
}

/// The `make_hash2` body with an explicit running hash and an empty work stack —
/// how a caller hashes a *synthetic* term (e.g. the record-definition tuple)
/// without materializing it. The entry fast-paths above are deliberately not
/// re-applied: the C's `ESTACK` machinery never applies them mid-stream.
fn make_hash2_from(term_arg: Eterm, hash_arg: u32, atoms: &AtomTable) -> TermResult<u32> {
    let mut hash: u32 = hash_arg;
    let mut hash_xor_pairs: u32 = 0;
    let mut term = term_arg;
    let mut stack: Vec<Eterm> = Vec::new();

    'outer: loop {
        // `to_common` mirrors the C's `goto hash2_common`; the LIST/ARITYVAL/
        // RECORD/FUN-local arms instead fall out of the switch and re-process
        // `term` on the next iteration.
        let to_common;
        match primary_tag(term) {
            PrimaryTag::List => {
                let mut ptr = list_val(term);
                let mut c: u32 = 0;
                let mut sh: u32 = 0;
                loop {
                    // SAFETY: `ptr` is the CAR of a live cons cell.
                    let head = unsafe { *ptr };
                    if !is_byte(head) {
                        break;
                    }
                    sh = (sh << 8).wrapping_add(unsigned_value(head) as u32);
                    if c == 3 {
                        hash = uint32_hash(hash, sh, HCONST_4);
                        c = 0;
                        sh = 0;
                    } else {
                        c += 1;
                    }
                    // SAFETY: `ptr` is a live cons cell, so word 1 is its CDR.
                    term = unsafe { *ptr.add(1) };
                    if !is_list(term) {
                        break;
                    }
                    // SAFETY: `term` is a cons cell.
                    ptr = list_val(term);
                }
                if c > 0 {
                    hash = uint32_hash(hash, sh, HCONST_4);
                }
                if is_list(term) {
                    // SAFETY: `ptr` is the last processed cons cell.
                    stack.push(unsafe { *ptr.add(1) }); // CDR
                    term = unsafe { *ptr }; // CAR
                }
                to_common = false;
            }
            PrimaryTag::Boxed => {
                // SAFETY: `term` is boxed.
                let p = boxed_val(term);
                let hdr = unsafe { *p };
                match header_subtag(hdr) as usize {
                    RECORD_SUBTAG => {
                        let field_count = unsafe { record_field_count(term)? };
                        let def = unsafe { record_definition(term)? };
                        let hash_term = unsafe { record_definition_hash(def)? };
                        if !is_small(hash_term) {
                            return Err(TermError::BadRecord);
                        }
                        let def_hash_val = unsigned_value(hash_term) as u32;
                        let values = unsafe { record_values(term)? };
                        hash = uint32_hash_2(hash, field_count as u32, def_hash_val, HCONST_23);
                        if field_count == 0 {
                            to_common = true;
                        } else {
                            // `values[arity-1] .. values[1]` are pushed in that
                            // order (so they pop in forward order); `values[0]` is
                            // processed now.
                            for i in (1..field_count).rev() {
                                stack.push(values[i]);
                            }
                            term = values[0];
                            to_common = false;
                        }
                    }
                    ARITYVAL_SUBTAG => {
                        let arity = header_arity(hdr);
                        hash = uint32_hash(hash, arity as u32, HCONST_9);
                        if arity == 0 {
                            to_common = true;
                        } else {
                            // `tuple_val(term)[i]` is element `i-1`.
                            // SAFETY: `term` is a tuple of `arity` elements.
                            let base = tuple_val(term);
                            for i in (2..=arity).rev() {
                                // SAFETY: `i <= arity`.
                                stack.push(unsafe { *base.add(i) });
                            }
                            // SAFETY: element 0 lives at word 1.
                            term = unsafe { *base.add(1) };
                            to_common = false;
                        }
                    }
                    MAP_SUBTAG => {
                        let map_tag = hdr.raw() & HEADER_MAP_SUBTAG_MASK;
                        if map_tag == HAMT_SUBTAG_HEAD_FLATMAP {
                            // SAFETY: the header says flat map.
                            let (ks, vs) = unsafe { flatmap_parts(term) };
                            let size = ks.len();
                            hash = uint32_hash(hash, size as u32, HCONST_16);
                            if size == 0 {
                                to_common = true;
                            } else {
                                stack.push(Eterm::from_raw(hash_xor_pairs as usize));
                                stack.push(Eterm::from_raw(hash as usize));
                                stack.push(HASH_MAP_TAIL);
                                hash = 0;
                                hash_xor_pairs = 0;
                                for i in (0..size).rev() {
                                    stack.push(HASH_MAP_PAIR);
                                    stack.push(vs[i]);
                                    stack.push(ks[i]);
                                }
                                to_common = true;
                            }
                        } else {
                            // `ctx.ptr = boxed_val(term) + 1`.
                            let mut ptr = unsafe { p.add(1) };
                            let mut empty = false;
                            if map_tag == HAMT_SUBTAG_HEAD_ARRAY
                                || map_tag == HAMT_SUBTAG_HEAD_BITMAP
                            {
                                // HEAD_ARRAY/HEAD_BITMAP nodes carry a size word.
                                // SAFETY: node layout `[hdr][size][slots..]`.
                                let size = unsafe { *ptr }.raw() as u32;
                                // SAFETY: as above.
                                ptr = unsafe { ptr.add(1) };
                                hash = uint32_hash(hash, size, HCONST_16);
                                if size == 0 {
                                    empty = true;
                                } else {
                                    stack.push(Eterm::from_raw(hash_xor_pairs as usize));
                                    stack.push(Eterm::from_raw(hash as usize));
                                    stack.push(HASH_MAP_TAIL);
                                    hash = 0;
                                    hash_xor_pairs = 0;
                                }
                            }
                            if empty {
                                to_common = true;
                            } else {
                                let count = match map_tag {
                                    HAMT_SUBTAG_HEAD_ARRAY => 16usize,
                                    HAMT_SUBTAG_HEAD_BITMAP | HAMT_SUBTAG_NODE_BITMAP => {
                                        map_header_val(hdr).count_ones() as usize
                                    }
                                    _ => return Err(TermError::BadMap),
                                };
                                for _ in 0..count {
                                    // SAFETY: `ptr` walks the node's slots.
                                    let slot = unsafe { *ptr };
                                    if is_list(slot) {
                                        // SAFETY: `slot` is a leaf cons cell.
                                        let cons = list_val(slot);
                                        stack.push(HASH_MAP_PAIR);
                                        // SAFETY: word 1 of the cons is its CDR.
                                        stack.push(unsafe { *cons.add(1) });
                                        // SAFETY: word 0 of the cons is its CAR.
                                        stack.push(unsafe { *cons });
                                    } else if is_boxed_term(slot) {
                                        // SAFETY: a map node slot is a term.
                                        let shdr = unsafe { *boxed_val(slot) };
                                        if is_arity_value(shdr) {
                                            // Collision node: a tuple of cons cells.
                                            let mut n = arityval(shdr);
                                            // SAFETY: the collision tuple's elements
                                            // start at word 1.
                                            let mut cp = unsafe { boxed_val(slot).add(1) };
                                            while n > 0 {
                                                // SAFETY: `cp` is a live cons cell.
                                                let cons = list_val(unsafe { *cp });
                                                stack.push(HASH_MAP_PAIR);
                                                // SAFETY: as above.
                                                stack.push(unsafe { *cons.add(1) });
                                                // SAFETY: as above.
                                                stack.push(unsafe { *cons });
                                                // SAFETY: as above.
                                                cp = unsafe { cp.add(1) };
                                                n -= 1;
                                            }
                                        } else {
                                            // A sub-node: re-processed as a term.
                                            stack.push(slot);
                                        }
                                    } else {
                                        return Err(TermError::BadMap);
                                    }
                                    // SAFETY: as above.
                                    ptr = unsafe { ptr.add(1) };
                                }
                                to_common = true;
                            }
                        }
                    }
                    FUN_SUBTAG => {
                        match fun_kind_of_header(hdr) {
                            // The C's two "local" fun kinds use `ErlFunEntry`. Our
                            // `Imported` kind (a `NEW_FUN_EXT` fun) carries the same
                            // fields, so it computes.
                            FunKind::Imported => {
                                let num_free = fun_num_free(hdr);
                                let module = unsafe { fun_module(term)? };
                                let index = unsafe { fun_index(term)? };
                                let old_uniq = unsafe { fun_old_uniq(term)? };
                                hash = uint32_hash_2(
                                    hash,
                                    num_free as u32,
                                    atoms.hvalue(module),
                                    HCONST,
                                );
                                hash = uint32_hash_2(hash, index, old_uniq, HCONST);
                                if num_free == 0 {
                                    to_common = true;
                                } else {
                                    let env = unsafe { fun_env(term)? };
                                    for i in (1..num_free).rev() {
                                        stack.push(env[i]);
                                    }
                                    term = env[0];
                                    to_common = false;
                                }
                            }
                            FunKind::External => {
                                // The C mixes `ep->info.mfa.arity` — the `Export`'s
                                // TRUE arity (`erl_term_hashing.c:1248-1255`), not the
                                // header's 8-bit field. `mfa.arity` is a `Uint`, so the
                                // `u32` cast is the C's own width.
                                // SAFETY: `term` is a live external fun.
                                let arity = unsafe { fun_true_arity(term)? };
                                let module = unsafe { fun_module(term)? };
                                let function = unsafe { fun_function(term)? };
                                hash = uint32_hash_2(
                                    hash,
                                    arity as u32,
                                    atoms.hvalue(module),
                                    HCONST,
                                );
                                hash = uint32_hash(hash, atoms.hvalue(function), HCONST_14);
                                to_common = true;
                            }
                            // `make_fun3`'s inlined descriptor is only
                            // `[uniq:u64][index:u32]` — no module, no `old_uniq`.
                            // The term is well-formed; the *identity* the C mixes
                            // in simply is not carried. Never substitute a wrong
                            // value.
                            FunKind::Local => return Err(TermError::FunIdentityUnavailable),
                        }
                    }
                    BIN_REF_SUBTAG | HEAP_BITS_SUBTAG | SUB_BITS_SUBTAG => {
                        // SAFETY: the header says bitstring.
                        let (ptr, offset, size) = unsafe { bitstring_parts_any(term) };
                        let con = HCONST_13.wrapping_add(hash);
                        let bytesize = size / 8;
                        let bitoffs = offset & 7;
                        let bitsize = size & 7;
                        // The C truncates the byte count to 32 bits for
                        // compatibility with older releases.
                        let sz = bytesize & 0xffff_ffff;
                        // SAFETY: `ptr` is the bitstring's first byte.
                        let base = unsafe { ptr.add(offset / 8) };
                        if sz == 0 && bitsize == 0 {
                            hash = con;
                        } else if bitoffs == 0 {
                            // SAFETY: `sz <= bytesize` bytes are readable.
                            let bytes = unsafe { core::slice::from_raw_parts(base, sz) };
                            hash = block_hash(bytes, con);
                            if bitsize > 0 {
                                // SAFETY: the partial byte follows the whole bytes.
                                let last = unsafe { *base.add(sz) };
                                hash = uint32_hash_2(
                                    hash,
                                    bitsize as u32,
                                    u32::from(last >> (8 - bitsize)),
                                    HCONST_15,
                                );
                            }
                        } else {
                            // Non-byte-aligned: copy the bits into a flat buffer.
                            let nbits = sz * 8 + bitsize;
                            let mut buf = vec![0u8; sz + 1];
                            for i in 0..nbits {
                                let b = (base_bits(base, bitoffs, i) != 0) as u8;
                                buf[i >> 3] |= b << (7 - (i & 7));
                            }
                            hash = block_hash(&buf[..sz], con);
                            if bitsize > 0 {
                                hash = uint32_hash_2(
                                    hash,
                                    bitsize as u32,
                                    u32::from(buf[sz] >> (8 - bitsize)),
                                    HCONST_15,
                                );
                            }
                        }
                        to_common = true;
                    }
                    POS_BIG_SUBTAG | NEG_BIG_SUBTAG => {
                        // SAFETY: the header says bignum.
                        let (neg, digits) = unsafe { big_parts(term) };
                        let con = if neg { HCONST_10 } else { HCONST_11 };
                        for &d in digits {
                            let x = (d & 0xffff_ffff) as u32;
                            let y = (d >> 32) as u32;
                            hash = uint32_hash_2(hash, x, y, con);
                        }
                        to_common = true;
                    }
                    REF_SUBTAG => {
                        // SAFETY: the header says internal reference.
                        let nums = unsafe { internal_ref_numbers(term) };
                        hash = uint32_hash(hash, nums[0], HCONST_7);
                        to_common = true;
                    }
                    EXTERNAL_REF_SUBTAG => {
                        // SAFETY: the header says external reference.
                        let (_, ids, _) = unsafe { external_ref_parts(term) };
                        // `UINT32_HASH_RET(external_ref_numbers(term)[0], …)`
                        // (`erl_term_hashing.c:1459-1461`): `external_ref_numbers` is
                        // `&external_ref_data(x)[1]`, i.e. the word right after the id
                        // count, so a zero-length reference mixes the zeroed word there
                        // — 0 — rather than failing.
                        let first = ids.first().copied().unwrap_or(0);
                        hash = uint32_hash(hash, first, HCONST_7);
                        to_common = true;
                    }
                    EXTERNAL_PID_SUBTAG => {
                        // SAFETY: the header says external pid.
                        let (_, number, _, _) = unsafe { external_pid_parts(term) };
                        hash = uint32_hash(hash, number, HCONST_5);
                        to_common = true;
                    }
                    EXTERNAL_PORT_SUBTAG => {
                        // SAFETY: the header says external port.
                        let (_, number, _) = unsafe { external_port_parts(term) };
                        let low = (number & 0xffff_ffff) as u32;
                        let high = (number >> 32) as u32;
                        hash = uint32_hash_2(hash, low, high, HCONST_6);
                        to_common = true;
                    }
                    FLOAT_SUBTAG => {
                        // SAFETY: the header says float.
                        let mut bits = unsafe { float_bits(term) };
                        if f64::from_bits(bits) == 0.0 {
                            // Ensure positive 0.0 (the C replaces `-0.0`).
                            bits = 0f64.to_bits();
                        }
                        // Little-endian: `UINT32_HASH_2(ff.fw[1], ff.fw[0], ..)`.
                        let low = (bits & 0xffff_ffff) as u32;
                        let high = (bits >> 32) as u32;
                        hash = uint32_hash_2(hash, high, low, HCONST_12);
                        to_common = true;
                    }
                    _ => return Err(TermError::BadFun),
                }
            }
            PrimaryTag::Immed1 => {
                let m1 = term.raw() & TAG_IMMED1_MASK;
                if m1 == TAG_IMMED1_PID {
                    hash = uint32_hash(hash, pid_number(term) as u32, HCONST_5);
                } else if m1 == TAG_IMMED1_PORT {
                    let number = internal_port_data(term);
                    let low = (number & 0xffff_ffff) as u32;
                    let high = (number >> 32) as u32;
                    hash = uint32_hash_2(hash, low, high, HCONST_6);
                } else if m1 == TAG_IMMED1_IMMED2 {
                    match term.raw() & TAG_IMMED2_MASK {
                        a if a == TAG_IMMED2_ATOM => {
                            let hv = atoms.hvalue(atom_val(term));
                            hash = if hash == 0 {
                                hv
                            } else {
                                uint32_hash(hash, hv, HCONST_3)
                            };
                        }
                        n if n == TAG_IMMED2_NIL => {
                            hash = if hash == 0 {
                                NIL_HASH
                            } else {
                                uint32_hash(hash, NIL_DEF, HCONST_2)
                            };
                        }
                        _ => return Err(TermError::BadFun),
                    }
                } else if m1 == TAG_IMMED1_SMALL {
                    let small = signed_value(term);
                    hash = if !is_ssmall28(small) {
                        not_ssmall28_hash(hash, small)
                    } else {
                        sint32_hash(hash, small as i32, HCONST)
                    };
                } else {
                    return Err(TermError::BadFun);
                }
                to_common = true;
            }
            PrimaryTag::Header => return Err(TermError::NotATerm),
        }

        if !to_common {
            continue 'outer;
        }

        // `hash2_common:` — pop the saved continuation.
        loop {
            match stack.pop() {
                None => return Ok(hash),
                Some(v) if v == HASH_MAP_TAIL => {
                    hash = pop_term(&mut stack)?.raw() as u32;
                    hash = uint32_hash(hash, hash_xor_pairs, HCONST_19);
                    hash_xor_pairs = pop_term(&mut stack)?.raw() as u32;
                }
                Some(v) if v == HASH_MAP_PAIR => {
                    hash_xor_pairs ^= hash;
                    hash = 0;
                }
                Some(v) => {
                    term = v;
                    break;
                }
            }
        }
    }
}

/// Pop a term off the `make_hash2` work stack.
fn pop_term(stack: &mut Vec<Eterm>) -> TermResult<Eterm> {
    stack.pop().ok_or(TermError::BadFun)
}

/// The record-definition hash `records:create/4` stores in a definition
/// (`erl_record.c:1041-1046`): `make_hash2` of the tuple
/// `{module, name, is_exported, keys-in-declaration-order}`.
///
/// The tuple is **not materialized** — this crate cannot allocate — so the
/// equivalent running hash is computed directly (arity mix, then one
/// [`make_hash2_from`] step per element), which is exactly what the C's
/// `ESTACK` walk does.
///
/// (Named `compute_record_definition_hash` rather than the obvious
/// `record_definition_hash` because [`crate::record::record_definition_hash`]
/// already owns that name and `lib.rs` glob-re-exports both modules — the
/// collision is a `ambiguous_glob_reexports` warning we may not fix from here.)
pub fn compute_record_definition_hash(def: Eterm, atoms: &AtomTable) -> TermResult<u32> {
    let module = unsafe { record_definition_module(def)? };
    let name = unsafe { record_definition_name(def)? };
    let is_exported = unsafe { record_definition_is_exported(def)? };
    let keys = unsafe { record_declaration_keys(def)? };
    let arity = 3 + keys.len();
    let mut hash = uint32_hash(0, arity as u32, HCONST_9);
    hash = make_hash2_from(module, hash, atoms)?;
    hash = make_hash2_from(name, hash, atoms)?;
    hash = make_hash2_from(is_exported, hash, atoms)?;
    for key in keys {
        hash = make_hash2_from(key, hash, atoms)?;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    //! Vectors pinning the exact `make_hash2` algorithm. The end-to-end
    //! cross-check against the real runtime lives in `tests/oracle.rs`; here we
    //! pin the values that can be derived from the C by inspection plus
    //! structural/consistency properties.

    use super::*;
    use crate::atom::Atom;
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, MAX_SMALL, MIN_SMALL, NIL};
    use crate::map::{map_words_capacity, write_map};

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "abc", "b"]);
        t
    }

    #[test]
    fn atom_hvalue_is_the_c_hashpjw() {
        // `atom.c:82`: h = (h << 4) + byte, with the dragon-book fold.
        // "" -> 0; "a" -> 97; "abc" -> ((97*16 + 98)*16) + 99 = 26499.
        let t = atoms();
        assert_eq!(t.hvalue(t.lookup("").unwrap_or(Atom::from_raw(999))), 999);
        let mut t2 = AtomTable::new();
        t2.seed(["", "a", "abc"]);
        assert_eq!(t2.hvalue(t2.lookup("").unwrap()), 0);
        assert_eq!(t2.hvalue(t2.lookup("a").unwrap()), 97);
        assert_eq!(t2.hvalue(t2.lookup("abc").unwrap()), 26499);
        // An index that is not in the table has no name: documented fallback.
        assert_eq!(t2.hvalue(Atom::from_raw(1234)), 1234);
    }

    #[test]
    fn make_hash2_fast_paths() {
        let t = atoms();
        // nil with a running hash of 0 is the hard-coded C constant.
        assert_eq!(make_hash2(NIL, &t).unwrap(), NIL_HASH);
        // An atom with a running hash of 0 returns `hvalue` directly.
        let abc = t.lookup("abc").unwrap();
        assert_eq!(make_hash2(make_atom(abc), &t).unwrap(), 26499);
        // Small ints: both sides of the 28-bit threshold must compute.
        for v in [
            0i64,
            1,
            -1,
            255,
            1 << 27,
            -(1 << 27),
            1 << 40,
            -(1 << 40),
            MAX_SMALL,
            MIN_SMALL,
        ] {
            assert!(make_hash2(make_small(v), &t).is_ok(), "small {v}");
        }
        // `is_ssmall28` reproduces `~((x >> 27) + 1) < 2`.
        assert!(is_ssmall28(0) && is_ssmall28(-(1 << 27)) && is_ssmall28((1 << 27) - 1));
        assert!(!is_ssmall28(1 << 27) && !is_ssmall28(-(1 << 27) - 1));
    }

    #[test]
    fn make_hash2_byte_lists_pin_the_batching() {
        // One byte-headed cons cell, tail `nil`: `UINT32_HASH(sh, HCONST_4)`
        // then the `nil` non-zero path. Values verified against the system ERTS
        // (`erlang:phash2([B], 1 bsl 32)`).
        let t = atoms();
        let expected = [
            (0i64, 3768589414u32),
            (1, 2768963718),
            (2, 3124094002),
            (3, 1984398927),
            (255, 717906935),
        ];
        for (b, want) in expected {
            let mut heap = Heap::new(8);
            let l = heap.cons(make_small(b), NIL).unwrap();
            assert_eq!(make_hash2(l, &t).unwrap(), want, "list [{b}]");
        }
    }

    #[test]
    fn make_hash2_flat_map_is_pair_order_independent() {
        let t = atoms();
        let ka = make_atom(t.lookup("a").unwrap());
        let kb = make_atom(t.lookup("b").unwrap());
        let kc = make_atom(t.lookup("abc").unwrap());
        let pairs: Vec<(Eterm, Eterm)> = vec![
            (ka, make_small(1)),
            (kb, make_small(2)),
            (kc, make_small(3)),
        ];
        let mut rev = pairs.clone();
        rev.reverse();
        let cap = map_words_capacity(pairs.len());
        let mut h1 = Heap::new(cap + 8);
        let mut h2 = Heap::new(cap + 8);
        // SAFETY: the heaps own the blocks; `cap` words are enough.
        let (m1, m2) = unsafe {
            let p1 = heap_alloc(&mut h1, cap);
            let m1 = write_map(p1, &pairs, &t).unwrap();
            let p2 = heap_alloc(&mut h2, cap);
            let m2 = write_map(p2, &rev, &t).unwrap();
            (m1, m2)
        };
        // The xor protocol makes the pair hash order-independent.
        assert_eq!(make_hash2(m1, &t).unwrap(), make_hash2(m2, &t).unwrap());
        // ... and it still sees the values.
        let mut h3 = Heap::new(cap + 8);
        // SAFETY: as above.
        let m3 = unsafe {
            let p3 = heap_alloc(&mut h3, cap);
            let alt = vec![(ka, make_small(1)), (kb, make_small(9)), (kc, make_small(3))];
            write_map(p3, &alt, &t).unwrap()
        };
        assert_ne!(make_hash2(m1, &t).unwrap(), make_hash2(m3, &t).unwrap());
    }

    /// Allocate `n` words in `heap` (test helper).
    ///
    /// # Safety
    ///
    /// `heap` must outlive the returned block.
    unsafe fn heap_alloc(heap: &mut Heap, n: usize) -> *mut Eterm {
        // SAFETY: the caller keeps the heap alive.
        unsafe { heap.alloc_words(n).unwrap() }
    }
}
