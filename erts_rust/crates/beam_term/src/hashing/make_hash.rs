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

//! The ERTS `make_hash` port (`erlang:phash/2`).
//!
//! Bit-exact u32-wrapping port of `make_hash` in
//! `erts/emulator/beam/erl_term_hashing.c`. `erlang:phash(T, R)` is
//! `1 + (make_hash(T) % R)` for `R < 2^32`, and the raw `make_hash(T) + 1` for
//! `R = 2^32` (`bif.c:5370-5420`). See the `common` submodule for the shared
//! error-handling note and the `make_hash2` submodule for the other port.
//!
//! Two term shapes need care beyond the straight transcription:
//!
//! * a **native record** reaches the definition tuple (the C's `RECORD_DEF` arm
//!   walks `boxed_val(term)+1`, `erl_term_hashing.c:369-378`), whose payload index 1
//!   is the definition's cached `hash` word; that one word is hashed from
//!   [`compute_record_definition_hash`] because `record::write_record` can only
//!   store a placeholder (see the `HOp::RecordInit` arm);
//! * an **internal reference** mixes `internal_ref_numbers(term)[0]`
//!   (`:288-290`) — an id of the node container this tree's M1 `Ref` thing does not
//!   carry — so it is refused with `TermError::RefIdentityUnavailable` rather than
//!   mixed from a different word.

use crate::atom::AtomTable;
use crate::bitstring::float_bits;
use crate::boxed::{arityval, boxed_val, header_subtag};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::fun::{
    fun_env, fun_function, fun_index, fun_kind_of_header, fun_module, fun_num_free, fun_old_uniq,
    fun_true_arity, FunKind,
};
use crate::immediate::{
    atom_val, internal_port_data, is_byte, make_small, pid_number, signed_value, unsigned_value,
};
use crate::list::{is_list, list_val};
use crate::tag::{
    primary_tag, PrimaryTag, ARITYVAL_SUBTAG, EXTERNAL_PID_SUBTAG, EXTERNAL_PORT_SUBTAG,
    EXTERNAL_REF_SUBTAG, FLOAT_SUBTAG, FUN_SUBTAG, HEAP_BITS_SUBTAG, MAP_SUBTAG, NEG_BIG_SUBTAG,
    POS_BIG_SUBTAG, RECORD_SUBTAG, REF_SUBTAG, SUB_BITS_SUBTAG, TAG_IMMED1_IMMED2, TAG_IMMED1_MASK,
    TAG_IMMED1_PID, TAG_IMMED1_PORT, TAG_IMMED1_SMALL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK,
    TAG_IMMED2_NIL,
};
use crate::thing::{big_parts, external_pid_parts, external_port_parts, external_ref_parts};

use super::common::{
    bitstring_parts_any, uint32_hash_step, HItem, HOp, FN1, FN10, FN11, FN12, FN13, FN14, FN15,
    FN2, FN3, FN4, FN5, FN6, FN8, FN9,
};
use super::make_hash2::{compute_record_definition_hash, make_hash2};

/// `hash_binary_bytes(bin, hash)` (`erl_term_hashing.c:114-163`) — `make_hash`'s
/// byte-wise binary hash.
fn hash_binary_bytes(t: Eterm, mut hash: u32) -> TermResult<u32> {
    // SAFETY: `t` is a live bitstring (the caller checked the tag).
    let (ptr, offset, size) = unsafe { bitstring_parts_any(t) };
    let bytesize = size / 8;
    let bitoffs = offset & 7;
    let bitsize = size & 7;
    // SAFETY: the first bit lives in `ptr[offset/8]`.
    let p = unsafe { ptr.add(offset / 8) };
    if bitoffs == 0 {
        for i in 0..bytesize {
            // SAFETY: `i < bytesize` whole bytes are readable.
            hash = hash.wrapping_mul(FN1).wrapping_add(u32::from(unsafe { *p.add(i) }));
        }
        if bitsize > 0 {
            // SAFETY: the partial byte follows the whole bytes.
            let b = u32::from(unsafe { *p.add(bytesize) } >> (8 - bitsize));
            hash = hash
                .wrapping_mul(FN1)
                .wrapping_add(b)
                .wrapping_mul(FN12)
                .wrapping_add(bitsize as u32);
        }
    } else {
        // `Uint previous = *ptr++;`
        let mut previous = u32::from(unsafe { *p });
        // SAFETY: `q` starts at the byte after the first.
        let q = unsafe { p.add(1) };
        let lshift = bitoffs;
        let rshift = 8 - lshift;
        for i in 0..bytesize {
            let mut b = (previous << lshift) & 0xff;
            // SAFETY: `i < bytesize`.
            previous = u32::from(unsafe { *q.add(i) });
            b |= previous >> rshift;
            hash = hash.wrapping_mul(FN1).wrapping_add(b);
        }
        if bitsize > 0 {
            let mut b = (previous << lshift) & 0xff;
            // SAFETY: the partial byte follows the whole bytes.
            previous = u32::from(unsafe { *q.add(bytesize) });
            b |= previous >> rshift;
            b >>= 8 - bitsize;
            hash = hash
                .wrapping_mul(FN1)
                .wrapping_add(b)
                .wrapping_mul(FN12)
                .wrapping_add(bitsize as u32);
        }
    }
    Ok(hash.wrapping_mul(FN4).wrapping_add(bytesize as u32))
}

/// `tag_val_def(term)` restricted to the classes `make_hash` handles.
fn hash_tag_def(t: Eterm) -> TermResult<HOp> {
    match primary_tag(t) {
        PrimaryTag::List => Ok(HOp::List),
        PrimaryTag::Boxed => {
            // SAFETY: `t` is boxed.
            let hdr = unsafe { *boxed_val(t) };
            Ok(match header_subtag(hdr) as usize {
                ARITYVAL_SUBTAG => HOp::TupleInit,
                RECORD_SUBTAG => HOp::RecordInit,
                POS_BIG_SUBTAG | NEG_BIG_SUBTAG => HOp::Big,
                REF_SUBTAG => HOp::Ref,
                FLOAT_SUBTAG => HOp::Float,
                FUN_SUBTAG => HOp::Fun,
                EXTERNAL_PID_SUBTAG => HOp::ExternalPid,
                EXTERNAL_PORT_SUBTAG => HOp::ExternalPort,
                EXTERNAL_REF_SUBTAG => HOp::ExternalRef,
                MAP_SUBTAG => HOp::Map,
                HEAP_BITS_SUBTAG | SUB_BITS_SUBTAG => HOp::Bitstring,
                // `BIN_REF_DEF` is "not a real term" and has no `make_hash` case.
                _ => return Err(TermError::BadFun),
            })
        }
        PrimaryTag::Immed1 => match t.raw() & TAG_IMMED1_MASK {
            m if m == TAG_IMMED1_PID => Ok(HOp::Pid),
            m if m == TAG_IMMED1_PORT => Ok(HOp::Port),
            m if m == TAG_IMMED1_IMMED2 => match t.raw() & TAG_IMMED2_MASK {
                a if a == TAG_IMMED2_ATOM => Ok(HOp::Atom),
                n if n == TAG_IMMED2_NIL => Ok(HOp::Nil),
                _ => Err(TermError::BadFun),
            },
            m if m == TAG_IMMED1_SMALL => Ok(HOp::Small),
            _ => Err(TermError::BadFun),
        },
        PrimaryTag::Header => Err(TermError::NotATerm),
    }
}

/// `make_hash(term)` (`erl_term_hashing.c:165-425`, `erlang:phash/2`).
///
/// `erlang:phash(T, R)` is `1 + (make_hash(T) % R)` for `R < 2^32`, and the raw
/// `make_hash(T) + 1` for `R = 2^32` (`bif.c:5370-5420`).
pub fn make_hash(t: Eterm, atoms: &AtomTable) -> TermResult<u32> {
    let mut hash: u32 = 0;
    let mut stack: Vec<HItem> = Vec::new();
    let mut term = t;
    let mut op = hash_tag_def(term)?;
    // The native-record definition-`hash` override (see the `HOp::RecordInit` arm):
    // the single payload word named here is hashed as the carried value instead of
    // the word actually stored there. The address is unique in the term graph, so
    // the slot fires at most once and is cleared as soon as it does.
    let mut def_hash_patch: Option<(*const Eterm, Eterm)> = None;

    loop {
        match op {
            HOp::Nil => hash = hash.wrapping_mul(FN3).wrapping_add(1),
            HOp::Atom => {
                hash = hash
                    .wrapping_mul(FN1)
                    .wrapping_add(atoms.hvalue(atom_val(term)))
            }
            HOp::Small => {
                let y1 = signed_value(term);
                let y2 = y1.unsigned_abs();
                hash = uint32_hash_step(hash, (y2 & 0xffff_ffff) as u32, FN2);
                if (y2 >> 32) != 0 {
                    hash = uint32_hash_step(hash, (y2 >> 32) as u32, FN2);
                }
                hash = hash.wrapping_mul(if y1 < 0 { FN4 } else { FN3 });
            }
            HOp::Bitstring => hash = hash_binary_bytes(term, hash)?,
            HOp::Fun => {
                // SAFETY: `term` is a fun.
                let hdr = unsafe { *boxed_val(term) };
                match fun_kind_of_header(hdr) {
                    FunKind::Imported => {
                        let num_free = fun_num_free(hdr);
                        hash = hash.wrapping_mul(FN10).wrapping_add(num_free as u32);
                        hash = hash
                            .wrapping_mul(FN1)
                            .wrapping_add(atoms.hvalue(unsafe { fun_module(term)? }));
                        hash = hash
                            .wrapping_mul(FN2)
                            .wrapping_add(unsafe { fun_index(term)? });
                        hash = hash
                            .wrapping_mul(FN2)
                            .wrapping_add(unsafe { fun_old_uniq(term)? });
                        if num_free > 0 {
                            let env = unsafe { fun_env(term)? };
                            if num_free > 1 {
                                stack.push(HItem::Ptr(env.as_ptr().wrapping_add(1)));
                                stack.push(HItem::Count(num_free - 1));
                                stack.push(HItem::Op(HOp::TermArray));
                            }
                            term = env[0];
                            op = hash_tag_def(term)?;
                            continue;
                        }
                    }
                    FunKind::External => {
                        // The C mixes `mfa->arity` — the `Export`'s TRUE arity
                        // (`erl_term_hashing.c:262-269`), widened to `Uint32` there.
                        // SAFETY: `term` is a live external fun.
                        let arity = unsafe { fun_true_arity(term)? };
                        hash = hash.wrapping_mul(FN11).wrapping_add(arity as u32);
                        hash = hash
                            .wrapping_mul(FN1)
                            .wrapping_add(atoms.hvalue(unsafe { fun_module(term)? }));
                        hash = hash
                            .wrapping_mul(FN1)
                            .wrapping_add(atoms.hvalue(unsafe { fun_function(term)? }));
                    }
                    // As in `make_hash2`: a `make_fun3` fun lacks module/`old_uniq`.
                    FunKind::Local => return Err(TermError::FunIdentityUnavailable),
                }
            }
            HOp::Pid | HOp::ExternalPid => {
                let number = if op == HOp::Pid {
                    pid_number(term) as u32
                } else {
                    // SAFETY: the tag says external pid.
                    unsafe { external_pid_parts(term) }.1
                };
                hash = uint32_hash_step(hash, number, FN5);
                hash = hash.wrapping_mul(FN6);
            }
            HOp::Port | HOp::ExternalPort => {
                let number = if op == HOp::Port {
                    internal_port_data(term)
                } else {
                    // SAFETY: the tag says external port.
                    unsafe { external_port_parts(term) }.1
                };
                let low = (number & 0xffff_ffff) as u32;
                let high = (number >> 32) as u32;
                if high != 0 {
                    hash = uint32_hash_step(hash, high, FN11);
                }
                hash = uint32_hash_step(hash, low, FN9);
                hash = hash.wrapping_mul(FN10);
            }
            HOp::Ref | HOp::ExternalRef => {
                let first = if op == HOp::Ref {
                    // The C mixes `internal_ref_numbers(term)[0]`
                    // (`erl_term_hashing.c:288-290`) — the first id of the reference's
                    // **node container**. This tree's M1 on-heap `Ref` thing carries a
                    // single process-local id and no node container, so the oracle's
                    // value (itself environment-dependent) cannot be reproduced.
                    // Refuse rather than mix a wrong number.
                    return Err(TermError::RefIdentityUnavailable);
                } else {
                    // SAFETY: the tag says external reference.
                    // `external_ref_numbers(x)` is `&external_ref_data(x)[1]` — the word
                    // after the id count — so a zero-length reference reads/mixes the
                    // zeroed word there: 0 (`erl_term_hashing.c:290`).
                    unsafe { external_ref_parts(term) }
                        .1
                        .first()
                        .copied()
                        .unwrap_or(0)
                };
                hash = uint32_hash_step(hash, first, FN9);
                hash = hash.wrapping_mul(FN10);
            }
            HOp::Float => {
                // SAFETY: `term` is a float.
                let mut bits = unsafe { float_bits(term) };
                if f64::from_bits(bits) == 0.0 {
                    bits = 0f64.to_bits();
                }
                hash = hash
                    .wrapping_mul(FN6)
                    .wrapping_add(((bits & 0xffff_ffff) as u32) ^ ((bits >> 32) as u32));
            }
            HOp::Map => {
                hash = hash
                    .wrapping_mul(FN13)
                    .wrapping_add(FN14)
                    .wrapping_add(make_hash2(term, atoms)?);
            }
            HOp::TupleInit | HOp::RecordInit => {
                // SAFETY: `term` is a tuple / record instance.
                let base = boxed_val(term);
                // SAFETY: word 0 is the (live) header.
                let arity = arityval(unsafe { *base });
                if op == HOp::RecordInit {
                    // The C `RECORD_DEF` arm (`erl_term_hashing.c:369-378`) walks the
                    // record instance's `[definition_term, v0..v(n-1)]` payload, so the
                    // walk reaches the definition **tuple** — whose payload index 1 is
                    // the definition's cached `hash` word. `records:create/4` stores the
                    // true `make_hash2` value of the definition there
                    // (`erl_record.c:1041-1054`), but this tree's
                    // `record::write_record` can only store a caller-supplied
                    // placeholder, so that one word is hashed from
                    // `compute_record_definition_hash` instead. Everything else about
                    // the walk (the `hash*FN9 + (n+5)` definition-tuple finish, the `n`
                    // values, the `hash*FN15 + (n+1)` record finish) is unchanged.
                    //
                    // SAFETY: word 1 of a record instance is its boxed definition; the
                    // readers validate it (a malformed definition is `BadRecord`
                    // before any pointer arithmetic happens below).
                    let def = unsafe { *base.add(1) };
                    let def_hash = compute_record_definition_hash(def, atoms)?;
                    // SAFETY: a validated definition block is an arityval block whose
                    // payload index 1 (word 2) is the cached `hash` term.
                    def_hash_patch = Some((
                        unsafe { boxed_val(def).add(2) },
                        make_small(i64::from(def_hash)),
                    ));
                }
                stack.push(HItem::Count(arity));
                // SAFETY: an arityval block has `arity` elements at words 1..=arity.
                stack.push(HItem::Ptr(unsafe { base.add(1) }));
                stack.push(HItem::Count(arity));
                op = if op == HOp::TupleInit {
                    HOp::Tuple
                } else {
                    HOp::Record
                };
                continue;
            }
            HOp::Tuple | HOp::Record | HOp::TermArray => {
                let i = pop_count(&mut stack)?;
                let ptr = pop_ptr(&mut stack)?;
                if i != 0 {
                    // SAFETY: `ptr` is inside a live term array with `i` elements.
                    term = unsafe { *ptr };
                    // The record-definition `hash` override (see `HOp::RecordInit`).
                    if let Some((slot, value)) = def_hash_patch {
                        if slot == ptr {
                            term = value;
                            def_hash_patch = None;
                        }
                    }
                    // SAFETY: one element fewer from the next word.
                    stack.push(HItem::Ptr(unsafe { ptr.add(1) }));
                    stack.push(HItem::Count(i - 1));
                    stack.push(HItem::Op(op));
                    op = hash_tag_def(term)?;
                    continue;
                }
                if op == HOp::Tuple {
                    let arity = pop_count(&mut stack)?;
                    hash = hash.wrapping_mul(FN9).wrapping_add(arity as u32);
                } else if op == HOp::Record {
                    let arity = pop_count(&mut stack)?;
                    hash = hash.wrapping_mul(FN15).wrapping_add(arity as u32);
                }
            }
            HOp::CdrPost => hash = hash.wrapping_mul(FN8),
            HOp::CdrPre | HOp::List => {
                if op == HOp::CdrPre {
                    let t2 = pop_term_item(&mut stack)?;
                    if !is_list(t2) {
                        stack.push(HItem::Op(HOp::CdrPost));
                        term = t2;
                        op = hash_tag_def(term)?;
                        continue;
                    }
                    term = t2;
                }
                // SAFETY: `term` is a cons cell.
                let mut list = list_val(term);
                let mut recur_tail = None;
                loop {
                    // SAFETY: `list` is a live cons cell.
                    let head = unsafe { *list };
                    if !is_byte(head) {
                        break;
                    }
                    // "Note that this hash is different from a 'small' hash."
                    hash = hash
                        .wrapping_mul(FN2)
                        .wrapping_add(unsigned_value(head) as u32);
                    // SAFETY: word 1 is the CDR.
                    let tl = unsafe { *list.add(1) };
                    if !is_list(tl) {
                        recur_tail = Some(tl);
                        break;
                    }
                    // SAFETY: `tl` is a cons cell.
                    list = list_val(tl);
                }
                if let Some(tl) = recur_tail {
                    stack.push(HItem::Op(HOp::CdrPost));
                    term = tl;
                } else {
                    // SAFETY: word 1 is the CDR (the non-byte tail).
                    stack.push(HItem::Term(unsafe { *list.add(1) }));
                    stack.push(HItem::Op(HOp::CdrPre));
                    // SAFETY: word 0 is the CAR.
                    term = unsafe { *list };
                }
                op = hash_tag_def(term)?;
                continue;
            }
            HOp::Big => {
                // SAFETY: the tag says bignum.
                let (neg, digits) = unsafe { big_parts(term) };
                let n = digits.len();
                for &limb in &digits[..n - 1] {
                    let mut d = limb;
                    for _ in 0..8 {
                        hash = hash.wrapping_mul(FN2).wrapping_add((d & 0xff) as u32);
                        d >>= 8;
                    }
                }
                let mut d = digits[n - 1];
                let klimbs = if (d >> 32) == 0 { 4 } else { 8 };
                for _ in 0..klimbs {
                    hash = hash.wrapping_mul(FN2).wrapping_add((d & 0xff) as u32);
                    d >>= 8;
                }
                hash = hash.wrapping_mul(if neg { FN4 } else { FN3 });
            }
        }

        // `if (WSTACK_ISEMPTY(stack)) break; op = WSTACK_POP(stack);`
        if stack.is_empty() {
            return Ok(hash);
        }
        op = pop_op(&mut stack)?;
    }
}

/// Pop an operation continuation.
fn pop_op(stack: &mut Vec<HItem>) -> TermResult<HOp> {
    match stack.pop() {
        Some(HItem::Op(op)) => Ok(op),
        _ => Err(TermError::BadFun),
    }
}

/// Pop an element count.
fn pop_count(stack: &mut Vec<HItem>) -> TermResult<usize> {
    match stack.pop() {
        Some(HItem::Count(n)) => Ok(n),
        _ => Err(TermError::BadFun),
    }
}

/// Pop an element pointer.
fn pop_ptr(stack: &mut Vec<HItem>) -> TermResult<*const Eterm> {
    match stack.pop() {
        Some(HItem::Ptr(p)) => Ok(p),
        _ => Err(TermError::BadFun),
    }
}

/// Pop a term.
fn pop_term_item(stack: &mut Vec<HItem>) -> TermResult<Eterm> {
    match stack.pop() {
        Some(HItem::Term(t)) => Ok(t),
        _ => Err(TermError::BadFun),
    }
}

#[cfg(test)]
mod tests {
    //! `make_hash` vectors plus the cross-port consistency checks. The
    //! end-to-end cross-check against the real runtime lives in `tests/oracle.rs`.

    use super::*;
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, MAX_SMALL, MIN_SMALL, NIL};
    use crate::list::{car, cdr};
    use crate::map::{map_words_capacity, write_map};

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "abc", "b"]);
        t
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

    #[test]
    fn make_hash_pins_the_nil_small_and_atom_paths() {
        let t = atoms();
        // `nil`: `hash = 0*FUNNY_NUMBER3 + 1` = 1.
        assert_eq!(make_hash(NIL, &t).unwrap(), 1);
        // `0`: every `UINT32_HASH_STEP` keeps 0, and `0 * FUNNY_NUMBER3` is 0.
        assert_eq!(make_hash(make_small(0), &t).unwrap(), 0);
        // An atom adds only `hvalue` to `hash * FUNNY_NUMBER1`.
        let abc = t.lookup("abc").unwrap();
        assert_eq!(make_hash(make_atom(abc), &t).unwrap(), 26499);
        // A list of bytes: `... * FUNNY_NUMBER2 + byte`, then `* FUNNY_NUMBER8`
        // for the non-list tail and `* FUNNY_NUMBER3 + 1` for `nil`.
        let mut heap = Heap::new(8);
        let l = heap.cons(make_small(1), NIL).unwrap();
        let want = (1u32
            .wrapping_mul(FN3)
            .wrapping_add(1))
        .wrapping_mul(FN8);
        assert_eq!(make_hash(l, &t).unwrap(), want);
        // The list really is [1].
        // SAFETY: `l` is a live cons cell.
        unsafe {
            assert_eq!(car(l), make_small(1));
            assert_eq!(cdr(l), NIL);
        }
    }

    /// Oracle-pinned `make_hash2` / `make_hash` vectors, `(term, make_hash2,
    /// make_hash)`.
    ///
    /// Every value was read out of the system ERTS and is asserted again against
    /// the live oracle by
    /// `tests/oracle.rs::oracle_agrees_on_make_hash2_and_make_hash`; pinning them
    /// here keeps the contract testable on a host with no `erl` on `PATH`. The
    /// corpus spans the paths that actually differ: the 28-bit small fast path,
    /// atoms (incl. the empty atom), `nil`, tuples, byte- and non-byte lists,
    /// improper lists, flat maps, bitstrings (empty / non-byte-aligned / a whole
    /// 12-byte block), floats (incl. `+0.0`), multi-limb bignums, and an external
    /// fun.
    #[test]
    fn make_hash2_and_make_hash_pin_the_oracle_vectors() {
        use crate::bitstring::{heap_bits_size_words, write_heap_bits};
        use crate::fun::{fun_words, write_fun_external};
        use crate::thing::{big_size_words, write_big, write_float, FLOAT_SIZE_WORDS};
        use crate::tuple::tuple0;

        let mut atoms = AtomTable::new();
        atoms.seed(["", "abc", "lists", "sort"]);
        let empty_atom = make_atom(atoms.lookup("").unwrap());
        let abc = make_atom(atoms.lookup("abc").unwrap());

        let mut heap = Heap::new(256);
        let t12 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let c3 = heap.cons(make_small(3), NIL).unwrap();
        let c23 = heap.cons(make_small(2), c3).unwrap();
        let l123 = heap.cons(make_small(1), c23).unwrap();
        let c99 = heap.cons(make_small(99), NIL).unwrap();
        let c9899 = heap.cons(make_small(98), c99).unwrap();
        let l_bytes = heap.cons(make_small(97), c9899).unwrap();
        let c_imp_tail = heap.cons(make_small(2), make_small(3)).unwrap();
        let l_imp = heap.cons(make_small(1), c_imp_tail).unwrap();

        // SAFETY: every block below is allocated from `heap`, which outlives the
        // test, with exactly the words the writer reserves.
        let (map_empty, map_12) = unsafe {
            let p = heap_alloc(&mut heap, map_words_capacity(0));
            let map_empty = write_map(p, &[], &atoms).unwrap();
            let p = heap_alloc(&mut heap, map_words_capacity(1));
            let map_12 = write_map(p, &[(make_small(1), make_small(2))], &atoms).unwrap();
            (map_empty, map_12)
        };
        // SAFETY: as above.
        let (bits_empty, bits3, bin12) = unsafe {
            let p = heap_alloc(&mut heap, heap_bits_size_words(0));
            let bits_empty = write_heap_bits(p, &[], 0).unwrap();
            let p = heap_alloc(&mut heap, heap_bits_size_words(3));
            let bits3 = write_heap_bits(p, &[0x20], 3).unwrap();
            let b12: [u8; 12] = [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0];
            let p = heap_alloc(&mut heap, heap_bits_size_words(96));
            let bin12 = write_heap_bits(p, &b12, 96).unwrap();
            (bits_empty, bits3, bin12)
        };
        // SAFETY: as above.
        let (f_zero, f_pi) = unsafe {
            let p = heap_alloc(&mut heap, FLOAT_SIZE_WORDS);
            let f_zero = write_float(p, 0.0);
            let p = heap_alloc(&mut heap, FLOAT_SIZE_WORDS);
            let f_pi = write_float(p, 3.141_592_653_589_793);
            (f_zero, f_pi)
        };
        // SAFETY: as above.
        let (big_pos, big_neg) = unsafe {
            let p = heap_alloc(&mut heap, big_size_words(2));
            let big_pos = write_big(p, false, &[0, 1]); // 2^64
            let p = heap_alloc(&mut heap, big_size_words(2));
            let big_neg = write_big(p, true, &[0, 1]); // -2^64
            (big_pos, big_neg)
        };
        // SAFETY: as above.
        let fun_sort = unsafe {
            let p = heap_alloc(&mut heap, fun_words(FunKind::External, 0));
            write_fun_external(
                p,
                1,
                atoms.lookup("lists").unwrap(),
                atoms.lookup("sort").unwrap(),
            )
            .unwrap()
        };

        let vectors: [(Eterm, u32, u32); 25] = [
            (make_small(0), 3_175_731_469, 0),
            (make_small(1), 539_485_162, 2_788_898_427),
            (make_small(-1), 1_117_813_597, 1_680_185_269),
            (make_small(1 << 27), 2_562_491_755, 2_147_483_672),
            (make_small(-(1 << 27)), 874_979_335, 2_147_489_128),
            (make_small(MAX_SMALL), 1_638_133_000, 3_114_282_912),
            (make_small(MIN_SMALL), 2_316_875_190, 2_147_489_128),
            (empty_atom, 0, 0),
            (abc, 26_499, 26_499),
            (NIL, 3_468_870_702, 1),
            (tuple0(), 221_703_996, 0),
            (t12, 4_118_095_950, 3_114_748_239),
            (l123, 4_186_538_188, 3_336_869_157),
            (l_bytes, 519_996_486, 3_654_580_165),
            (l_imp, 4_169_601_349, 265_598_414),
            (map_empty, 844_985_373, 1_113_425_984),
            (map_12, 1_248_490_261, 1_516_930_872),
            (bits_empty, 147_926_629, 0),
            (bits3, 3_965_351_140, 1_345_689_960),
            (bin12, 3_273_225_850, 1_369_995_014),
            (f_zero, 423_528_920, 0),
            (f_pi, 764_465_415, 340_593_891),
            (big_pos, 2_519_041_713, 2_788_898_427),
            (big_neg, 2_563_594_619, 1_680_185_269),
            (fun_sort, 2_047_717_521, 3_566_694_934),
        ];

        for (term, want2, want) in vectors {
            assert_eq!(
                make_hash2(term, &atoms).unwrap(),
                want2,
                "make_hash2({term:?})"
            );
            assert_eq!(make_hash(term, &atoms).unwrap(), want, "make_hash({term:?})");
        }
    }

    #[test]
    fn make_hash_and_make_hash2_disagree_for_the_same_term() {
        // Two different algorithms; only `phash2` equals `make_hash2`.
        let t = atoms();
        let mut heap = Heap::new(8);
        let l = heap.cons(make_small(200), NIL).unwrap();
        let h2 = make_hash2(l, &t).unwrap();
        let h = make_hash(l, &t).unwrap();
        assert_ne!(h2, h);
    }
}
