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

//! `erlang:phash/2` — the legacy ERTS `make_hash` term hash (and `phash2`).
//!
//! Ground truth (read-only reference; never edited):
//! `../../erts/emulator/beam/erl_term_hashing.c` — `hash_binary_bytes` (lines
//! 114–163) and `make_hash` (lines 165–425) plus `MIX`/`block_hash` (427–521) and
//! `make_hash2_helper` (858–1585), the `bif erlang:phash/2` and
//! `erlang:phash2/1,2` entries in `../../erts/emulator/beam/bif.tab:82-84`, the
//! BIF wrappers `phash_2`/`phash2_1`/`phash2_2` (`../../erts/emulator/beam/bif.c:5370-5447`),
//! the atom hash `atom_hash` (`../../erts/emulator/beam/atom.c:82-105`) and
//! `term_equals_2pow32` (`../../erts/emulator/beam/big.c:4038-4044`).
//!
//! This module owns **both** ERTS term hashes. `erlang:phash/2` is the **legacy**
//! `make_hash`; `erlang:phash2/1,2` is `make_hash2` — Bob Jenkins' `MIX`-based
//! block hash, an unrelated algorithm (`erl_term_hashing.c:427-521, 858-1601`,
//! BIF wrappers `bif.c:5401-5447`). Both are reproduced bit-exactly.
//!
//! # The legacy algorithm
//!
//! A single `u32` accumulator `h`, starting at `0`, is threaded through the term;
//! every step is a multiply and an add, so [`u32::wrapping_mul`] /
//! [`u32::wrapping_add`] reproduce the C word arithmetic bit-exactly (each C
//! result is used only mod 2^32).
//!
//! * `[]` — `h*F3 + 1`.
//! * atom — `h*F1 + atom_hvalue(name)` (hashpjw over the atom's name bytes).
//! * small integer — a 32-bit byte-wise step over `|v|` (twice, when
//!   `|v| >= 2^32`), then `h*(v < 0 ? F4 : F3)`.
//! * bignum — the *same* bytes of `|N|` as the small path, least-significant byte
//!   first: eight bytes per full limb, four or eight for the top limb, then
//!   `h*(negative ? F4 : F3)`.
//! * float — `-0.0` is normalised to `+0.0`, then
//!   `h*F6 + (low_bits ^ high_bits)`.
//! * binary/bitstring — the significant bytes (MSB-first packing) via
//!   `hash_binary_bytes`, then the trailing partial byte as
//!   `(h*F1 + b)*F12 + bitsize`, then `h*F4 + bytesize`.
//! * tuple — every element left to right, then `h*F9 + arity`.
//! * list — `h*F2 + byte` per byte-sized head element, a full recursive hash for
//!   any other head, and exactly one `h*F8` at the terminating non-list tail
//!   (`[]` included — [`beam_term::is_list`] is **false** for `NIL`).
//! * pid — `UINT32_HASH_STEP(number, F5)`, then `h*F6`.
//! * port — the 64-bit number: a step over the high half (when set) with `F11`,
//!   a step over the low half with `F9`, then `h*F10`.
//! * external fun — `h*F11 + arity`, then `h*F1 + atom_hvalue(module)`, then
//!   `h*F1 + atom_hvalue(function)`.
//! * local/imported fun — `h*F10 + num_free`, `h*F1 + atom_hvalue(module)`,
//!   `h*F2 + index`, `h*F2 + old_uniq`, then the environment `h*F8`-free
//!   (`env[0]` first, then `env[1..]`).
//! * map — the `MAP_DEF` rule delegates to the *other* hash:
//!   `h*F13 + F14 + make_hash2(term)`.
//!
//! # Reproduction limits (always a typed error, never a wrong number)
//!
//! A **record**'s `make_hash`/`make_hash2` identity is its definition's cached
//! `make_hash2` hash, which this tree stores as `0` (no `make_hash2` at record
//! creation), and an **internal reference** read reaches through the reference's
//! container (`internal_ref_numbers`, `erl_node_container_utils.h`) whose first
//! word this tree's M1 ref thing does not carry. A **local** (`make_fun3`) fun's
//! C identity — `ErlFunEntry.module` and `old_uniq` — is not part of this tree's
//! inlined descriptor ([`beam_term::fun`] carries only `[uniq:u64][index:u32]`).
//! None of those has a faithful `u32` here, so they raise the typed
//! [`unsupported_bif`] (see the crate `CONTEXT.md`, Known Issues).
//!
//! # The `make_hash2` algorithm (`phash2/1,2`)
//!
//! A `u32` accumulator threaded through the term, but every fold is Bob Jenkins'
//! `MIX` over three words (`a = con + e1`, `b = con + e2`, `c = hash`) — a 9-round
//! shift/xor lattice (`MIX`, `erl_term_hashing.c:428-439`). Binary and bitstring
//! payloads go through the `block_hash` 12-byte unrolled loop instead of a byte
//! multiply. Entries:
//!
//! * atom ⇒ `hvalue` when the accumulator is `0`, else `MIX(HCONST_3+hvalue, HCONST_3, h)`.
//! * `[]` ⇒ `3468870702` when `0`, else `MIX(HCONST_2+2, HCONST_2, h)`.
//! * small integer ⇒ `SINT32_HASH` (mixed twice when negative) inside `|v| < 2^27`,
//!   else `NOT_SSMALL28_HASH` over the two 32-bit halves of `|v|` with
//!   `HCONST_10`/`HCONST_11`.
//! * tuple ⇒ `MIX(HCONST_9+arity, …, h)` then elements left to right.
//! * list ⇒ the byte-run fast path (four bytes per `MIX` with `HCONST_4`) then the
//!   ordinary rules for the first non-byte head and the remaining CDR chain.
//! * map ⇒ the size with `HCONST_16`, then the **xor** of each independently hashed
//!   pair folded in with `HCONST_19` (portable/order-independent).
//! * float ⇒ the normalised `[sign,exp,mantissa]` words `(high, low)` with `HCONST_12`.
//! * pid/port/ref/fun/bignum/bitstring ⇒ the per-class `HCONST_*` mixes below.
//!
//! `phash2/1` reduces with `hash & ((1<<27)-1)`; `phash2/2` returns `hash % range`
//! (no `+1`, unlike `phash/2`) — `range == 0` being the special `2^32` case.
//!
//! # `unsafe`
//!
//! None: every accessor goes through `crate::access` (the crate's only
//! `#[allow(unsafe_code)]` module) or through `beam_codec::term`'s safe readers.

use beam_codec::term::{
    fun_arity_term, fun_env, fun_function, fun_index, fun_kind_term, fun_module,
    fun_num_free_term, fun_old_uniq, FunKind,
};
use beam_common::{BeamError, Result};
use beam_term::{AtomTable, Eterm};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// The fifteen `FUNNY_NUMBER*` primes (`erl_term_hashing.c:98-112`)
// ---------------------------------------------------------------------------
//
// `F7` and `F15` are deliberately absent: `F13`/`F14` are the map rule's factor and
// offset (used below) and `F15` belongs to the record rule (`hash*F15 + arity`),
// which raises `unsupported_bif` (see the module docs).

/// `FUNNY_NUMBER1` — the atom/binary/byte step prime.
const F1: u32 = 268_440_163;
/// `FUNNY_NUMBER2` — the small-int/bignum/list-byte step prime.
const F2: u32 = 268_439_161;
/// `FUNNY_NUMBER3` — the non-negative magnitude factor.
const F3: u32 = 268_435_459;
/// `FUNNY_NUMBER4` — the negative magnitude / bitstring-tail factor.
const F4: u32 = 268_436_141;
/// `FUNNY_NUMBER5` — the pid-number step prime.
const F5: u32 = 268_438_633;
/// `FUNNY_NUMBER6` — the pid/float factor.
const F6: u32 = 268_437_017;
/// `FUNNY_NUMBER8` — the end-of-list factor.
const F8: u32 = 268_437_511;
/// `FUNNY_NUMBER9` — the tuple/port-low step prime.
const F9: u32 = 268_439_627;
/// `FUNNY_NUMBER10` — the ref/port/local-fun factor.
const F10: u32 = 268_440_479;
/// `FUNNY_NUMBER11` — the external-fun/port-high step prime.
const F11: u32 = 268_440_577;
/// `FUNNY_NUMBER12` — the bitstring trailing-bits factor.
const F12: u32 = 268_440_581;
/// `FUNNY_NUMBER13` — the map rule's accumulator factor.
const F13: u32 = 268_440_593;
/// `FUNNY_NUMBER14` — the map rule's accumulator offset.
const F14: u32 = 268_440_611;

/// `UINT32_HASH_STEP(x, prime)` (`erl_term_hashing.c:184-192`): fold the four
/// bytes of `x`, most significant last, into the accumulator.
#[inline]
fn step(h: u32, x: u32, prime: u32) -> u32 {
    let h = h.wrapping_mul(prime).wrapping_add(x & 0xFF);
    let h = h.wrapping_mul(prime).wrapping_add((x >> 8) & 0xFF);
    let h = h.wrapping_mul(prime).wrapping_add((x >> 16) & 0xFF);
    h.wrapping_mul(prime).wrapping_add(x >> 24)
}

// ---------------------------------------------------------------------------
// Atom hashpjw (`atom.c:82-105`)
// ---------------------------------------------------------------------------

/// `atom_tab(i)->slot.bucket.hvalue` — the hashpjw value [`make_hash`] adds for an
/// atom, computed over the atom's name **bytes**.
///
/// The accumulator is a **`u64`** (the C `HashValue` is `typedef UWord HashValue`,
/// `hash.h:32`) and only the low 32 bits are used, because the C adds the 64-bit
/// hvalue into a 32-bit `Uint32 hash` (so the sum is truncated). The `0xf0000000`
/// mask clears bits 28–31 only: for a long name the remaining high bits survive and
/// the accumulator keeps growing, which `u64` wrapping reproduces exactly.
///
/// The "latin1 clutch for r16" is reproduced too: a `0xC2`/`0xC3` lead byte
/// followed by a `0x80..=0xBF` continuation collapses into the single latin1 byte
/// it encodes (`(v << 6) | (next & 0x3F)` truncated to eight bits).
fn atom_hvalue(name: &str) -> u32 {
    let bytes = name.as_bytes();
    let mut h: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let mut v = bytes[i];
        let mut width = 1;
        if i + 1 < bytes.len() && (v & 0xFE) == 0xC2 && (bytes[i + 1] & 0xC0) == 0x80 {
            // The C assigns into a `byte v`, so the shifted value is truncated.
            v = (((u32::from(v) << 6) | u32::from(bytes[i + 1] & 0x3F)) & 0xFF) as u8;
            width = 2;
        }
        h = h.wrapping_shl(4).wrapping_add(u64::from(v));
        let g = h & 0xf000_0000;
        if g != 0 {
            h ^= g >> 24;
            h ^= g;
        }
        i += width;
    }
    (h & 0xFFFF_FFFF) as u32
}

// ---------------------------------------------------------------------------
// The term walk
// ---------------------------------------------------------------------------

/// `make_hash(term)` starting from accumulator `h`, with the atom table needed to
/// resolve atom names.
///
/// Lists are walked iteratively; only *nested* containers recurse (exactly as the
/// crate's other term walkers do).
///
/// # Errors
///
/// [`unsupported_bif`] for a tag class this crate cannot reproduce exactly (map,
/// record, reference, local/imported fun, …) and [`BeamError::TermNotFound`] for
/// an atom index absent from `atoms` — never a wrong hash.
fn hash_term(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    // `[]` is an immediate, and `is_list(NIL)` is false (see the module docs).
    if beam_term::is_nil(term) {
        return Ok(h.wrapping_mul(F3).wrapping_add(1));
    }
    if beam_term::is_small(term) {
        let y1 = beam_term::signed_value(term);
        let y2 = y1.unsigned_abs();
        let mut hash = step(h, y2 as u32, F2);
        if (y2 >> 32) != 0 {
            hash = step(hash, (y2 >> 32) as u32, F2);
        }
        return Ok(hash.wrapping_mul(if y1 < 0 { F4 } else { F3 }));
    }
    if beam_term::is_atom(term) {
        let name = atoms
            .name(beam_term::atom_val(term))
            .ok_or(BeamError::TermNotFound)?;
        return Ok(h.wrapping_mul(F1).wrapping_add(atom_hvalue(name)));
    }
    if access::is_list(term) {
        return hash_list(term, atoms, h);
    }
    if access::is_tuple(term) {
        let arity = access::tuple_size(term);
        let mut hash = h;
        for i in 0..arity {
            hash = hash_term(access::tuple_element(term, i), atoms, hash)?;
        }
        return Ok(hash.wrapping_mul(F9).wrapping_add(arity as u32));
    }
    if access::is_float(term) {
        let value = access::float_value(term);
        // The C forces a positive zero (`refs_get_positive_zero_float`), so `-0.0`
        // hashes like `+0.0`.
        let bits = if value == 0.0 { 0 } else { value.to_bits() };
        return Ok(h
            .wrapping_mul(F6)
            .wrapping_add((bits as u32) ^ ((bits >> 32) as u32)));
    }
    if access::is_big(term) {
        return hash_big(term, h);
    }
    if access::is_bitstring(term) {
        return hash_bitstring(term, h);
    }
    if access::is_internal_pid(term) {
        return Ok(step(h, beam_term::pid_number(term) as u32, F5).wrapping_mul(F6));
    }
    if access::is_external_pid(term) {
        let (_, number, _, _) = beam_codec::term::external_pid_parts(term)?;
        return Ok(step(h, number, F5).wrapping_mul(F6));
    }
    if access::is_internal_port(term) {
        return Ok(hash_port(h, beam_term::port_number(term)));
    }
    if access::is_external_port(term) {
        let (_, number, _) = beam_codec::term::external_port_parts(term)?;
        return Ok(hash_port(h, number));
    }
    if access::is_function(term) {
        return hash_fun(term, atoms, h);
    }
    if access::is_map(term) {
        // `MAP_DEF` (`erl_term_hashing.c:366-368`): the map's whole contribution is
        // delegated to `make_hash2`, offset and scaled by this hash.
        let hash = h.wrapping_mul(F13).wrapping_add(F14);
        return Ok(hash.wrapping_add(make_hash2(term, atoms)?));
    }
    // Record, reference and every other tag class: `make_hash` reads state this
    // tree's terms do not carry (see the module docs).
    Err(unsupported_bif())
}

/// The `LIST_DEF` rule (`erl_term_hashing.c:310-333`).
///
/// A byte-sized head element folds in with `F2` (the "optimization for strings");
/// any other head is hashed recursively. The walk stops at the first non-list tail,
/// which is hashed and then multiplied by `F8` — so each list level applies exactly
/// one `*F8`, at its terminator.
fn hash_list(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    let mut cur = term;
    let mut hash = h;
    loop {
        let car = access::car(cur);
        let cdr = access::cdr(cur);
        if beam_term::is_byte(car) {
            hash = hash
                .wrapping_mul(F2)
                .wrapping_add(beam_term::unsigned_value(car) as u32);
        } else {
            hash = hash_term(car, atoms, hash)?;
        }
        if !access::is_list(cdr) {
            return Ok(hash_term(cdr, atoms, hash)?.wrapping_mul(F8));
        }
        cur = cdr;
    }
}

/// The `BIG_DEF` rule (`erl_term_hashing.c:335-365`) — byte-identical to the small
/// integer path for equal magnitudes.
fn hash_big(term: Eterm, h: u32) -> Result<u32> {
    let (negative, digits) = beam_codec::term::big_parts(term)?;
    let mut hash = h;
    if let Some((&top, rest)) = digits.split_last() {
        for &digit in rest {
            let mut d = digit;
            for _ in 0..8 {
                hash = hash.wrapping_mul(F2).wrapping_add((d & 0xFF) as u32);
                d >>= 8;
            }
        }
        // `k = sizeof(ErtsDigit)`, halved on `ARCH_64` when the top limb fits 32 bits.
        let bytes = if (top >> 32) != 0 { 8 } else { 4 };
        let mut d = top;
        for _ in 0..bytes {
            hash = hash.wrapping_mul(F2).wrapping_add((d & 0xFF) as u32);
            d >>= 8;
        }
    }
    Ok(hash.wrapping_mul(if negative { F4 } else { F3 }))
}

/// `hash_binary_bytes` (`erl_term_hashing.c:114-163`).
///
/// The C shifts the *unaligned* bit stream in place, which is exactly the MSB-first
/// byte packing [`access::bitstring_bits`] produces: packed byte `i` holds bits
/// `[8i, 8i+8)` of the bitstring, so both the `bitoffs == 0` loop and the
/// `bitoffs != 0` shift loop add the same bytes. The trailing `bitsize = size % 8`
/// bits are the **high** bits of packed byte `bytesize`, and the result ends with
/// `h*F4 + bytesize` — the count of *whole* bytes.
fn hash_bitstring(term: Eterm, h: u32) -> Result<u32> {
    let bit_len = access::bitstring_bit_size(term)?;
    let packed = access::bitstring_bits(term)?;
    let bytesize = bit_len / 8;
    let bitsize = bit_len % 8;

    let mut hash = h;
    for &byte in &packed[..bytesize] {
        hash = hash.wrapping_mul(F1).wrapping_add(u32::from(byte));
    }
    if bitsize > 0 {
        let tail = u32::from(packed[bytesize] >> (8 - bitsize));
        hash = hash
            .wrapping_mul(F1)
            .wrapping_add(tail)
            .wrapping_mul(F12)
            .wrapping_add(bitsize as u32);
    }
    Ok(hash.wrapping_mul(F4).wrapping_add(bytesize as u32))
}

/// The `PORT_DEF`/`EXTERNAL_PORT_DEF` rule (`erl_term_hashing.c:279-287`).
fn hash_port(h: u32, number: u64) -> u32 {
    let low = number as u32;
    let high = (number >> 32) as u32;
    let hash = if high != 0 { step(h, high, F11) } else { h };
    step(hash, low, F9).wrapping_mul(F10)
}

/// The `FUN_DEF` rule for the fun kinds this crate can reproduce.
///
/// An **external** fun (`EXPORT_EXT`) carries the exported MFA's module and
/// function atoms plus the arity in its header, so
/// `h*F11 + arity; h*F1 + hvalue(module); h*F1 + hvalue(function)` is exact.
///
/// An **imported** (`NEW_FUN_EXT`) fun is a *local* fun in C terms
/// (`external.c:5125` writes `MAKE_FUN_HEADER(arity, num_free, 0)`), but its
/// inlined descriptor carries exactly what `FUN_DEF`'s local rule needs — the
/// module atom, the lambda `index`, `old_uniq` and the environment — so
/// `h*F10 + num_free; h*F1 + hvalue(module); h*F2 + index; h*F2 + old_uniq`, then
/// each `env[k]` in index order, is exact too.
///
/// A local (`make_fun3`) fun's C identity is `ErlFunEntry.module`/`old_uniq`,
/// which this tree's `[uniq:u64][index:u32]` descriptor does not carry, so it
/// raises `unsupported_bif`.
fn hash_fun(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    match fun_kind_term(term)? {
        FunKind::External => {
            let arity = fun_arity_term(term)? as u32;
            let module = atoms
                .name(fun_module(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let function = atoms
                .name(fun_function(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let hash = h.wrapping_mul(F11).wrapping_add(arity);
            let hash = hash.wrapping_mul(F1).wrapping_add(atom_hvalue(module));
            Ok(hash.wrapping_mul(F1).wrapping_add(atom_hvalue(function)))
        }
        FunKind::Imported => {
            let num_free = fun_num_free_term(term)? as u32;
            let module = atoms
                .name(fun_module(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let index = fun_index(term)?;
            let old_uniq = fun_old_uniq(term)?;
            let mut hash = h.wrapping_mul(F10).wrapping_add(num_free);
            hash = hash.wrapping_mul(F1).wrapping_add(atom_hvalue(module));
            hash = hash.wrapping_mul(F2).wrapping_add(index);
            hash = hash.wrapping_mul(F2).wrapping_add(old_uniq);
            for &e in fun_env(term)? {
                hash = hash_term(e, atoms, hash)?;
            }
            Ok(hash)
        }
        FunKind::Local => Err(unsupported_bif()),
    }
}

// ---------------------------------------------------------------------------
// `make_hash2` — the `phash2` hash (`erl_term_hashing.c:427-521, 858-1585`)
// ---------------------------------------------------------------------------
//
// A `u32` accumulator folded through Bob Jenkins' `MIX` (`erl_term_hashing.c:428`)
// rather than the multiply/add ladder above. `HCONST_N` is `HCONST * N mod 2^32`
// (`erl_term_hashing.c:866-888`). All arithmetic wraps.

/// `HCONST` — the golden-ratio constant (`erl_term_hashing.c:441`).
const HCONST: u32 = 0x9e37_79b9;
/// `HCONST_2` — `HCONST * 2 mod 2^32` (the `[]` mix).
const HCONST_2: u32 = 0x3c6e_f372;
/// `HCONST_3` — the in-context atom mix.
const HCONST_3: u32 = 0xdaa6_6d2b;
/// `HCONST_4` — the list byte-run mix.
const HCONST_4: u32 = 0x78dd_e6e4;
/// `HCONST_5` — the pid-number mix.
const HCONST_5: u32 = 0x1715_609d;
/// `HCONST_6` — the port-number mix.
const HCONST_6: u32 = 0xb54c_da56;
/// `HCONST_7` — the external-reference mix.
const HCONST_7: u32 = 0x5384_540f;
/// `HCONST_9` — the tuple-arity mix.
const HCONST_9: u32 = 0x8ff3_4781;
/// `HCONST_10` — the negative bignum magnitude constant.
const HCONST_10: u32 = 0x2e2a_c13a;
/// `HCONST_11` — the non-negative bignum magnitude constant.
const HCONST_11: u32 = 0xcc62_3af3;
/// `HCONST_12` — the float mix.
const HCONST_12: u32 = 0x6a99_b4ac;
/// `HCONST_13` — the binary/bitstring context constant (added to the hash).
const HCONST_13: u32 = 0x08d1_2e65;
/// `HCONST_14` — the external-fun function-name mix.
const HCONST_14: u32 = 0xa708_a81e;
/// `HCONST_15` — the bitstring trailing-bits mix.
const HCONST_15: u32 = 0x4540_21d7;
/// `HCONST_16` — the map-size mix.
const HCONST_16: u32 = 0xe377_9b90;
/// `HCONST_19` — the map `xor` fold constant.
const HCONST_19: u32 = 0xbe1e_08bb;

/// Bob Jenkins' `MIX` (`erl_term_hashing.c:428-439`) — 9 rounds over `(a, b, c)`,
/// returning `c`. Every step is a wrapping `u32` subtract/xor/shift.
#[inline]
fn mix(mut a: u32, mut b: u32, mut c: u32) -> u32 {
    a = a.wrapping_sub(b);
    a = a.wrapping_sub(c);
    a ^= c >> 13;
    b = b.wrapping_sub(c);
    b = b.wrapping_sub(a);
    b ^= a << 8;
    c = c.wrapping_sub(a);
    c = c.wrapping_sub(b);
    c ^= b >> 13;
    a = a.wrapping_sub(b);
    a = a.wrapping_sub(c);
    a ^= c >> 12;
    b = b.wrapping_sub(c);
    b = b.wrapping_sub(a);
    b ^= a << 16;
    c = c.wrapping_sub(a);
    c = c.wrapping_sub(b);
    c ^= b >> 5;
    a = a.wrapping_sub(b);
    a = a.wrapping_sub(c);
    a ^= c >> 3;
    b = b.wrapping_sub(c);
    b = b.wrapping_sub(a);
    b ^= a << 10;
    c = c.wrapping_sub(a);
    c = c.wrapping_sub(b);
    c ^= b >> 15;
    c
}

/// [`mix`], returning the *mutated* `(a, b, c)`.
///
/// The C `MIX` macro (`erl_term_hashing.c:428-439`) mutates all three of its
/// lvalues in place, and `block_hash_buffer`/`block_hash_final_bytes` keep the
/// mutated `ctx->a`/`ctx->b` across iterations (the tail bytes are still added to
/// them). [`mix`] can therefore not be used inside `block_hash`'s loop, which must
/// carry `a`/`b` forward.
#[inline]
fn mix3(a: u32, b: u32, c: u32) -> (u32, u32, u32) {
    let mut a = a;
    let mut b = b;
    let mut c = c;
    a = a.wrapping_sub(b);
    a = a.wrapping_sub(c);
    a ^= c >> 13;
    b = b.wrapping_sub(c);
    b = b.wrapping_sub(a);
    b ^= a << 8;
    c = c.wrapping_sub(a);
    c = c.wrapping_sub(b);
    c ^= b >> 13;
    a = a.wrapping_sub(b);
    a = a.wrapping_sub(c);
    a ^= c >> 12;
    b = b.wrapping_sub(c);
    b = b.wrapping_sub(a);
    b ^= a << 16;
    c = c.wrapping_sub(a);
    c = c.wrapping_sub(b);
    c ^= b >> 5;
    a = a.wrapping_sub(b);
    a = a.wrapping_sub(c);
    a ^= c >> 3;
    b = b.wrapping_sub(c);
    b = b.wrapping_sub(a);
    b ^= a << 10;
    c = c.wrapping_sub(a);
    c = c.wrapping_sub(b);
    c ^= b >> 15;
    (a, b, c)
}

/// `UINT32_HASH_2(Expr1, Expr2, AConst)` (`erl_term_hashing.c:894-900`): fold two
/// words plus a constant into the accumulator through [`MIX`].
#[inline]
fn hash2_pair(hash: u32, e1: u32, e2: u32, con: u32) -> u32 {
    mix(con.wrapping_add(e1), con.wrapping_add(e2), hash)
}

/// `UINT32_HASH(Expr, AConst)` — [`hash2_pair`] with a zero second word.
#[inline]
fn hash2_one(hash: u32, e: u32, con: u32) -> u32 {
    hash2_pair(hash, e, 0, con)
}

/// `SINT32_HASH(Expr, AConst)` (`erl_term_hashing.c:904-912`): a negative value is
/// deliberately mixed twice (as `|y|` then as `y`).
#[inline]
fn hash2_sint32(hash: u32, y: i32, con: u32) -> u32 {
    let hash = if y < 0 {
        hash2_one(hash, y.unsigned_abs(), con)
    } else {
        hash
    };
    hash2_one(hash, y as u32, con)
}

/// `NOT_SSMALL28_HASH(SMALL)` (`erl_term_hashing.c:916-931`): the two 32-bit halves
/// of `|SMALL|` under the sign-selected constant.
#[inline]
fn hash2_not_ssmall28(hash: u32, s: i64) -> u32 {
    let (con, t) = if s < 0 {
        (HCONST_10, s.unsigned_abs())
    } else {
        (HCONST_11, s as u64)
    };
    hash2_pair(hash, (t & 0xFFFF_FFFF) as u32, (t >> 32) as u32, con)
}

/// `IS_SSMALL28(x)` (`erl_term_hashing.c:914`): `x ∈ [-2^27, 2^27)`.
#[inline]
fn is_ssmall28(x: i64) -> bool {
    let shifted = x >> 27;
    shifted == 0 || shifted == -1
}

/// The little-endian `u32` load of `k[0..4)` used by [`block_hash`].
#[inline]
fn le_u32(k: &[u8]) -> u32 {
    u32::from(k[0]) | (u32::from(k[1]) << 8) | (u32::from(k[2]) << 16) | (u32::from(k[3]) << 24)
}

/// `block_hash` (`erl_term_hashing.c:449-521`): the 12-byte unrolled Jenkin's block
/// hash — the payload path shared by binaries and bitstrings.
fn block_hash(bytes: &[u8], initval: u32) -> u32 {
    let mut a = HCONST;
    let mut b = HCONST;
    let mut c = initval;
    let full = bytes.len() - (bytes.len() % 12);
    let mut k = 0;
    while k < full {
        a = a.wrapping_add(le_u32(&bytes[k..]));
        b = b.wrapping_add(le_u32(&bytes[k + 4..]));
        c = c.wrapping_add(le_u32(&bytes[k + 8..]));
        (a, b, c) = mix3(a, b, c);
        k += 12;
    }
    // `block_hash_final_bytes`: the length is folded into `c`, then the ≤11 tail
    // bytes fall through the reversed switch (`l == 11` ≡ `l >= 11`).
    let fin = &bytes[full..];
    c = c.wrapping_add(bytes.len() as u32);
    let l = fin.len();
    if l >= 11 {
        c = c.wrapping_add(u32::from(fin[10]) << 24);
    }
    if l >= 10 {
        c = c.wrapping_add(u32::from(fin[9]) << 16);
    }
    if l >= 9 {
        c = c.wrapping_add(u32::from(fin[8]) << 8);
    }
    if l >= 8 {
        b = b.wrapping_add(u32::from(fin[7]) << 24);
    }
    if l >= 7 {
        b = b.wrapping_add(u32::from(fin[6]) << 16);
    }
    if l >= 6 {
        b = b.wrapping_add(u32::from(fin[5]) << 8);
    }
    if l >= 5 {
        b = b.wrapping_add(u32::from(fin[4]));
    }
    if l >= 4 {
        a = a.wrapping_add(u32::from(fin[3]) << 24);
    }
    if l >= 3 {
        a = a.wrapping_add(u32::from(fin[2]) << 16);
    }
    if l >= 2 {
        a = a.wrapping_add(u32::from(fin[1]) << 8);
    }
    if l >= 1 {
        a = a.wrapping_add(u32::from(fin[0]));
    }
    mix3(a, b, c).2
}

/// `make_hash2(term)` (`erl_term_hashing.c:1598-1601`) — the full 32-bit hash, as
/// `make_hash`'s `MAP_DEF` rule needs it.
///
/// # Errors
///
/// [`unsupported_bif`] for a tag class this crate cannot reproduce exactly (a
/// record, an internal reference, a local fun) and [`BeamError::TermNotFound`] for
/// an atom index absent from `atoms` — never a wrong hash.
fn make_hash2(term: Eterm, atoms: &AtomTable) -> Result<u32> {
    hash2_term(term, atoms, 0)
}

/// The `make_hash2_helper` walk (`erl_term_hashing.c:1045-1580`) from accumulator
/// `h`. Lists are walked iteratively; only *nested* containers recurse.
fn hash2_term(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    if beam_term::is_nil(term) {
        // `NIL_DEF` is `0x02` (`erl_term.h:1443`); the bare constant is the oracle's
        // `3468870702UL` fast path for an empty accumulator.
        return Ok(if h == 0 {
            3_468_870_702
        } else {
            hash2_one(h, 0x02, HCONST_2)
        });
    }
    if beam_term::is_small(term) {
        let s = beam_term::signed_value(term);
        return Ok(if is_ssmall28(s) {
            hash2_sint32(h, s as i32, HCONST)
        } else {
            hash2_not_ssmall28(h, s)
        });
    }
    if beam_term::is_atom(term) {
        let name = atoms
            .name(beam_term::atom_val(term))
            .ok_or(BeamError::TermNotFound)?;
        let hv = atom_hvalue(name);
        return Ok(if h == 0 { hv } else { hash2_one(h, hv, HCONST_3) });
    }
    if access::is_list(term) {
        return hash2_list(term, atoms, h);
    }
    if access::is_tuple(term) {
        let arity = access::tuple_size(term);
        let mut hash = hash2_one(h, arity as u32, HCONST_9);
        for i in 0..arity {
            hash = hash2_term(access::tuple_element(term, i), atoms, hash)?;
        }
        return Ok(hash);
    }
    if access::is_float(term) {
        let value = access::float_value(term);
        // The C forces a positive zero (`erts_get_positive_zero_float`), so `-0.0`
        // hashes like `+0.0`; the words are `(high, low)` on a little-endian host.
        let bits = if value == 0.0 { 0 } else { value.to_bits() };
        return Ok(hash2_pair(h, (bits >> 32) as u32, bits as u32, HCONST_12));
    }
    if access::is_big(term) {
        let (negative, digits) = beam_codec::term::big_parts(term)?;
        let con = if negative { HCONST_10 } else { HCONST_11 };
        let mut hash = h;
        for &d in digits {
            hash = hash2_pair(hash, (d & 0xFFFF_FFFF) as u32, (d >> 32) as u32, con);
        }
        return Ok(hash);
    }
    if access::is_bitstring(term) {
        return hash2_bitstring(term, h);
    }
    if access::is_internal_pid(term) {
        // `internal_pid_number` is the 28-bit `_GET_PID_NUM` (`erl_term.h:652,646`).
        return Ok(hash2_one(h, beam_term::pid_number(term) as u32, HCONST_5));
    }
    if access::is_external_pid(term) {
        let (_, number, _, _) = beam_codec::term::external_pid_parts(term)?;
        return Ok(hash2_one(h, number, HCONST_5));
    }
    if access::is_internal_port(term) {
        return Ok(hash2_port(h, beam_term::port_number(term)));
    }
    if access::is_external_port(term) {
        let (_, number, _) = beam_codec::term::external_port_parts(term)?;
        return Ok(hash2_port(h, number));
    }
    if access::is_external_ref(term) {
        // `EXTERNAL_REF_SUBTAG`: only the first id word is hashed.
        let (_, ids, _) = beam_codec::term::external_ref_parts(term)?;
        let first = *ids.first().ok_or_else(badarg)?;
        return Ok(hash2_one(h, first, HCONST_7));
    }
    if access::is_function(term) {
        return hash2_fun(term, atoms, h);
    }
    if access::is_map(term) {
        return hash2_map(term, atoms, h);
    }
    // Record, internal reference and every other tag class: `make_hash2` reads
    // state this tree's terms do not carry (see the module docs).
    Err(unsupported_bif())
}

/// The `TAG_PRIMARY_LIST` rule (`erl_term_hashing.c:1047-1076`).
///
/// A run of byte-sized heads is packed four-at-a-time into `sh` and folded with
/// `HCONST_4` (the "optimization for strings"), each run's accumulator being local
/// to its list level. The first non-byte head is hashed in place; the remaining CDR
/// chain continues the same level.
fn hash2_list(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    let mut hash = h;
    let mut cur = term;
    loop {
        let mut c: u32 = 0;
        let mut sh: u32 = 0;
        let mut cell = cur;
        let head = loop {
            let car = access::car(cell);
            if !beam_term::is_byte(car) {
                break car;
            }
            sh = sh
                .wrapping_shl(8)
                .wrapping_add(beam_term::unsigned_value(car) as u32);
            if c == 3 {
                hash = hash2_one(hash, sh, HCONST_4);
                c = 0;
                sh = 0;
            } else {
                c += 1;
            }
            let tail = access::cdr(cell);
            if !access::is_list(tail) {
                if c > 0 {
                    hash = hash2_one(hash, sh, HCONST_4);
                }
                return hash2_term(tail, atoms, hash);
            }
            cell = tail;
        };
        if c > 0 {
            hash = hash2_one(hash, sh, HCONST_4);
        }
        // The head is not a byte: hash it, then continue with the rest of this level.
        hash = hash2_term(head, atoms, hash)?;
        let rest = access::cdr(cell);
        if !access::is_list(rest) {
            return hash2_term(rest, atoms, hash);
        }
        cur = rest;
    }
}

/// The `BIN_REF`/`HEAP_BITS`/`SUB_BITS` rule (`erl_term_hashing.c:1262-1404`).
///
/// The significant bytes (MSB-first packing, as [`access::bitstring_bits`] yields)
/// go through [`block_hash`] with `HCONST_13 + hash` as the initial value; the
/// trailing `bitsize` bits are then folded with `HCONST_15`.
fn hash2_bitstring(term: Eterm, h: u32) -> Result<u32> {
    let con = HCONST_13.wrapping_add(h);
    let bit_len = access::bitstring_bit_size(term)?;
    let packed = access::bitstring_bits(term)?;
    let sz = bit_len / 8;
    let bitsize = bit_len % 8;
    if sz == 0 && bitsize == 0 {
        return Ok(con);
    }
    let mut hash = block_hash(&packed[..sz], con);
    if bitsize > 0 {
        let b = u32::from(packed[sz] >> (8 - bitsize));
        hash = hash2_pair(hash, bitsize as u32, b, HCONST_15);
    }
    Ok(hash)
}

/// The `PORT_DEF`/`EXTERNAL_PORT_DEF` rule (`erl_term_hashing.c:1468-1474, 1503-1509`).
fn hash2_port(h: u32, number: u64) -> u32 {
    hash2_pair(h, number as u32, (number >> 32) as u32, HCONST_6)
}

/// The `FUN_SUBTAG` rule (`erl_term_hashing.c:1220-1260`).
///
/// An **external** fun uses `(arity, hvalue(module))` with `HCONST` then
/// `hvalue(function)` with `HCONST_14`; an **imported** (`NEW_FUN_EXT`) fun uses
/// `(num_free, hvalue(module))` then `(index, old_uniq)` with `HCONST`, then each
/// `env[k]` in index order. A **local** (`make_fun3`) fun's `module`/`old_uniq` are
/// absent from this tree's descriptor, so it raises `unsupported_bif`.
fn hash2_fun(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    match fun_kind_term(term)? {
        FunKind::External => {
            let arity = fun_arity_term(term)? as u32;
            let module = atoms
                .name(fun_module(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let function = atoms
                .name(fun_function(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let hash = hash2_pair(h, arity, atom_hvalue(module), HCONST);
            Ok(hash2_one(hash, atom_hvalue(function), HCONST_14))
        }
        FunKind::Imported => {
            let num_free = fun_num_free_term(term)? as u32;
            let module = atoms
                .name(fun_module(term)?)
                .ok_or(BeamError::TermNotFound)?;
            let index = fun_index(term)?;
            let old_uniq = fun_old_uniq(term)?;
            let mut hash = hash2_pair(h, num_free, atom_hvalue(module), HCONST);
            hash = hash2_pair(hash, index, old_uniq, HCONST);
            for &e in fun_env(term)? {
                hash = hash2_term(e, atoms, hash)?;
            }
            Ok(hash)
        }
        FunKind::Local => Err(unsupported_bif()),
    }
}

/// The `MAP_SUBTAG` rule (`erl_term_hashing.c:1130-1216`).
///
/// The map size is folded with `HCONST_16`, then each `(key, value)` pair is hashed
/// *independently* (starting from `0`) and the results are xor-ed together and
/// folded with `HCONST_19` — so the result is deliberately independent of pair
/// order, and a flat map and a hashmap of the same pairs agree.
fn hash2_map(term: Eterm, atoms: &AtomTable, h: u32) -> Result<u32> {
    let pairs = beam_codec::term::map_pairs_storage_order(term)?;
    let hash = hash2_one(h, pairs.len() as u32, HCONST_16);
    if pairs.is_empty() {
        return Ok(hash);
    }
    let mut xor: u32 = 0;
    for (key, value) in &pairs {
        let mut pair_hash = hash2_term(*key, atoms, 0)?;
        pair_hash = hash2_term(*value, atoms, pair_hash)?;
        xor ^= pair_hash;
    }
    Ok(hash2_one(hash, xor, HCONST_19))
}

// ---------------------------------------------------------------------------
// The BIF
// ---------------------------------------------------------------------------

/// `phash_2`'s `Range` argument (`bif.c:5376-5385`) — the `Uint32` modulus, or `0`
/// for the special `2^32` range.
///
/// # Errors
///
/// `badarg` for anything that is not an integer `u` with `1 <= u <= 0xFFFF_FFFF`
/// (`term_to_Uint` followed by the C's `((u >> 16) >> 16) != 0 || !u` check) — a
/// negative or too-large integer, a float, an atom, a list, `Range = 0`. The value
/// `2^32` is *accepted* and mapped to `0` (`term_equals_2pow32`).
fn range_of(term: Eterm) -> Result<u32> {
    let value: u64 = if beam_term::is_small(term) {
        let v = beam_term::signed_value(term);
        if v < 0 {
            return Err(badarg());
        }
        v as u64
    } else if access::is_big(term) {
        let (negative, digits) = beam_codec::term::big_parts(term)?;
        if negative {
            return Err(badarg());
        }
        // The magnitude with any (non-canonical) leading zero limbs dropped.
        let significant = match digits.iter().rposition(|&d| d != 0) {
            Some(top) => &digits[..=top],
            None => &digits[..0],
        };
        match significant {
            [] => 0,
            [single] => *single,
            // More than one significant limb is far beyond 2^32 (and u64).
            _ => return Err(badarg()),
        }
    } else {
        return Err(badarg());
    };

    if value == (1u64 << 32) {
        return Ok(0);
    }
    if (1..=0xFFFF_FFFF).contains(&value) {
        return Ok(value as u32);
    }
    Err(badarg())
}

/// `erlang:phash(Term, Range)` (`bif.tab:82`, `phash_2`).
///
/// # Errors
///
/// `badarg` for an invalid `Range` and the `hash_term` errors above.
fn phash_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = range_of(args[1])?;
    let hash = hash_term(args[0], ctx.atoms(), 0)?;
    // `range != 0` ⇒ `1 + (hash % range)`, i.e. `[1..range]`; the `2^32` range
    // (`range == 0`) ⇒ `hash + 1` (one past the top of the `u32` space, promoted
    // exactly as the C `erts_mixed_plus` does).
    let final_hash: u64 = if range != 0 {
        u64::from(hash % range) + 1
    } else {
        u64::from(hash) + 1
    };
    arith::int_from_i64(ctx, final_hash as i64)
}

/// Registers `erlang:phash/2` and `erlang:phash2/1,2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "phash", 2, phash_2);
    reg.register("erlang", "phash2", 1, phash2_1);
    reg.register("erlang", "phash2", 2, phash2_2);
}

/// `erlang:phash2(Term)` (`bif.tab:83`, `phash2_1`): `make_hash2(Term)` reduced to
/// the low 27 bits (`bif.c:5407`).
///
/// # Errors
///
/// The `make_hash2` walk errors above.
fn phash2_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let hash = make_hash2(args[0], ctx.atoms())?;
    // `make_small(hash & ((1L << 27) - 1))` — always a small integer.
    ctx.int(i64::from(hash & 0x07FF_FFFF))
}

/// `erlang:phash2(Term, Range)` (`bif.tab:84`, `phash2_2`): `make_hash2(Term)`
/// modulo `Range`, in `[0 .. Range-1]` (no `+1`, unlike `phash/2`); the special
/// `2^32` range returns the whole hash (`bif.c:5434-5438`).
///
/// # Errors
///
/// `badarg` for an invalid `Range` and the `make_hash2` walk errors above.
fn phash2_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = range_of(args[1])?;
    let hash = make_hash2(args[0], ctx.atoms())?;
    let final_hash = if range != 0 { hash % range } else { hash };
    ctx.int(i64::from(final_hash))
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pieces `../../tests/phash.rs` cannot reach directly: the
    //! `atom_hash` hashpjw (including the latin1 clutch), the bit-exactness of the
    //! wrapping `step` helper, the `MIX` rounds, the `block_hash` fold and the
    //! `IS_SSMALL28` boundaries. The oracle-pinned end-to-end values live in the
    //! integration suite.

    use super::*;

    #[test]
    fn step_folds_bytes_most_significant_last() {
        // `(((0*F1 + 0x11) * F1 + 0x22) * F1 + 0x33) * F1 + 0x44`, wrapping.
        let expected = 0x11u32
            .wrapping_mul(F1)
            .wrapping_add(0x22)
            .wrapping_mul(F1)
            .wrapping_add(0x33)
            .wrapping_mul(F1)
            .wrapping_add(0x44);
        assert_eq!(step(0, 0x4433_2211, F1), expected);
    }

    #[test]
    fn atom_hvalue_matches_hashpjw_for_ascii() {
        // Hand-computed hashpjw for "abc" (3 bytes, no clutch triggered).
        let mut h: u64 = 0;
        for v in b"abc" {
            h = (h << 4) + u64::from(*v);
            let g = h & 0xf000_0000;
            if g != 0 {
                h ^= g >> 24;
                h ^= g;
            }
        }
        assert_eq!(atom_hvalue("abc"), (h & 0xFFFF_FFFF) as u32);
        // The empty name hashes to 0 (no iterations).
        assert_eq!(atom_hvalue(""), 0);
    }

    #[test]
    fn atom_hvalue_applies_the_latin1_clutch() {
        // U+0080..U+00FF is a two-byte UTF-8 sequence (0xC2/0xC3 + 0x80..0xBF) that
        // the r16 clutch collapses into the single latin1 byte it encodes.
        for (name, latin1) in [("\u{80}", 0x80u8), ("\u{bf}", 0xBF), ("\u{c0}", 0xC0), ("\u{ff}", 0xFF)]
        {
            let mut expected: u64 = 0;
            expected = (expected << 4) + u64::from(latin1);
            assert_eq!(atom_hvalue(name), (expected & 0xFFFF_FFFF) as u32, "{name}");
        }
        // A lead byte outside 0xC2/0xC3 is hashed as its own raw bytes: U+0100 is
        // `0xC4 0x80`, so both bytes fold in separately.
        let mut expected: u64 = 0;
        for v in [0xC4u8, 0x80] {
            expected = (expected << 4) + u64::from(v);
            let g = expected & 0xf000_0000;
            if g != 0 {
                expected ^= g >> 24;
                expected ^= g;
            }
        }
        assert_eq!(atom_hvalue("\u{100}"), (expected & 0xFFFF_FFFF) as u32);
    }

    #[test]
    fn mix_matches_the_c_rounds() {
        // The C `MIX(a,b,c)` (`erl_term_hashing.c:428-439`) mutates all three of its
        // lvalues; `mix` returns only the final `c` and `mix3` returns the whole
        // triple. Both values below are the C result for `(a,b,c) = (1,2,3)`.
        assert_eq!(mix(1, 2, 3), 3_082_062_082);
        assert_eq!(mix3(1, 2, 3), (120_349_509, 1_098_030_347, 3_082_062_082));
    }

    #[test]
    fn block_hash_matches_the_c_twelve_byte_fold() {
        // `payload[i] = (i*7+3) mod 256`. `HCONST_13` is the initial value the
        // binary/bitstring rule passes (`hash2_bitstring`), and the low 27 bits of
        // the result are the oracle's `erlang:phash2/1` value (43353299 for 12
        // bytes, 101390983 for 13 bytes). The 12-byte input exercises the unrolled
        // loop once (carrying the mutated `a`/`b` into the tail fold); the 13-byte
        // input adds one tail byte to the carried `a`.
        let p12 = [3u8, 10, 17, 24, 31, 38, 45, 52, 59, 66, 73, 80];
        let p13 = [3u8, 10, 17, 24, 31, 38, 45, 52, 59, 66, 73, 80, 87];
        assert_eq!(block_hash(&p12, HCONST_13), 3_398_796_499);
        assert_eq!(block_hash(&p12, HCONST_13) & 0x07FF_FFFF, 43_353_299);
        assert_eq!(block_hash(&p13, HCONST_13), 504_044_167);
        assert_eq!(block_hash(&p13, HCONST_13) & 0x07FF_FFFF, 101_390_983);
    }

    #[test]
    fn is_ssmall28_has_the_documented_boundaries() {
        // `IS_SSMALL28(x)` (`erl_term_hashing.c:914`): `x ∈ [-2^27, 2^27)`.
        assert!(is_ssmall28(0));
        assert!(is_ssmall28((1 << 27) - 1));
        assert!(!is_ssmall28(1 << 27));
        assert!(is_ssmall28(-(1 << 27)));
        assert!(is_ssmall28(-((1 << 27) - 1)));
        assert!(!is_ssmall28(-(1 << 27) - 1));
    }
}
