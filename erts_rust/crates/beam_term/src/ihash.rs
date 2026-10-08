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

//! The ERTS *internal* term hash — `make_internal_hash` / `erts_map_hash`.
//!
//! This is the hash that drives the **map** HAMT (see [`crate::map`]): ERTS places
//! a key at level `L` of the trie according to the 4-bit nibble
//! `(erts_map_hash(key) >> (4*L)) & 0xF` (`hashmap_index`, `erl_map.h:219`). To make
//! a `write_map`-built map store its pairs in the *same order* as the oracle — which
//! is what a byte-exact ETF image of a >32-key map needs — this module reproduces
//! `erts_map_hash` **bit-exactly**, unlike [`crate::hashing::term_hash`] (this crate's
//! own structural hash, whose exact values are explicitly *not* a compatibility
//! contract).
//!
//! Ground truth: `erts/emulator/beam/erl_term_hashing.c` — the `IHASH_TYPE_*` enum
//! (`1636-1657`), the `IHASH_MIX_ALPHA`/`IHASH_MIX_BETA`/`…_2F32`/`IHASH_MIX_IMMEDIATE`/
//! `IHASH_PUSH_TERM` macros (`1660-1727`), `read_u64` (`1732-1745`), `ihash_mix64`
//! (`1747-1767`), `make_internal_hash` (`1769-2255`) and
//! `erts_internal_{,salted_}hash`/`erts_map_hash` (`2292-2319`).
//!
//! # What is reproduced exactly
//!
//! The walk is the C's: an explicit work stack whose entries are either a pending
//! *term* or the `CAR`/`CDR` marker that the C pushes for a cons cell whose head is
//! not an immediate (`IHASH_CAR_MARKER`/`IHASH_CDR_MARKER` are `_make_header(1|2,
//! _TAG_HEADER_REF)`; this port uses typed stack variants instead of the raw marker
//! words, which is behaviourally identical because a *term* word never has the
//! `TAG_PRIMARY_HEADER` tag). For an **immediate** the C short-circuits to
//! `ihash_mix64(term)` on the raw `Eterm` word (`erts_internal_hash`'s fast path), so
//! small integers and atoms hash from their raw word — reproduced here verbatim.
//!
//! Verified against the live oracle by `tests/map_hash.rs`: the storage order of
//! `write_map`-built maps with 33/40/50/64/100 small-integer and binary keys equals
//! `maps:to_list/1` of the same map under the system `erl`.
//!
//! # Documented deviations (values the C derives from a *pointer*)
//!
//! The C's hash is explicitly *not* portable between VM instances (its own comment
//! "Limitation #1"), because it mixes raw pointers. This runtime has no such pointers,
//! so the following cases cannot be bit-exact and use the closest deterministic
//! substitute instead:
//!
//! * **External fun** (`IHASH_TYPE_EXTERNAL_FUN`): the C mixes `(UWord)funp->entry.exp`,
//!   the address of the canonical `Export`. Here the `External` fun's descriptor holds
//!   its `module`/`function` atoms (the C `entry` pointer is inlined at M1), so those
//!   two atoms are mixed instead.
//! * **External pid/port/ref** (`IHASH_TYPE_EXTERNAL_*`): the C mixes `(UWord)thing->node`,
//!   the `ErlNode*`. Here the `ExternalThing`'s `node` slot holds an atom *term*, whose
//!   raw word is mixed instead.
//! * **Local fun** (`IHASH_TYPE_LOCAL_FUN`): the C mixes the out-of-line
//!   `ErlFunEntry`'s `index` and `old_uniq`. The inlined `Local` descriptor keeps
//!   `[uniq: u64][index: u32]` and no `old_uniq`; `index` is mixed and `old_uniq` is
//!   taken as `0` (its value for a `make_fun3`-built fun). An `Imported` fun (the
//!   `NEW_FUN_EXT` image) *does* carry `index` **and** `old_uniq`, so it is hashed
//!   with both, exactly as the C's local-fun branch does.
//! * **Internal ref** (`IHASH_TYPE_LOCAL_REF`): the 64-bit little-endian
//!   `ErtsORefThing`/`ErtsPRefThing` layout is assumed when extracting the `Uint32`
//!   number array. This runtime creates no internal references, so the path is
//!   unverifiable here. A ref thing whose arity is neither the ordinary (3-word) nor
//!   the pid (4-word) size falls back to mixing its raw payload words.
//! * **`read_u64`** is endian-agnostic in the C but produces *different* hash values on
//!   big- and little-endian hosts; the little-endian value is used (as on the oracle's
//!   x86-64).
//!
//! The classes that matter for map keys (immediates/atoms/small integers, tuples,
//! lists, flat maps, hash maps, and binaries/bitstrings) are bit-exact.

use crate::bitstring::bit_count_to_bytes;
use crate::boxed::{boxed_val, header_arity, header_subtag, is_boxed_term, map_header_val};
use crate::eterm::Eterm;
use crate::fun::{fun_desc_words, fun_kind_of_header, fun_num_free, FunKind};
use crate::immediate::{is_byte, is_immediate, unsigned_value};
use crate::list::{is_list, list_val};
use crate::tag::{
    primary_tag, PrimaryTag, ARITYVAL_SUBTAG, BIN_REF_SUBTAG, EXTERNAL_PID_SUBTAG,
    EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG, FLOAT_SUBTAG, FUN_SUBTAG, HAMT_SUBTAG_HEAD_ARRAY,
    HAMT_SUBTAG_HEAD_BITMAP, HAMT_SUBTAG_HEAD_FLATMAP, HAMT_SUBTAG_NODE_BITMAP,
    HEADER_MAP_SUBTAG_MASK, HEAP_BITS_SUBTAG, MAP_SUBTAG, NEG_BIG_SUBTAG, POS_BIG_SUBTAG,
    RECORD_SUBTAG, REF_SUBTAG, SUB_BITS_SUBTAG,
};
use crate::thing::{big_parts, flatmap_parts};

// ---------------------------------------------------------------------------
// The mixing primitive
// ---------------------------------------------------------------------------

/// `IHASH_C1` (`erl_term_hashing.c:1663`).
const IHASH_C1: u64 = 0x87C3_7B91_1142_53D5;
/// `IHASH_C2` (`erl_term_hashing.c:1664`).
const IHASH_C2: u64 = 0x4CF5_AD43_2745_937F;

// The `IHASH_TYPE_*` discriminators (`erl_term_hashing.c:1636-1657`). The C enum
// starts at `1`, so these values are part of the hash input.
const IHASH_TYPE_IMMEDIATE: u64 = 1;
const IHASH_TYPE_ARRAY_ELEMENT: u64 = 2;
const IHASH_TYPE_CAR: u64 = 3;
const IHASH_TYPE_CDR: u64 = 4;
const IHASH_TYPE_STRING: u64 = 5;
const IHASH_TYPE_TUPLE: u64 = 6;
const IHASH_TYPE_RECORD: u64 = 7;
const IHASH_TYPE_FLATMAP: u64 = 8;
const IHASH_TYPE_HASHMAP_HEAD_ARRAY: u64 = 9;
const IHASH_TYPE_HASHMAP_HEAD_BITMAP: u64 = 10;
const IHASH_TYPE_HASHMAP_NODE: u64 = 11;
const IHASH_TYPE_BINARY: u64 = 12;
const IHASH_TYPE_LOCAL_FUN: u64 = 13;
const IHASH_TYPE_EXTERNAL_FUN: u64 = 14;
const IHASH_TYPE_NEG_BIGNUM: u64 = 15;
const IHASH_TYPE_POS_BIGNUM: u64 = 16;
const IHASH_TYPE_LOCAL_REF: u64 = 17;
const IHASH_TYPE_EXTERNAL_REF: u64 = 18;
const IHASH_TYPE_EXTERNAL_PID: u64 = 19;
const IHASH_TYPE_EXTERNAL_PORT: u64 = 20;
const IHASH_TYPE_FLOAT: u64 = 21;

/// `ROTL64` (`erl_term_hashing.c:1662`).
#[inline]
const fn rotl64(x: u64, y: u32) -> u64 {
    (x << y) | (x >> (64 - y))
}

/// `ihash_mix64` (`erl_term_hashing.c:1747-1767`) — the MurmurHash3-64 finalizer.
///
/// This is also the *entire* hash of an immediate (`erts_internal_hash`'s fast path).
#[inline]
pub fn ihash_mix64(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    hash ^= hash >> 33;
    hash
}

/// The `hash_alpha`/`hash_beta`/`hash_ticks` accumulator of `make_internal_hash`.
#[derive(Clone, Copy)]
struct IHash {
    alpha: u64,
    beta: u64,
    ticks: u64,
}

impl IHash {
    #[inline]
    const fn new(salt: u64) -> Self {
        IHash {
            alpha: salt,
            beta: salt,
            ticks: 0,
        }
    }

    /// `IHASH_MIX_ALPHA(Expr)`.
    #[inline]
    fn mix_alpha(&mut self, expr: u64) {
        let mut e = expr;
        e = e.wrapping_mul(IHASH_C1);
        e = rotl64(e, 31);
        e = e.wrapping_mul(IHASH_C2);
        self.alpha ^= e;
        self.alpha = rotl64(self.alpha, 27);
        self.alpha = self.alpha.wrapping_add(self.beta);
        self.alpha = self.alpha.wrapping_mul(5).wrapping_add(0x52DC_E729);
        self.ticks += 1;
    }

    /// `IHASH_MIX_BETA(Expr)`.
    #[inline]
    fn mix_beta(&mut self, expr: u64) {
        let mut e = expr;
        e = e.wrapping_mul(IHASH_C2);
        e = rotl64(e, 33);
        e = e.wrapping_mul(IHASH_C1);
        self.beta ^= e;
        self.beta = rotl64(self.beta, 31);
        self.beta = self.beta.wrapping_add(self.alpha);
        self.beta = self.beta.wrapping_mul(5).wrapping_add(0x3849_5AB5);
        self.ticks += 1;
    }

    /// `IHASH_MIX_ALPHA_2F32(Expr1, Expr2)` — `(u64)Expr1 | ((u64)Expr2 << 32)`.
    #[inline]
    fn mix_alpha_2f32(&mut self, lo: u32, hi: u32) {
        self.mix_alpha(u64::from(lo) | (u64::from(hi) << 32));
    }

    /// `IHASH_MIX_BETA_2F32(Expr1, Expr2)`.
    #[inline]
    fn mix_beta_2f32(&mut self, lo: u32, hi: u32) {
        self.mix_beta(u64::from(lo) | (u64::from(hi) << 32));
    }

    /// `IHASH_MIX_IMMEDIATE(term)` (the `ARCH_64` form).
    #[inline]
    fn mix_immediate(&mut self, term: u64) {
        self.mix_alpha(IHASH_TYPE_IMMEDIATE);
        self.mix_beta(term);
    }

    /// The finalization block at `pop_next` (`erl_term_hashing.c:2220-2237`).
    #[inline]
    fn finish(self) -> u64 {
        let mut a = self.alpha ^ self.ticks;
        let mut b = self.beta ^ self.ticks;
        a = a.wrapping_add(b);
        b = b.wrapping_add(a);
        a = ihash_mix64(a);
        b = ihash_mix64(b);
        a = a.wrapping_add(b);
        b = b.wrapping_add(a);
        a ^ b
    }
}

// ---------------------------------------------------------------------------
// The work stack
// ---------------------------------------------------------------------------

/// One entry of the C's `ESTACK` in `make_internal_hash`: a pending term, or the
/// `IHASH_CDR_MARKER` sentinel that contributes `IHASH_TYPE_CDR` to `hash_beta`.
///
/// The C also defines an `IHASH_CAR_MARKER`, but never pushes it (the head of an
/// improper cons cell is hashed as the new current term instead), so there is no
/// `Car` variant here.
#[derive(Clone, Copy)]
enum Item {
    Term(Eterm),
    Cdr,
}

/// `IHASH_PUSH_TERM(stack, term)`: an immediate is mixed up-front (a fast path for
/// `{atom(), immed()}` keys), anything else is deferred onto the stack — after mixing
/// the *presence* of a non-immediate so that terms with a different internal order
/// hash differently.
#[inline]
fn push_term(h: &mut IHash, stack: &mut Vec<Item>, term: Eterm) {
    if is_immediate(term) {
        h.mix_immediate(term.raw() as u64);
    } else {
        h.mix_alpha(IHASH_TYPE_ARRAY_ELEMENT);
        stack.push(Item::Term(term));
    }
}

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// `make_internal_hash(term, salt)` (`erl_term_hashing.c:1769-2255`) — the full
/// structural walk. `term` must be a boxed or list term (the C entry points
/// short-circuit immediates before calling this).
///
/// # Safety
///
/// `term` must be a live, well-formed term whose boxed words are all initialised —
/// the same caller invariant [`crate::compare::cmp`] and [`crate::size::size_object`]
/// carry. A malformed interior (e.g. a record whose definition word is not boxed)
/// cannot occur for a term this runtime produced; where a value is unreadable this
/// port falls back to a documented deterministic substitute rather than aborting.
fn make_internal_hash(term: Eterm, salt: u64) -> u64 {
    let mut h = IHash::new(salt);
    let mut stack: Vec<Item> = Vec::new();
    let mut term = term;

    'outer: loop {
        match primary_tag(term) {
            PrimaryTag::List => {
                // The C's string batching: runs of byte-valued heads are folded four
                // at a time into one `IHASH_TYPE_STRING` mix.
                let mut value: u64 = 0;
                let mut bytes: u32 = 0;
                while is_list(term) {
                    // SAFETY: `term` is a live cons cell, so its two words are live.
                    let cell = list_val(term);
                    let (head, tail) = unsafe { (*cell, *cell.add(1)) };
                    if !is_byte(head) {
                        break;
                    }
                    value = (value << 8) | unsigned_value(head);
                    bytes += 1;
                    if bytes % 4 == 0 {
                        h.mix_alpha_2f32(
                            (IHASH_TYPE_STRING | (u64::from(bytes) << 8)) as u32,
                            value as u32,
                        );
                        value = 0;
                        bytes = 0;
                    }
                    term = tail;
                }
                if bytes > 0 {
                    h.mix_alpha_2f32(
                        (IHASH_TYPE_STRING | (u64::from(bytes) << 8)) as u32,
                        value as u32,
                    );
                }
                if is_list(term) {
                    // SAFETY: `term` is a live cons cell.
                    let cell = list_val(term);
                    let (head, tail) = unsafe { (*cell, *cell.add(1)) };
                    if is_immediate(head) {
                        h.mix_alpha_2f32(IHASH_TYPE_IMMEDIATE as u32, IHASH_TYPE_CAR as u32);
                        h.mix_beta(head.raw() as u64);
                        if !is_list(tail) {
                            h.mix_alpha(IHASH_TYPE_CDR);
                        }
                        term = tail;
                    } else {
                        stack.push(Item::Term(tail));
                        if !is_list(tail) {
                            stack.push(Item::Cdr);
                        }
                        h.mix_alpha(IHASH_TYPE_CAR);
                        term = head;
                    }
                }
                // The C's LIST case always `continue`s: `term` is re-dispatched.
                continue 'outer;
            }
            PrimaryTag::Boxed => {
                // SAFETY: `term` is a live boxed term, so `boxed_val(term)` points at
                // its live, initialised header word.
                let bp = boxed_val(term);
                let hdr = unsafe { *bp };
                if let Some(next) = hash_boxed(&mut h, &mut stack, term, bp, hdr) {
                    term = next;
                    continue 'outer;
                }
            }
            PrimaryTag::Immed1 => {
                h.mix_immediate(term.raw() as u64);
            }
            PrimaryTag::Header => {
                // A bare header word is not a value; the C aborts. Treat it as an
                // immediate so the walk stays total (this cannot occur for a term this
                // runtime produced).
                h.mix_immediate(term.raw() as u64);
            }
        }

        // `pop_next:` — the next pending item, or the finalization block.
        loop {
            match stack.pop() {
                None => return h.finish(),
                Some(Item::Cdr) => h.mix_beta(IHASH_TYPE_CDR),
                Some(Item::Term(next)) => {
                    term = next;
                    continue 'outer;
                }
            }
        }
    }
}

/// The `TAG_PRIMARY_BOXED` arm of `make_internal_hash`.
///
/// Returns `Some(next)` when the C `continue`s with a new current term, `None` when
/// it falls through to `pop_next`.
fn hash_boxed(
    h: &mut IHash,
    stack: &mut Vec<Item>,
    term: Eterm,
    bp: *mut Eterm,
    hdr: Eterm,
) -> Option<Eterm> {
    let subtag = header_subtag(hdr) as usize;
    match subtag {
        RECORD_SUBTAG => {
            // `RECORD_INST_SIZE(n) == 2 + n`, so `header_arity == n + 1`.
            let arity = header_arity(hdr);
            h.mix_alpha(IHASH_TYPE_RECORD);
            h.mix_beta(arity as u64);
            // Word 1 is the boxed record definition; its word 2 is the field hash
            // (`ErtsRecordDefinition.hash`, `erl_record.h:43-61`).
            // SAFETY: an instance's word 1 is a live boxed definition pointer.
            let def = unsafe { *bp.add(1) };
            let def_hash = if is_boxed_term(def) {
                // SAFETY: `def` is boxed, so it points at a live `ErtsRecordDefinition`
                // whose word 2 is the hash.
                unsafe { *boxed_val(def).add(2) }
            } else {
                def
            };
            push_term(h, stack, def_hash);
            if arity > 1 {
                for i in 2..arity {
                    // SAFETY: word `i < arity` is within the instance payload.
                    push_term(h, stack, unsafe { *bp.add(i) });
                }
                // SAFETY: word `arity` is the last field value.
                return Some(unsafe { *bp.add(arity) });
            }
            None
        }
        ARITYVAL_SUBTAG => {
            let arity = header_arity(hdr);
            h.mix_alpha(IHASH_TYPE_TUPLE);
            h.mix_beta(arity as u64);
            if arity > 0 {
                for i in 1..arity {
                    // SAFETY: word `i < arity` is within the tuple payload.
                    push_term(h, stack, unsafe { *bp.add(i) });
                }
                // SAFETY: word `arity` is the last tuple element.
                return Some(unsafe { *bp.add(arity) });
            }
            None
        }
        MAP_SUBTAG => hash_map_body(h, stack, term, bp, hdr),
        FUN_SUBTAG => hash_fun(h, stack, bp, hdr),
        HEAP_BITS_SUBTAG | SUB_BITS_SUBTAG | BIN_REF_SUBTAG => {
            // SAFETY: `term` is a live bitstring (heap bits, a sub-bits window or the
            // `ErlSubBits` root of a refc binary).
            let (data, offset_bits, size_bits) = unsafe { crate::bitstring::bitstring_parts(term) };
            // SAFETY: `data` is the term's payload base and holds `ceil((offset+size)/8)`
            // readable bytes.
            hash_binary(h, data, offset_bits, size_bits);
            None
        }
        POS_BIG_SUBTAG | NEG_BIG_SUBTAG => {
            // SAFETY: `term` is a live bignum.
            let (negative, digits) = unsafe { big_parts(term) };
            let n = digits.len() as u32;
            h.mix_alpha_2f32(
                if negative {
                    IHASH_TYPE_NEG_BIGNUM as u32
                } else {
                    IHASH_TYPE_POS_BIGNUM as u32
                },
                n,
            );
            let mut i = 0usize;
            while i + 2 <= digits.len() {
                h.mix_alpha(digits[i]);
                h.mix_beta(digits[i + 1]);
                i += 2;
            }
            if i < digits.len() {
                h.mix_beta(digits[i]);
            }
            None
        }
        REF_SUBTAG => {
            hash_internal_ref(h, bp, hdr);
            None
        }
        EXTERNAL_REF_SUBTAG | EXTERNAL_PID_SUBTAG | EXTERNAL_PORT_SUBTAG => {
            hash_external_thing(h, term, subtag);
            None
        }
        FLOAT_SUBTAG => {
            h.mix_alpha(IHASH_TYPE_FLOAT);
            // `IHASH_MIX_BETA_2F32(ff.fw[0], ff.fw[1])` on a little-endian host is the
            // double's bit pattern.
            // SAFETY: `term` is a live boxed float.
            h.mix_beta(unsafe { crate::bitstring::float_bits(term) });
            None
        }
        _ => {
            // An unknown boxed subtag is not a term this runtime produces; hash it as
            // an opaque immediate so the walk stays total (the C aborts here).
            h.mix_immediate(hdr.raw() as u64);
            None
        }
    }
}

/// The `MAP_SUBTAG` arm: flat maps, and hash-map head/node nodes.
fn hash_map_body(
    h: &mut IHash,
    stack: &mut Vec<Item>,
    term: Eterm,
    bp: *mut Eterm,
    hdr: Eterm,
) -> Option<Eterm> {
    let map_subtag = hdr.raw() & HEADER_MAP_SUBTAG_MASK;
    if map_subtag == HAMT_SUBTAG_HEAD_FLATMAP {
        // SAFETY: `term` is a live flat map.
        let (keys, values) = unsafe { flatmap_parts(term) };
        let size = keys.len();
        h.mix_alpha(IHASH_TYPE_FLATMAP);
        h.mix_beta(size as u64);
        if size > 0 {
            for i in 0..size - 1 {
                push_term(h, stack, values[i]);
                push_term(h, stack, keys[i]);
            }
            push_term(h, stack, values[size - 1]);
            return Some(keys[size - 1]);
        }
        return None;
    }

    // A hash-map node. `elements` starts at the head's `size` word (word 1) for the
    // array/bitmap heads, and at the first slot (word 1) for a node bitmap.
    let slots_off;
    match map_subtag {
        HAMT_SUBTAG_HEAD_ARRAY => {
            // SAFETY: word 1 of a live head node is its size word.
            let size = unsafe { *bp.add(1) };
            h.mix_alpha(IHASH_TYPE_HASHMAP_HEAD_ARRAY);
            h.mix_beta(size.raw() as u64);
            if size.raw() == 0 {
                return None;
            }
            slots_off = 2;
        }
        HAMT_SUBTAG_HEAD_BITMAP => {
            // SAFETY: word 1 of a live head node is its size word.
            let size = unsafe { *bp.add(1) };
            h.mix_alpha(IHASH_TYPE_HASHMAP_HEAD_BITMAP);
            h.mix_beta(size.raw() as u64);
            if size.raw() == 0 {
                return None;
            }
            slots_off = 2;
        }
        HAMT_SUBTAG_NODE_BITMAP => {
            h.mix_alpha(IHASH_TYPE_HASHMAP_NODE);
            slots_off = 1;
        }
        _ => {
            // Not a matchable map header; treat as opaque to stay total.
            h.mix_immediate(hdr.raw() as u64);
            return None;
        }
    }

    let nslots = if map_subtag == HAMT_SUBTAG_HEAD_ARRAY {
        // `HAMT_HEAD_ARRAY_SZ(18)`: always 16 slots for the array head.
        crate::map::HAMT_MAX_LEVEL
    } else {
        map_header_val(hdr).count_ones() as usize
    };
    for i in 0..nslots {
        // SAFETY: slot `i` is within the node's payload.
        let item = unsafe { *bp.add(slots_off + i) };
        if is_list(item) {
            // SAFETY: a leaf slot is a live `[Key, Value]` cons cell.
            let cell = list_val(item);
            let (key, value) = unsafe { (*cell, *cell.add(1)) };
            push_term(h, stack, value);
            push_term(h, stack, key);
        } else {
            // A child or collision node: the C `ESTACK_PUSH`es it **directly** (no
            // `IHASH_TYPE_ARRAY_ELEMENT` presence mix, which only `IHASH_PUSH_TERM`
            // emits). A collision node is a tuple, so it is hashed as one.
            stack.push(Item::Term(item));
        }
    }
    None
}

/// The `FUN_SUBTAG` arm. See the module docs for the documented deviations on the
/// fun kinds whose C hash derives from an out-of-line `entry` pointer.
fn hash_fun(h: &mut IHash, stack: &mut Vec<Item>, bp: *mut Eterm, hdr: Eterm) -> Option<Eterm> {
    let num_free = fun_num_free(hdr);
    let kind = fun_kind_of_header(hdr);
    match kind {
        FunKind::Local => {
            // Descriptor words 1..2 are `[uniq: u64][index: u32]`.
            // SAFETY: words 1 and 2 are within a live local fun's payload.
            let index = unsafe { (*bp.add(2)).raw() } as u32;
            h.mix_alpha_2f32(IHASH_TYPE_LOCAL_FUN as u32, num_free as u32);
            // The C mixes the `ErlFunEntry`'s `index` and `old_uniq`; the inlined
            // descriptor has no `old_uniq`, which is 0 for a `make_fun3`-built fun.
            h.mix_beta_2f32(index, 0);
            h.mix_alpha(IHASH_TYPE_IMMEDIATE);
            h.mix_beta(0);
        }
        FunKind::Imported => {
            // Descriptor word 1 is the module atom, word 4 `index`, word 6 `old_uniq`.
            // SAFETY: words 1, 4 and 6 are within a live imported fun's payload.
            let (module, index, old_uniq) =
                unsafe { (*bp.add(1), (*bp.add(4)).raw() as u32, (*bp.add(6)).raw() as u32) };
            h.mix_alpha_2f32(IHASH_TYPE_LOCAL_FUN as u32, num_free as u32);
            h.mix_beta_2f32(index, old_uniq);
            h.mix_alpha(IHASH_TYPE_IMMEDIATE);
            h.mix_beta(module.raw() as u64);
        }
        FunKind::External => {
            // The C mixes `(UWord)funp->entry.exp` (an `Export*`). We have no export
            // table, so the descriptor's `module`/`function` atoms are mixed instead.
            // SAFETY: words 1 and 2 are within a live external fun's payload.
            let (module, function) = unsafe { (*bp.add(1), *bp.add(2)) };
            h.mix_alpha(IHASH_TYPE_EXTERNAL_FUN);
            h.mix_beta(module.raw() as u64);
            h.mix_beta(function.raw() as u64);
            return None;
        }
    }
    if num_free > 0 {
        let env_off = 1 + fun_desc_words(kind);
        for i in 0..num_free - 1 {
            // SAFETY: env word `i` is within the fun's payload.
            push_term(h, stack, unsafe { *bp.add(env_off + i) });
        }
        // SAFETY: env word `num_free - 1` is within the fun's payload.
        return Some(unsafe { *bp.add(env_off + num_free - 1) });
    }
    None
}

/// The `REF_SUBTAG` arm. Assumes the 64-bit little-endian `ErtsORefThing` /
/// `ErtsPRefThing` layout; see the module docs.
fn hash_internal_ref(h: &mut IHash, bp: *mut Eterm, hdr: Eterm) {
    // Total words including the header (`ERTS_REF_THING_SIZE` = 3,
    // `ERTS_PID_REF_THING_SIZE` = 4 on 64-bit; the header's arity is `words - 1`).
    let words = header_arity(hdr) + 1;
    // SAFETY: words 1.. of a live ref thing are its `Uint32` number array.
    let read = |i: usize| unsafe { (*bp.add(i)).raw() as u64 };
    if words == 4 {
        // `ErtsPRefThing`: `num[0..5]` packed little-endian from byte offset 8.
        let (w1, w2, w3) = (read(1), read(2), read(3));
        let numbers = [
            w1 as u32,
            (w1 >> 32) as u32,
            w2 as u32,
            (w2 >> 32) as u32,
            w3 as u32,
        ];
        h.mix_alpha_2f32(IHASH_TYPE_LOCAL_REF as u32, numbers[0]);
        h.mix_beta_2f32(numbers[1], numbers[2]);
        h.mix_alpha_2f32(numbers[3], numbers[4]);
    } else if words == 3 {
        // `ErtsORefThing`: `num[0..3]` packed little-endian from byte offset 8,
        // followed by the `0xffff_ffff` marker.
        let (w1, w2) = (read(1), read(2));
        let numbers = [w1 as u32, w2 as u32, (w2 >> 32) as u32];
        h.mix_alpha_2f32(IHASH_TYPE_LOCAL_REF as u32, numbers[0]);
        h.mix_beta_2f32(numbers[1], numbers[2]);
    } else {
        // An unknown ref-thing size: mix the raw payload words (documented fallback).
        for i in 1..words {
            h.mix_beta(read(i));
        }
    }
}

/// The `EXTERNAL_{REF,PID,PORT}_SUBTAG` arms.
fn hash_external_thing(h: &mut IHash, term: Eterm, subtag: usize) {
    // SAFETY: `term` is a live external pid/port/ref thing.
    unsafe {
        match subtag {
            EXTERNAL_REF_SUBTAG => {
                let (node, ids, _creation) = crate::thing::external_ref_parts(term);
                let n = ids.len();
                h.mix_alpha_2f32(IHASH_TYPE_EXTERNAL_REF as u32, n as u32);
                let mut i = 0usize;
                while i + 2 <= n {
                    h.mix_beta_2f32(ids[i], ids[i + 1]);
                    i += 2;
                }
                if i < n {
                    h.mix_beta(u64::from(ids[i]));
                }
                // The C mixes the `ErlNode*`; the node slot holds an atom term here.
                h.mix_alpha(node.raw() as u64);
            }
            EXTERNAL_PID_SUBTAG => {
                let (node, number, serial, _creation) = crate::thing::external_pid_parts(term);
                h.mix_alpha(IHASH_TYPE_EXTERNAL_PID);
                h.mix_beta(node.raw() as u64);
                h.mix_alpha_2f32(number, serial);
            }
            EXTERNAL_PORT_SUBTAG => {
                let (node, id, _creation) = crate::thing::external_port_parts(term);
                h.mix_alpha(IHASH_TYPE_EXTERNAL_PORT);
                h.mix_beta(node.raw() as u64);
                h.mix_alpha(id);
            }
            _ => {}
        }
    }
}

/// The `BIN_REF_SUBTAG`/`HEAP_BITS_SUBTAG`/`SUB_BITS_SUBTAG` arm: mix the bit size,
/// then the payload in 8-byte halves, then the sub-byte tail.
fn hash_binary(h: &mut IHash, data: *const u8, offset_bits: usize, size_bits: usize) {
    h.mix_alpha(IHASH_TYPE_BINARY);
    h.mix_beta(size_bits as u64);
    if size_bits == 0 {
        return;
    }
    let nbytes = bit_count_to_bytes(size_bits);

    // When the window is not byte-aligned the C copies the bits into a scratch buffer
    // (`erts_copy_bits_fwd`); otherwise it reads the payload bytes in place. Both
    // produce the same bytes for the window, except that the scratch copy zeroes the
    // bits past the window in the last byte — which only the *tail* mix observes, and
    // that mix discards them (`>> (8 - TAIL_BITS)`).
    let scratch = if offset_bits % 8 != 0 {
        Some(bit_window(data, offset_bits, size_bits))
    } else {
        None
    };
    let byte = |i: usize| -> u8 {
        match &scratch {
            // SAFETY (borrowed case): the window `[offset/8, offset/8 + nbytes)` lies
            // within the payload's `ceil((offset+size)/8)` bytes.
            None => unsafe { *data.add((offset_bits >> 3) + i) },
            Some(buf) => buf[i],
        }
    };

    let mut it = 0usize;
    while it + 16 <= nbytes {
        h.mix_alpha(u64::from_le_bytes([
            byte(it),
            byte(it + 1),
            byte(it + 2),
            byte(it + 3),
            byte(it + 4),
            byte(it + 5),
            byte(it + 6),
            byte(it + 7),
        ]));
        h.mix_beta(u64::from_le_bytes([
            byte(it + 8),
            byte(it + 9),
            byte(it + 10),
            byte(it + 11),
            byte(it + 12),
            byte(it + 13),
            byte(it + 14),
            byte(it + 15),
        ]));
        it += 16;
    }

    // The remaining `nbytes % 16` bytes. Bytes 8.. (into a beta-only accumulator) and
    // bytes 0..8 (into an alpha-only accumulator) are mixed with a *bare* XOR — no
    // rotation/add/ticks (the C inlines the primitive here instead of the macro).
    let rem = nbytes % 16;
    if rem >= 9 {
        let mut value: u64 = 0;
        for j in 8..rem {
            value ^= u64::from(byte(it + j)) << ((j - 8) * 8);
        }
        let mut e = value;
        e = e.wrapping_mul(IHASH_C2);
        e = rotl64(e, 33);
        e = e.wrapping_mul(IHASH_C1);
        h.beta ^= e;

        let mut value: u64 = 0;
        for j in 0..8 {
            value ^= u64::from(byte(it + j)) << (j * 8);
        }
        let mut e = value;
        e = e.wrapping_mul(IHASH_C1);
        e = rotl64(e, 31);
        e = e.wrapping_mul(IHASH_C2);
        h.alpha ^= e;
    } else if rem >= 1 {
        let mut value: u64 = 0;
        for j in 0..rem {
            value ^= u64::from(byte(it + j)) << (j * 8);
        }
        let mut e = value;
        e = e.wrapping_mul(IHASH_C1);
        e = rotl64(e, 31);
        e = e.wrapping_mul(IHASH_C2);
        h.alpha ^= e;
    }

    // `TAIL_BITS(size) != 0`: the high `size % 8` bits of the window's last byte.
    let tail = size_bits % 8;
    if tail != 0 {
        let shift = (8 - tail) as u32;
        h.mix_alpha(u64::from(byte(size_bits >> 3) >> shift));
    }
}

/// The bit-shifted copy `erts_copy_bits_fwd(data, offset, tmp, 0, size)` produces:
/// `ceil(size/8)` bytes, MSB-first, with the bits past the window zeroed.
fn bit_window(data: *const u8, offset_bits: usize, size_bits: usize) -> Vec<u8> {
    let nbytes = bit_count_to_bytes(size_bits);
    let mut out = vec![0u8; nbytes];
    for (j, slot) in out.iter_mut().enumerate() {
        let mut b = 0u8;
        for k in 0..8 {
            let bit = j * 8 + k;
            if bit >= size_bits {
                // Trailing bits stay zero, as the C's bit copy leaves them.
                break;
            }
            let src = offset_bits + bit;
            // SAFETY: bit `bit < size` so the source byte is within the payload.
            let s = unsafe { *data.add(src >> 3) };
            if (s >> (7 - (src & 7))) & 1 != 0 {
                b |= 1 << (7 - k);
            }
        }
        *slot = b;
    }
    out
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// `erts_internal_hash(term)` (`erl_term_hashing.c:2307-2313`): the raw 64-bit
/// internal hash. An **immediate** hashes from its raw `Eterm` word
/// (`ihash_mix64(term)`); anything else takes the structural walk with a zero salt.
///
/// The caller must pass a live, well-formed term (the usual `beam_term` invariant).
pub fn erts_internal_hash(term: Eterm) -> u64 {
    if is_immediate(term) {
        return ihash_mix64(term.raw() as u64);
    }
    make_internal_hash(term, 0)
}

/// `erts_internal_salted_hash(term, salt)` (`erl_term_hashing.c:2292-2299`).
///
/// The caller must pass a live, well-formed term.
pub fn erts_internal_salted_hash(term: Eterm, salt: u64) -> u64 {
    if is_immediate(term) {
        return ihash_mix64((term.raw() as u64).wrapping_add(salt));
    }
    make_internal_hash(term, salt)
}

/// `erts_map_hash(key)` (`erl_term_hashing.c:2315-2319`) — the hash that selects a
/// key's path through a map's HAMT (`hashmap_make_hash`, `erl_map.h:78`).
///
/// Level `L` of the trie uses the nibble `(erts_map_hash(key) >> (4*L)) & 0xF`
/// (`hashmap_index`, `erl_map.h:219`; see [`crate::map`]).
///
/// The caller must pass a live, well-formed term (the usual `beam_term` invariant);
/// see the module docs for the classes whose C hash is derived from a process-local
/// pointer and therefore cannot be reproduced bit-exactly.
pub fn erts_map_hash(key: Eterm) -> u64 {
    erts_internal_hash(key)
}

/// The **swizzle** of a map hash (`swizzle_map_hash`, `erl_map.c:4170-4177`):
/// `hashmap_byte_swap` followed by a per-byte nibble swap.
///
/// The two steps together are exactly a **nibble reversal** of the 64-bit hash, so
/// MSB-nibble-first ordering of `swizzle_map_hash(h)` (`maskval`, `erl_map.c:109`) is
/// LSB-nibble-first ordering of `h` — the order in which the trie consumes
/// [`erts_map_hash`]'s nibbles. Ordering leaves by this value makes equal-nibble runs
/// contiguous at every level, which is what lets `write_map` group them in one pass.
pub const fn swizzle_map_hash(hash: u64) -> u64 {
    // GHEFCDAB: reverse the byte order...
    let swapped = hash.swap_bytes();
    // ... then swap the nibbles inside every byte: HGFEDCBA.
    ((swapped & 0xF0F0_F0F0_F0F0_F0F0) >> 4) | ((swapped & 0x0F0F_0F0F_0F0F_0F0F) << 4)
}

/// The trie level selector: `hashmap_index` after `(HAMT_MAX_LEVEL - 1 - L)` shifts
/// (`maskval`, `erl_map.c:109` with the swizzled hash) — i.e. the **`level`-th least
/// significant** nibble of `hash`.
#[inline]
pub const fn hashmap_nibble(hash: u64, level: usize) -> u32 {
    ((hash >> (4 * level)) & 0xF) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small};
    use crate::thing::{write_flatmap, write_float};
    use crate::tuple::tuple0;

    #[test]
    fn mixer_matches_an_independent_model() {
        // (kind, alpha_expr, beta_expr): kind 1 = alpha+beta, 2 = alpha only,
        // 3 = beta only. Checked against `IHash`, so a typo in either the mixing
        // primitive or the call order makes this fail.
        let ops: &[(u64, u64, u64)] = &[
            (1, IHASH_TYPE_TUPLE, 2),
            (2, IHASH_TYPE_ARRAY_ELEMENT, 0),
            (1, IHASH_TYPE_IMMEDIATE, 0xF),
            (3, 0, 0x1234_5678_9ABC_DEF0),
            (1, IHASH_TYPE_STRING | (4 << 8), 0x6463_6261),
        ];
        let mut h = IHash::new(0xDEAD_BEEF);
        for &(kind, ae, be) in ops {
            if kind != 3 {
                h.mix_alpha(ae);
            }
            if kind != 2 {
                h.mix_beta(be);
            }
        }
        let (mut a, mut b, mut t) = (0xDEAD_BEEFu64, 0xDEAD_BEEFu64, 0u64);
        for &(kind, ae, be) in ops {
            if kind != 3 {
                let mut e = ae;
                e = e.wrapping_mul(IHASH_C1);
                e = rotl64(e, 31);
                e = e.wrapping_mul(IHASH_C2);
                a ^= e;
                a = rotl64(a, 27);
                a = a.wrapping_add(b);
                a = a.wrapping_mul(5).wrapping_add(0x52DC_E729);
                t += 1;
            }
            if kind != 2 {
                let mut e = be;
                e = e.wrapping_mul(IHASH_C2);
                e = rotl64(e, 33);
                e = e.wrapping_mul(IHASH_C1);
                b ^= e;
                b = rotl64(b, 31);
                b = b.wrapping_add(a);
                b = b.wrapping_mul(5).wrapping_add(0x3849_5AB5);
                t += 1;
            }
        }
        let mut x = a ^ t;
        let mut y = b ^ t;
        x = x.wrapping_add(y);
        y = y.wrapping_add(x);
        x = ihash_mix64(x);
        y = ihash_mix64(y);
        x = x.wrapping_add(y);
        y = y.wrapping_add(x);
        assert_eq!(h.finish(), x ^ y);
    }

    #[test]
    fn immediate_hash_is_the_raw_word_mixed() {
        // `erts_internal_hash`'s fast path: `ihash_mix64((Uint)term)`.
        for n in [-1000i64, -1, 0, 1, 2, 42, 255, 1 << 40] {
            let t = make_small(n);
            assert_eq!(erts_map_hash(t), ihash_mix64(t.raw() as u64));
        }
        // An atom immediate likewise hashes from its raw (index-shifted) word, not
        // from its name.
        let atom0 = make_atom(crate::atom::Atom::from_raw(0));
        assert_eq!(erts_map_hash(atom0), ihash_mix64(atom0.raw() as u64));
    }

    #[test]
    fn salted_immediate_hash_adds_the_salt_first() {
        for n in [0i64, 1, 7, 1 << 30] {
            let t = make_small(n);
            assert_eq!(
                erts_internal_salted_hash(t, 0x1234),
                ihash_mix64((t.raw() as u64).wrapping_add(0x1234))
            );
            assert_eq!(erts_internal_salted_hash(t, 0), erts_internal_hash(t));
        }
    }

    #[test]
    fn a_two_element_integer_tuple_uses_the_small_int_path() {
        // `{1,2}`: TUPLE(6), arity(2), then element 1 is pushed (an immediate, so
        // mixed at once) and element 2 becomes the current term.
        let mut heap = Heap::new(8);
        let t = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits");
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha(IHASH_TYPE_TUPLE);
            h.mix_beta(2);
            // push_term on element 1 (an immediate): mix immediate, no push.
            h.mix_immediate((make_small(1)).raw() as u64);
            // current term = element 2 (an immediate).
            h.mix_immediate((make_small(2)).raw() as u64);
            h.finish()
        };
        assert_eq!(erts_map_hash(t), expected);
    }

    #[test]
    fn a_boxed_element_is_deferred_with_the_presence_marker() {
        // `{{7}, 1}`: element 0 (a tuple) is not an immediate, so its presence
        // marker is mixed and the element deferred; element 1 is the current term.
        // The deferred tuple is then popped and processed as a tuple.
        let mut heap = Heap::new(16);
        let inner = heap.tuple(&[make_small(7)]).expect("the inner tuple fits");
        let t = heap
            .tuple(&[inner, make_small(1)])
            .expect("the tuple fits");
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha(IHASH_TYPE_TUPLE);
            h.mix_beta(2);
            // push_term on element 0 (a tuple): presence marker + defer.
            h.mix_alpha(IHASH_TYPE_ARRAY_ELEMENT);
            // current term = element 1 (an immediate).
            h.mix_immediate((make_small(1)).raw() as u64);
            // pop the deferred tuple and process it as a 1-tuple.
            h.mix_alpha(IHASH_TYPE_TUPLE);
            h.mix_beta(1);
            h.mix_immediate((make_small(7)).raw() as u64);
            h.finish()
        };
        assert_eq!(erts_map_hash(t), expected);
    }

    #[test]
    fn float_hashes_its_ieee_bits() {
        // Each float occupies `FLOAT_SIZE_WORDS` words and the bump arena is not
        // reset, so reserve room for all six.
        let mut heap = Heap::new(6 * crate::thing::FLOAT_SIZE_WORDS);
        for v in [0.0f64, -0.0, 1.0, 3.5, f64::MIN_POSITIVE, f64::MAX] {
            // SAFETY: 2 words are reserved for the float block.
            let t = unsafe {
                let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
                write_float(hp, v)
            };
            let expected = {
                let mut h = IHash::new(0);
                h.mix_alpha(IHASH_TYPE_FLOAT);
                h.mix_beta(v.to_bits());
                h.finish()
            };
            assert_eq!(erts_map_hash(t), expected);
            assert!(erts_map_hash(t) != 0);
        }
    }

    #[test]
    fn flat_map_mixes_values_before_keys_and_matches_the_model() {
        let mut heap = Heap::new(32);
        let a = make_small(1);
        let b = make_small(2);
        // SAFETY: 4 + 2*2 words for a 2-pair flat map.
        let m = unsafe {
            let hp = heap.alloc_words(crate::thing::flatmap_size_words(2)).unwrap();
            write_flatmap(hp, &[(a, make_small(10)), (b, make_small(20))])
        };
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha(IHASH_TYPE_FLATMAP);
            h.mix_beta(2);
            h.mix_immediate((make_small(10)).raw() as u64);
            h.mix_immediate(a.raw() as u64);
            h.mix_immediate((make_small(20)).raw() as u64);
            h.mix_immediate(b.raw() as u64);
            h.finish()
        };
        assert_eq!(erts_map_hash(m), expected);
    }

    #[test]
    fn empty_flat_map_hashes_only_its_header_and_size() {
        let mut heap = Heap::new(8);
        // SAFETY: 3 words for the empty flat map.
        let m = unsafe {
            let hp = heap.alloc_words(crate::thing::flatmap_size_words(0)).unwrap();
            write_flatmap(hp, &[])
        };
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha(IHASH_TYPE_FLATMAP);
            h.mix_beta(0);
            h.finish()
        };
        assert_eq!(erts_map_hash(m), expected);
    }

    #[test]
    fn swizzle_is_a_nibble_reversal() {
        for h in [0u64, 1, 0x0123_4567_89AB_CDEF, u64::MAX, 0xFFFF_0000_FFFF_0000] {
            let mut manual = 0u64;
            for j in 0..16u32 {
                manual |= ((h >> (4 * j)) & 0xF) << (4 * (15 - j));
            }
            assert_eq!(swizzle_map_hash(h), manual);
        }
        // `hashmap_nibble(swizzle(h), L)` is the MSB nibble of the swizzle, i.e. the
        // LSB nibble of `h` — the C's `maskval(swizzle, L)`.
        for h in [0x0123_4567_89AB_CDEFu64, 0xDEAD_BEEF_CAFE_BABE] {
            let sw = swizzle_map_hash(h);
            for level in 0..16usize {
                assert_eq!(
                    (sw >> ((15 - level) * 4)) & 0xF,
                    u64::from(hashmap_nibble(h, level)),
                    "level {level}"
                );
            }
        }
    }

    #[test]
    fn list_string_batching_matches_the_model() {
        // `"abc"` = [97, 98, 99]: 3 byte heads are batched into one STRING mix, then
        // the tail NIL is the current immediate.
        let mut heap = Heap::new(16);
        let mut list = crate::immediate::NIL;
        for c in [99i64, 98, 97] {
            list = heap.cons(make_small(c), list).unwrap();
        }
        let mut value: u64 = 0;
        for c in [97i64, 98, 99] {
            value = (value << 8) | (c as u64);
        }
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha_2f32((IHASH_TYPE_STRING | (3 << 8)) as u32, value as u32);
            h.mix_immediate(crate::immediate::NIL.raw() as u64);
            h.finish()
        };
        assert_eq!(erts_map_hash(list), expected);

        // A four-byte run flushes exactly at the fourth byte, then starts over.
        let mut list = crate::immediate::NIL;
        for c in [100i64, 99, 98, 97] {
            list = heap.cons(make_small(c), list).unwrap();
        }
        let value = (97u64 << 24) | (98 << 16) | (99 << 8) | 100;
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha_2f32((IHASH_TYPE_STRING | (4 << 8)) as u32, value as u32);
            h.mix_immediate(crate::immediate::NIL.raw() as u64);
            h.finish()
        };
        assert_eq!(erts_map_hash(list), expected);
    }

    #[test]
    fn non_byte_head_starts_the_cons_cell_path() {
        // `[1000]`: the head is not a byte, so no STRING mix; the cell is `CAR`(immed)
        // + a NIL `CDR` (which is not a list, so `IHASH_TYPE_CDR` is mixed).
        let mut heap = Heap::new(8);
        let l = heap.cons(make_small(1000), crate::immediate::NIL).unwrap();
        let expected = {
            let mut h = IHash::new(0);
            h.mix_alpha_2f32(IHASH_TYPE_IMMEDIATE as u32, IHASH_TYPE_CAR as u32);
            h.mix_beta((make_small(1000)).raw() as u64);
            h.mix_alpha(IHASH_TYPE_CDR);
            h.mix_immediate(crate::immediate::NIL.raw() as u64);
            h.finish()
        };
        assert_eq!(erts_map_hash(l), expected);
    }

    #[test]
    fn distinct_terms_hash_differently() {
        let mut heap = Heap::new(32);
        let tup = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let a = make_small(1);
        let b = make_small(2);
        assert_ne!(erts_map_hash(tup), erts_map_hash(a));
        assert_ne!(erts_map_hash(a), erts_map_hash(b));
        // `{}` (the empty tuple literal) is a distinct term from NIL.
        assert_ne!(erts_map_hash(tuple0()), erts_map_hash(crate::immediate::NIL));
    }
}
