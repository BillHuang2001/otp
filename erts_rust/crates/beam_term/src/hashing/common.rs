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

//! Shared low-level primitives for the two bit-exact ERTS hash ports.
//!
//! Everything here is a faithful transcription of the helper layer in
//! `erts/emulator/beam/erl_term_hashing.c` that the `make_hash2` and `make_hash`
//! ports build on: the `FUNNY_NUMBER*`/`HCONST*` constants, the
//! Bob-Jenkins `MIX`/`block_hash` primitives, the
//! `UINT32_HASH*`/`SINT32_HASH`/`UINT32_HASH_STEP` mixing steps, the byte-level
//! bitstring accessors and the `make_hash` work-stack entry types. The two ports
//! themselves live in the sibling modules and consume these.
//!
//! Error handling note (shared by both ports): a well-formed term always hashes.
//! The only *reachable* typed failure for a well-formed term is a `FunKind::Local`
//! fun (a `make_fun3` closure), whose inlined 2-word descriptor carries neither the
//! module nor `old_uniq` that the C's `ErlFunEntry` supplies — see [`crate::fun`].
//! That is reported as the dedicated `TermError::FunIdentityUnavailable` (the term
//! is fine; the identity is not carried), never as a substituted value. A word that
//! is not a term at all (a bare header word, e.g. the ABI's `THE_NON_VALUE`) is
//! reported as `TermError::NotATerm` rather than dereferenced. Every other `Err` in
//! the ports is for a forged/unknown term layout (unreachable through the safe API)
//! and stays `TermError::BadFun`, whose meaning — "this is not a well-formed fun" —
//! is exactly right there.

use crate::bitstring::bitstring_parts;
use crate::boxed::{boxed_val, make_boxed, ERL_BIN_REF_SIZE};
use crate::eterm::Eterm;

// `FUNNY_NUMBER1..15` (`erl_term_hashing.c:98-112`) — the primes just above 2^28
// used by `make_hash`.
pub(super) const FN1: u32 = 268_440_163;
pub(super) const FN2: u32 = 268_439_161;
pub(super) const FN3: u32 = 268_435_459;
pub(super) const FN4: u32 = 268_436_141;
pub(super) const FN5: u32 = 268_438_633;
pub(super) const FN6: u32 = 268_437_017;
pub(super) const FN8: u32 = 268_437_511;
pub(super) const FN9: u32 = 268_439_627;
pub(super) const FN10: u32 = 268_440_479;
pub(super) const FN11: u32 = 268_440_577;
pub(super) const FN12: u32 = 268_440_581;
pub(super) const FN13: u32 = 268_440_593;
pub(super) const FN14: u32 = 268_440_611;
pub(super) const FN15: u32 = 268_440_629;

/// `HCONST` (`erl_term_hashing.c:441`).
pub(super) const HCONST: u32 = 0x9e37_79b9;
/// `HCONST_n` = `(HCONST * n) mod 2^32` (`erl_term_hashing.c:867-888`). Only the
/// ones the two algorithms actually use are defined.
pub(super) const HCONST_2: u32 = 0x3c6e_f372;
pub(super) const HCONST_3: u32 = 0xdaa6_6d2b;
pub(super) const HCONST_4: u32 = 0x78dd_e6e4;
pub(super) const HCONST_5: u32 = 0x1715_609d;
pub(super) const HCONST_6: u32 = 0xb54c_da56;
pub(super) const HCONST_7: u32 = 0x5384_540f;
pub(super) const HCONST_9: u32 = 0x8ff3_4781;
pub(super) const HCONST_10: u32 = 0x2e2a_c13a;
pub(super) const HCONST_11: u32 = 0xcc62_3af3;
pub(super) const HCONST_12: u32 = 0x6a99_b4ac;
pub(super) const HCONST_13: u32 = 0x08d1_2e65;
pub(super) const HCONST_14: u32 = 0xa708_a81e;
pub(super) const HCONST_15: u32 = 0x4540_21d7;
pub(super) const HCONST_16: u32 = 0xe377_9b90;
pub(super) const HCONST_19: u32 = 0xbe1e_08bb;
pub(super) const HCONST_23: u32 = 0x36fb_ef9f;

/// `MIX(a, b, c)` (Bob Jenkins, `erl_term_hashing.c:428-439`), returning `c`.
#[inline]
pub(super) fn mix32(a: u32, b: u32, c: u32) -> u32 {
    mix3(a, b, c).2
}

/// [`mix32`] returning all three working variables (`block_hash_buffer` carries
/// `a`/`b`/`c` across iterations).
#[inline]
pub(super) fn mix3(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    a = a.wrapping_sub(b).wrapping_sub(c);
    a ^= c >> 13;
    b = b.wrapping_sub(c).wrapping_sub(a);
    b ^= a << 8;
    c = c.wrapping_sub(a).wrapping_sub(b);
    c ^= b >> 13;

    a = a.wrapping_sub(b).wrapping_sub(c);
    a ^= c >> 12;
    b = b.wrapping_sub(c).wrapping_sub(a);
    b ^= a << 16;
    c = c.wrapping_sub(a).wrapping_sub(b);
    c ^= b >> 5;

    a = a.wrapping_sub(b).wrapping_sub(c);
    a ^= c >> 3;
    b = b.wrapping_sub(c).wrapping_sub(a);
    b ^= a << 10;
    c = c.wrapping_sub(a).wrapping_sub(b);
    c ^= b >> 15;
    (a, b, c)
}

/// `UINT32_HASH_2(e1, e2, AConst)` (`erl_term_hashing.c:894-900`).
#[inline]
pub(super) fn uint32_hash_2(hash: u32, e1: u32, e2: u32, k: u32) -> u32 {
    mix32(k.wrapping_add(e1), k.wrapping_add(e2), hash)
}

/// `UINT32_HASH(e, AConst)`.
#[inline]
pub(super) fn uint32_hash(hash: u32, e: u32, k: u32) -> u32 {
    uint32_hash_2(hash, e, 0, k)
}

/// `SINT32_HASH(y, AConst)` (`erl_term_hashing.c:904-912`) — a negative value is
/// mixed twice (once with `-y`, once with `y`).
#[inline]
pub(super) fn sint32_hash(mut hash: u32, y: i32, k: u32) -> u32 {
    if y < 0 {
        hash = uint32_hash(hash, y.unsigned_abs(), k);
    }
    uint32_hash(hash, y as u32, k)
}

/// `IS_SSMALL28(x)` (`erl_term_hashing.c:914`) — `-2^27 <= x < 2^27`.
#[inline]
pub(super) const fn is_ssmall28(x: i64) -> bool {
    ((x >> 27) as u64).wrapping_add(1) < 2
}

/// `NOT_SSMALL28_HASH` (`erl_term_hashing.c:916-931`) — the two-word form for a
/// 60-bit small that does not fit the 28-bit fast path.
#[inline]
pub(super) fn not_ssmall28_hash(hash: u32, small: i64) -> u32 {
    let (con, t) = if small < 0 {
        (HCONST_10, small.unsigned_abs())
    } else {
        (HCONST_11, small as u64)
    };
    let x = (t & 0xffff_ffff) as u32;
    let y = (t >> 32) as u32;
    uint32_hash_2(hash, x, y, con)
}

/// `UINT32_HASH_STEP(x, Prime1)` (`erl_term_hashing.c:184-192`) — one bytewise
/// multiply-accumulate step, least-significant byte first.
#[inline]
pub(super) fn uint32_hash_step(hash: u32, x: u32, prime: u32) -> u32 {
    let h = hash
        .wrapping_mul(prime)
        .wrapping_add(x & 0xff)
        .wrapping_mul(prime)
        .wrapping_add((x >> 8) & 0xff)
        .wrapping_mul(prime)
        .wrapping_add((x >> 16) & 0xff)
        .wrapping_mul(prime);
    h.wrapping_add(x >> 24)
}

/// The four little-endian bytes at `k[..4]` as a `u32`.
#[inline]
pub(super) fn le32(k: &[u8]) -> u32 {
    u32::from(k[0])
        | (u32::from(k[1]) << 8)
        | (u32::from(k[2]) << 16)
        | (u32::from(k[3]) << 24)
}

/// `block_hash(block, block_length, initval)` (`erl_term_hashing.c:503-521`) —
/// the Jenkins 12-byte block hash, exactly as written (note the shift layout of
/// the final `c` contribution: its low byte is reserved for the length).
pub(super) fn block_hash(bytes: &[u8], initval: u32) -> u32 {
    const BPI: usize = 12;
    let block_length = bytes.len();
    let not_in_loop = block_length % BPI;
    let in_loop = block_length - not_in_loop;

    let (mut a, mut b, mut c) = (HCONST, HCONST, initval);
    let mut k = &bytes[..in_loop];
    while k.len() >= BPI {
        let (na, nb, nc) = mix3(
            a.wrapping_add(le32(&k[0..])),
            b.wrapping_add(le32(&k[4..])),
            c.wrapping_add(le32(&k[8..])),
        );
        a = na;
        b = nb;
        c = nc;
        k = &k[BPI..];
    }

    let fb = &bytes[in_loop..];
    let len = fb.len();
    c = c.wrapping_add(block_length as u32);
    if len >= 1 {
        a = a.wrapping_add(u32::from(fb[0]));
    }
    if len >= 2 {
        a = a.wrapping_add(u32::from(fb[1]) << 8);
    }
    if len >= 3 {
        a = a.wrapping_add(u32::from(fb[2]) << 16);
    }
    if len >= 4 {
        a = a.wrapping_add(u32::from(fb[3]) << 24);
    }
    if len >= 5 {
        b = b.wrapping_add(u32::from(fb[4]));
    }
    if len >= 6 {
        b = b.wrapping_add(u32::from(fb[5]) << 8);
    }
    if len >= 7 {
        b = b.wrapping_add(u32::from(fb[6]) << 16);
    }
    if len >= 8 {
        b = b.wrapping_add(u32::from(fb[7]) << 24);
    }
    if len >= 9 {
        c = c.wrapping_add(u32::from(fb[8]) << 8);
    }
    if len >= 10 {
        c = c.wrapping_add(u32::from(fb[9]) << 16);
    }
    if len >= 11 {
        c = c.wrapping_add(u32::from(fb[10]) << 24);
    }
    mix3(a, b, c).2
}

/// The `(data base, bit offset, bit size)` of any bitstring term, resolving a
/// `BIN_REF` root to the `ErlSubBits` that follows it
/// (`erl_bits.c:2672-2712`: the root of a refc binary is the sub-bits).
///
/// # Safety
///
/// `t` must be a live bitstring (heap bits, sub bits, or a bin ref).
pub(super) unsafe fn bitstring_parts_any(t: Eterm) -> (*const u8, usize, usize) {
    // SAFETY: `t` is a live bitstring per the caller's contract.
    let hdr = unsafe { *boxed_val(t) };
    if hdr == crate::boxed::HEADER_BIN_REF {
        // SAFETY: the ErlSubBits of a refc binary sits `ERL_BIN_REF_SIZE` words
        // above the BinRef this term points at.
        unsafe { bitstring_parts(make_boxed(boxed_val(t).add(ERL_BIN_REF_SIZE))) }
    } else {
        // SAFETY: as above.
        unsafe { bitstring_parts(t) }
    }
}

/// The bit `i` of a bit string whose first bit is the MSB of `base[bitoffs/8]`
/// (`erts_copy_bits_fwd`, ground truth `erl_bits.c`).
#[inline]
pub(super) fn base_bits(base: *const u8, bitoffs: usize, i: usize) -> u8 {
    let bit = bitoffs + i;
    // SAFETY: the caller only asks for bits inside the string.
    (unsafe { *base.add(bit >> 3) } >> (7 - (bit & 7))) & 1
}

/// The virtual tag `tag_val_def` returns (`erl_term.h:1441-1458`), plus the
/// `MAKE_HASH_*_OP` pseudo-ops `make_hash` pushes on its work stack.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum HOp {
    Bitstring,
    List,
    Nil,
    Map,
    RecordInit,
    TupleInit,
    Pid,
    ExternalPid,
    Port,
    ExternalPort,
    Fun,
    Ref,
    ExternalRef,
    Atom,
    Float,
    Big,
    Small,
    /// `MAKE_HASH_TUPLE_OP`.
    Tuple,
    /// `MAKE_HASH_RECORD_OP`.
    Record,
    /// `MAKE_HASH_TERM_ARRAY_OP`.
    TermArray,
    /// `MAKE_HASH_CDR_PRE_OP`.
    CdrPre,
    /// `MAKE_HASH_CDR_POST_OP`.
    CdrPost,
}

/// An entry on `make_hash`'s work stack (the C pushes untyped words).
#[derive(Clone, Copy)]
pub(super) enum HItem {
    Op(HOp),
    Term(Eterm),
    Ptr(*const Eterm),
    Count(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_hash_matches_the_jk_reference_shape() {
        // `block_hash` with an empty block never enters the loop, so it mixes the
        // untouched `(HCONST, HCONST, initval)`; the length added to `c` is 0.
        assert_eq!(block_hash(&[], 0), mix32(HCONST, HCONST, 0));
        assert_eq!(block_hash(&[], 0), 3_175_731_469);

        // A 12-byte block runs exactly one loop iteration. The loop adds
        // `le32(k[0..4])`, `le32(k[4..8])`, `le32(k[8..12])` to `a`/`b`/`c` and
        // then `MIX`es them **inside the loop** (`block_hash_buffer`,
        // `erl_term_hashing.c:457-472`), so the running state after the iteration is
        // `mix3(HCONST+1, HCONST+2, initval+3)`. `block_hash_final_bytes`
        // (`:474-501`) then **always** does `c += full_length` — even with no
        // leftover bytes — before the final `MIX`.
        let b: [u8; 12] = [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0];
        let (a, bb, c) = mix3(HCONST.wrapping_add(1), HCONST.wrapping_add(2), 3);
        assert_eq!(block_hash(&b, 0), mix3(a, bb, c.wrapping_add(12)).2);
        // Pin the raw value: dropping the in-loop `MIX` or the unconditional
        // `c += full_length` would move it.
        assert_eq!(block_hash(&b, 0), 598_152_079);

        // An 11-byte block never enters the loop; its tail byte-adds are the
        // fall-through `case 11 .. case 1` arms (`c += le32 = 3 << 8`, `b += 2`,
        // `a += 1`) on top of `c += full_length (11)`.
        let p: [u8; 11] = [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0];
        let expect_p = mix3(
            HCONST.wrapping_add(1),
            HCONST.wrapping_add(2),
            11u32.wrapping_add(3 << 8),
        )
        .2;
        assert_eq!(block_hash(&p, 0), expect_p);
        assert_eq!(block_hash(&p, 0), 699_853_219);
    }
}
