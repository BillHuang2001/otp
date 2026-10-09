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

//! Off-heap term materialization: heap-agnostic writers and readers for the boxed
//! "thing" families whose interior layout this crate owns.
//!
//! The frozen term layout of a float, a bignum, a flat map and the external
//! pid/port/ref blocks is owned by `beam_term` (the lowest common ancestor for
//! term operations — workspace `CONTEXT.md` §2). The constructors and accessors
//! therefore live here, next to the predicates in [`crate::boxed`].
//!
//! # The heap-agnostic contract
//!
//! `beam_term` is a dependency *sink*: it cannot allocate, because the production
//! allocators (`beam_alloc`) and the copying collector (`beam_gc`) depend
//! downward on it. Every writer here therefore does **not** allocate: the caller
//! reserves the words (from `beam_alloc` in production, [`crate::heap::Heap`] in
//! tests), passes the `hp: *mut Eterm` base of that block, and receives back the
//! tagged root word. The word counts are fixed and exposed as `pub const fn`s
//! (`big_size_words`, `flatmap_size_words`, `external_*_size_words`) so a caller
//! can size its allocation exactly.
//!
//! All layouts are pinned line-by-line to the legacy C ground truth —
//! `erts/emulator/beam/erl_term.h` (`ExternalThing`, `HEADER_FLONUM`),
//! `erts/emulator/beam/big.h` (bignum) and `erts/emulator/beam/erl_map.h` /
//! `erl_map.c:671-714` (flat map) — and are byte-for-byte the layouts produced by
//! `beam_codec::term`, which forwards here. `erts/emulator/beam/external.c`
//! (`make_big`/`make_external_ref`) is the reference for the range invariants.
//!
//! # `unsafe` contract
//!
//! Every function is `unsafe`: an `Eterm` being a live, well-formed term of the
//! expected kind is a caller invariant, and there is no way to read or write a
//! raw heap word safely. Each item documents, in its `# Safety` section, exactly
//! how many words the caller must have reserved, which of them are written, and
//! how long a returned borrowed slice stays valid.

use core::mem::size_of;

use crate::boxed::{arityval, boxed_val, make_arityval, make_boxed, make_header, HEADER_FLONUM};
use crate::eterm::Eterm;
use crate::tag::{
    BIG_SIGN_BIT, EXTERNAL_PID_SUBTAG, EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG, MAP_SUBTAG,
    NEG_BIG_SUBTAG, POS_BIG_SUBTAG,
};
use crate::tuple::{make_tuple, tuple0};

/// Number of bytes in one term word (`size_of::<Eterm>()`).
const WORD_BYTES: usize = size_of::<Eterm>();
/// Number of `u32`s that fit in one term word (two on `ARCH_64`).
const U32_PER_WORD: usize = WORD_BYTES / size_of::<u32>();
/// `EXTERNAL_THING_HEAD_SIZE` (`erl_term.h:1161`):
/// `offsetof(ExternalThing, data) / sizeof(Uint)` — the `[header, node, next]`
/// head, three words on `ARCH_64`.
const EXTERNAL_THING_HEAD_SIZE: usize = 3;

/// Number of words in a float block: `[HEADER_FLONUM, f64 bits]`
/// (`erl_term.h:479`).
pub const FLOAT_SIZE_WORDS: usize = 2;
/// `ERTS_MAX_REF_NUMBERS` (`erl_term.h:780`): the largest external-reference id
/// count [`write_external_ref`] accepts (and therefore the largest
/// [`external_ref_parts`] reports).
pub const MAX_REF_IDS: usize = 5;

// The frozen term layout is the 64-bit `ARCH_64` ABI of the C ground truth:
// `ErtsDigit` is `Uint` = one `Eterm` word (`big.h:29`) and the
// external-identifier data union packs two `Uint32`s per word (`erl_term.h:1156`).
const _: () = assert!(WORD_BYTES == 8, "the frozen term layout assumes ARCH_64");

// ---------------------------------------------------------------------------
// Float
// ---------------------------------------------------------------------------

/// Writes a float block at `hp` and returns the tagged root term.
///
/// Layout pinned to `erl_term.h:479` (`HEADER_FLONUM = make_header(1, FLOAT_SUBTAG)`)
/// and `erl_term.h:506` (`PUT_DOUBLE`): the two words `[HEADER_FLONUM, f64 bits]`,
/// with the root word `make_boxed(hp)` pointing at the first of them.
///
/// # Safety
///
/// `hp` must point at `FLOAT_SIZE_WORDS` (2) exclusively-owned, writable words.
/// Both are written by this call and must stay live (and not be moved) for as
/// long as the returned term is used.
pub unsafe fn write_float(hp: *mut Eterm, value: f64) -> Eterm {
    // SAFETY: the caller reserved the 2-word `[HEADER_FLONUM, bits]` block at `hp`;
    // both words are written before the pointer escapes.
    unsafe {
        *hp = HEADER_FLONUM;
        *hp.add(1) = Eterm::from_raw(value.to_bits() as usize);
    }
    make_boxed(hp)
}

/// Reads the `f64` value of a float term.
///
/// The exact inverse of [`write_float`]. The float payload layout is owned by
/// [`crate::bitstring`] (the crate's single public definition — see
/// [`crate::bitstring::float_value`]); this is a re-export under `beam_term::thing`
/// so the float accessor surface here stays unchanged.
///
/// # Safety
///
/// `t` must be a live boxed float (`is_float(t)`), i.e. the `[HEADER_FLONUM, bits]`
/// block written by [`write_float`], kept alive by the caller.
pub use crate::bitstring::float_value;

/// Reads the raw `f64` bit pattern of a float term.
///
/// The `f64` bits are the word following the float header (`erl_term.h:479`,
/// `erl_term.h:506`); see [`crate::bitstring::float_bits`] (the crate's single
/// public definition), re-exported here.
///
/// # Safety
///
/// As [`float_value`]: `t` must be a live boxed float.
pub use crate::bitstring::float_bits;

// ---------------------------------------------------------------------------
// Bignum
// ---------------------------------------------------------------------------

/// `BIG_NEED_SIZE(n)` (`big.h:97`) — the words a bignum with `n_digits`
/// `ErtsDigit`s occupies: one header word plus one word per digit.
pub const fn big_size_words(n_digits: usize) -> usize {
    n_digits + 1
}

/// Writes a bignum block at `hp` and returns the tagged root term.
///
/// `digits` are `ErtsDigit`s (`big.h:29`: `typedef Uint ErtsDigit`, exactly one
/// machine word — `u64` on `ARCH_64`), **least significant first**; `negative`
/// sets the header's `_BIG_SIGN_BIT`. Layout pinned to `erl_term.h:452-471` and
/// `big.h:68-73,96-97`: `[make_pos_bignum_header(n) | make_neg_bignum_header(n),
/// d0 … d(n-1)]`, one digit per word, with the root word `make_boxed(hp)`.
///
/// The header arity field **is** the digit count `n` — the C reads it back
/// directly (`big.h:70`: `BIG_ARITY(xp) = bignum_header_arity(*(xp))`), and every
/// producer stores the count, not a count-minus-one (`big.c:2274-2277`
/// `uint_to_big`: `make_pos_bignum_header(1)` for one digit; `big.c:3360`
/// `arity = BIG_NEED_SIZE(xl)-1` = `xl` = the digit count).
///
/// # Safety
///
/// `hp` must point at [`big_size_words`]`(digits.len())` exclusively-owned,
/// writable words, which are all written by this call and must stay live (and
/// unmoved) for as long as the returned term is used. `digits` must be
/// **non-empty**: a zero-width bignum is illegal (`external.c:4474-4486` strips
/// leading zeroes precisely to avoid building one; `big.h:97`
/// `BIG_NEED_SIZE(n) = n + 1`).
pub unsafe fn write_big(hp: *mut Eterm, negative: bool, digits: &[u64]) -> Eterm {
    debug_assert!(!digits.is_empty(), "a bignum needs at least one ErtsDigit");
    let n = digits.len();
    let subtag = if negative {
        NEG_BIG_SUBTAG
    } else {
        POS_BIG_SUBTAG
    };
    // SAFETY: the caller reserved `1 + n` words. Word `0` receives the bignum header
    // (arity `n`, the digit count) and words `1..=n` the digits in
    // least-significant-first order (`BIG_V(xp) = (ErtsDigit *)((xp) + 1)`, `big.h:68`).
    unsafe {
        *hp = make_header(n, subtag);
        for (i, &digit) in digits.iter().enumerate() {
            *hp.add(1 + i) = Eterm::from_raw(digit as usize);
        }
    }
    make_boxed(hp)
}

/// Splits a bignum term into `(negative, digits)`.
///
/// `digits` are `ErtsDigit`s (`u64`), least significant first, borrowed from the
/// term's storage — the exact inverse of [`write_big`] (`big.h:59-73`:
/// `BIG_V(xp) = (ErtsDigit *)((xp) + 1)`, `BIG_ARITY(xp) = bignum_header_arity(*xp)`;
/// the sign lives in the header's `_BIG_SIGN_BIT`).
///
/// # Safety
///
/// `t` must be a live bignum written by [`write_big`]. The returned slice borrows
/// the term's storage, so the caller must keep the owning heap alive and unmoved
/// for `'a`.
pub unsafe fn big_parts<'a>(t: Eterm) -> (bool, &'a [u64]) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live bignum, so `p` is its header word.
    let hdr = unsafe { *p };
    // The header arity **is** the digit count (`big.h:70`:
    // `BIG_ARITY(xp) = bignum_header_arity(*(xp))`).
    let n = arityval(hdr);
    // SAFETY: the header encodes the digit count `n` and `write_big` stored the `n`
    // one-word `ErtsDigit`s starting at word one of the block, so the slice is live
    // and in bounds; the caller keeps the owning heap alive for `'a`.
    let digits = unsafe { core::slice::from_raw_parts(p.add(1).cast::<u64>(), n) };
    ((hdr.raw() & BIG_SIGN_BIT) != 0, digits)
}

// ---------------------------------------------------------------------------
// Flat map
// ---------------------------------------------------------------------------

/// The total words a flat map with `n_pairs` pairs occupies.
///
/// `n_pairs == 0` is the special three-word empty block `[head, 0, keys]`;
/// otherwise the block is `[arityval(n), k0 … k(n-1), head, n, keys, v0 … v(n-1)]`
/// (`4 + 2n` words).
pub const fn flatmap_size_words(n_pairs: usize) -> usize {
    if n_pairs == 0 {
        3
    } else {
        4 + 2 * n_pairs
    }
}

/// Writes a small flat map at `hp` and returns the tagged root term.
///
/// Layout pinned to `erl_map.h:181-199` (`MAP_HEADER_FLATMAP = 0x12C`, i.e.
/// `_make_header(4, MAP_SUBTAG)`) and `erl_map.c:673-700`
/// (`erts_map_from_ks_and_vs`): the word block
/// `[arityval(n), k0 … k(n-1), MAP_HEADER_FLATMAP, n, keys, v0 … v(n-1)]`, where
/// `keys = make_tuple(hp)` points back at the base of the keys seq, the flatmap
/// head sits at word `1 + n`, the `size` field is the raw integer `n`, and the
/// root is `make_boxed(hp + 1 + n)`. `n == 0` is the 3-word block
/// `[MAP_HEADER_FLATMAP, 0, tuple0()]` with root `make_boxed(hp)`.
///
/// Duplicate keys are **not** detected (that needs term comparison, owned by
/// [`crate::compare`]); the pair order is preserved as given.
///
/// # Safety
///
/// `hp` must point at [`flatmap_size_words`]`(pairs.len())` exclusively-owned,
/// writable words, which are all written by this call and must stay live (and
/// unmoved) for as long as the returned term is used. `pairs.len()` must be at
/// most [`crate::map::MAP_SMALL_MAP_LIMIT`] (32): a larger map must be a
/// hash map, which the caller builds with [`crate::map::write_map`] (the
/// HAMT/CHAMP builder owned by this crate).
pub unsafe fn write_flatmap(hp: *mut Eterm, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let n = pairs.len();
    debug_assert!(
        n <= crate::map::MAP_SMALL_MAP_LIMIT,
        "a flat map holds at most MAP_SMALL_MAP_LIMIT pairs"
    );
    if n == 0 {
        // SAFETY: the caller reserved the 3-word empty-block words at `hp`; all three
        // are written before the pointer escapes.
        unsafe {
            *hp = make_header(4, MAP_SUBTAG);
            *hp.add(1) = Eterm::from_raw(0);
            *hp.add(2) = tuple0();
        }
        return make_boxed(hp);
    }
    // SAFETY: the caller reserved `4 + 2n` words at `hp`. The keys tuple occupies
    // `hp[0..=n]`, the flatmap head prefix `[head, size, keys]` occupies
    // `hp[1+n..=3+n]`, and the values occupy `hp[4+n..4+2n]` — every write below is
    // in bounds.
    let root = unsafe {
        *hp = make_arityval(n);
        for i in 0..n {
            *hp.add(1 + i) = pairs[i].0;
        }
        *hp.add(1 + n) = make_header(4, MAP_SUBTAG);
        *hp.add(2 + n) = Eterm::from_raw(n);
        *hp.add(3 + n) = make_tuple(hp);
        for j in 0..n {
            *hp.add(4 + n + j) = pairs[j].1;
        }
        hp.add(1 + n)
    };
    make_boxed(root)
}

/// Splits a flat map into its `(keys, values)`.
///
/// Both slices are borrowed from the term's storage and have the same length, in
/// wire order — the exact inverse of [`write_flatmap`]
/// (`erl_map.h:181-199`; the root points at the `flatmap_t` head `[head, size,
/// keys]`, so word `1` is the raw pair count).
///
/// # Safety
///
/// `t` must be a live *flat* map written by [`write_flatmap`]. The returned slices
/// borrow the term's storage, so the caller must keep the owning heap alive and
/// unmoved for `'a`.
pub unsafe fn flatmap_parts<'a>(t: Eterm) -> (&'a [Eterm], &'a [Eterm]) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live flat map, so `p` is its head word
    // and the `[head, size, keys]` prefix (word `1` = the raw pair count) is live.
    let n = unsafe { (*p.add(1)).raw() };
    if n == 0 {
        return (&[], &[]);
    }
    // SAFETY: a well-formed `n`-pair flat map owns `4 + 2n` words and the caller keeps
    // the owning heap alive for `'a`. Relative to the head pointer `p`, word `2` is
    // the keys tuple term and words `3..3+n` are the values — both slices are live and
    // in bounds.
    unsafe {
        let values = core::slice::from_raw_parts(p.add(3), n);
        let keys_tup = *p.add(2);
        let keys = core::slice::from_raw_parts(boxed_val(keys_tup).add(1), n);
        (keys, values)
    }
}

// ---------------------------------------------------------------------------
// External pid / port / ref
// ---------------------------------------------------------------------------

/// `EXTERNAL_PID_HEAP_SIZE` (`erl_term.h:1166`): the three-word head plus one
/// 64-bit `data.pid` word.
pub const fn external_pid_size_words() -> usize {
    EXTERNAL_THING_HEAD_SIZE + 1
}

/// `EXTERNAL_PORT_HEAP_SIZE` (`erl_term.h:1177`): the three-word head plus one
/// 64-bit `data.port.id` word.
pub const fn external_port_size_words() -> usize {
    EXTERNAL_THING_HEAD_SIZE + 1
}

/// `EXTERNAL_REF_HEAP_SIZE(`n_ids`)`: the three-word head plus the `data.ui32`
/// section holding the id count and the `n_ids` identifiers, rounded up to whole
/// words (`bif.c:4818-4835`: `DW = n / (word bytes / 4) + 1`).
pub const fn external_ref_size_words(n_ids: usize) -> usize {
    EXTERNAL_THING_HEAD_SIZE + n_ids / U32_PER_WORD + 1
}

/// Writes an external pid block at `hp` and returns the tagged root term.
///
/// Layout pinned to `erl_term.h:1137-1172` (`ExternalThing`): the three-word head
/// `[header, node, next]` (`EXTERNAL_THING_HEAD_SIZE`) followed by the 64-bit
/// `data.pid` union, so `EXTERNAL_PID_HEAP_SIZE = 4` words and the header is
/// `make_external_pid_header() = make_header(EXTERNAL_PID_HEAP_SIZE - 1, …)`.
/// `number` and `serial` are the two `Uint32`s of `data.pid` packed into the single
/// data word (`num` first, then `ser`), exactly as C lays the struct out.
///
/// **M1 substitution:** in C, `node` is a pointer to a registered node (which
/// carries the sysname *and* the creation) and `next` is the off-heap link
/// (`etp->node = erts_find_or_insert_node(sysname, cre, …)`). There is no node
/// registry yet, so word `1` holds the sysname atom term and word `2` — the unused
/// off-heap-link slot — holds `creation`. The word positions and arity are exactly
/// C's; only the `node`/`next` pointer semantics are deferred.
///
/// # Safety
///
/// `hp` must point at [`external_pid_size_words`] (4) exclusively-owned, writable
/// words, which are all written by this call and must stay live (and unmoved) for
/// as long as the returned term is used. `node`, if boxed, must itself be a live
/// term.
pub unsafe fn write_external_pid(
    hp: *mut Eterm,
    node: Eterm,
    number: u32,
    serial: u32,
    creation: u32,
) -> Eterm {
    // SAFETY: the caller reserved the 4-word `[header, node, creation, data]` block at
    // `hp`; all four words are written before the pointer escapes. Word `3` is the
    // `data.pid = { Uint32 num; Uint32 ser; }` union, so it carries two native-endian
    // `u32`s and the full 32-bit range of both fields survives.
    unsafe {
        *hp = make_header(EXTERNAL_THING_HEAD_SIZE, EXTERNAL_PID_SUBTAG);
        *hp.add(1) = node;
        *hp.add(2) = Eterm::from_raw(creation as usize);
        let data = hp.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        *data = number;
        *data.add(1) = serial;
    }
    make_boxed(hp)
}

/// Splits an external pid term into `(node, number, serial, creation)`.
///
/// The exact inverse of [`write_external_pid`] (`erl_term.h:1137-1172`): the data
/// word is `data.pid = { num, ser }` and `creation` comes from the `next` slot
/// (the M1 substitution described on [`write_external_pid`]).
///
/// # Safety
///
/// `t` must be a live external pid written by [`write_external_pid`].
pub unsafe fn external_pid_parts(t: Eterm) -> (Eterm, u32, u32, u32) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live 4-word `[header, node, creation,
    // data]` block, so words `1..=3` are live and initialised. The data word holds two
    // native-endian `u32`s.
    unsafe {
        let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        (*p.add(1), *data, *data.add(1), (*p.add(2)).raw() as u32)
    }
}

/// Writes an external port block at `hp` and returns the tagged root term.
///
/// Layout pinned to `erl_term.h:1137-1183` (`ExternalThing`): the three-word head
/// `[header, node, next]` followed by the `data.port` union, whose `id` is a
/// `Uint64` on `ARCH_64` (`erl_term.h:1148-1155`) — so
/// `EXTERNAL_PORT_HEAP_SIZE = 4` words and the header is
/// `make_external_port_header() = make_header(EXTERNAL_PORT_HEAP_SIZE - 1, …)`.
/// The `node`/`next` slots carry the sysname atom and `creation` (the M1
/// substitution described on [`write_external_pid`]); `number` is the 64-bit
/// `V4_PORT_EXT` id.
///
/// # Safety
///
/// `hp` must point at [`external_port_size_words`] (4) exclusively-owned, writable
/// words, which are all written by this call and must stay live (and unmoved) for
/// as long as the returned term is used. `node`, if boxed, must itself be a live
/// term.
pub unsafe fn write_external_port(
    hp: *mut Eterm,
    node: Eterm,
    number: u64,
    creation: u32,
) -> Eterm {
    // SAFETY: the caller reserved the 4-word `[header, node, creation, id]` block at
    // `hp`; all four words are written before the pointer escapes. Word `3` is
    // `data.port.id` (`Uint64` on `ARCH_64`), so the full 64-bit id survives.
    unsafe {
        *hp = make_header(EXTERNAL_THING_HEAD_SIZE, EXTERNAL_PORT_SUBTAG);
        *hp.add(1) = node;
        *hp.add(2) = Eterm::from_raw(creation as usize);
        *hp.add(EXTERNAL_THING_HEAD_SIZE) = Eterm::from_raw(number as usize);
    }
    make_boxed(hp)
}

/// Splits an external port term into `(node, number, creation)`.
///
/// The exact inverse of [`write_external_port`] (`erl_term.h:1137-1183`): the data
/// word is the 64-bit `data.port.id` and `creation` comes from the `next` slot
/// (the M1 substitution described on [`write_external_pid`]).
///
/// # Safety
///
/// `t` must be a live external port written by [`write_external_port`].
pub unsafe fn external_port_parts(t: Eterm) -> (Eterm, u64, u32) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live 4-word `[header, node, creation,
    // id]` block, so words `1..=3` are live and initialised.
    unsafe {
        (
            *p.add(1),
            (*p.add(EXTERNAL_THING_HEAD_SIZE)).raw() as u64,
            (*p.add(2)).raw() as u32,
        )
    }
}

/// Writes an external reference block at `hp` and returns the tagged root term.
///
/// Layout pinned to `erl_term.h:1137-1188` (`ExternalThing`,
/// `make_external_ref_header`) and `bif.c:4810-4848`: the three-word head
/// `[header, node, next]`, then the `data.ui32` array holding the id **count**
/// first (`etp->data.ui32[0] = n`, `bif.c:4840`) followed by the `n` ids. With
/// `DW = n / U32_PER_WORD + 1` (`bif.c:4820`) the data section is `DW` words and
/// `make_external_ref_header(DW) = make_header(DW + EXTERNAL_THING_HEAD_SIZE - 1,
/// …)`, i.e. arity `n / 2 + 3` on `ARCH_64` — exactly the C word shape.
/// `node`/`next` carry the sysname atom and `creation` (the M1 substitution
/// described on [`write_external_pid`]); `ids` are the 32-bit
/// `NEWER_REFERENCE_EXT` identifier words (three on a 64-bit node).
///
/// **`ids` may be empty.** The oracle accepts a zero-id external reference
/// (`binary_to_term(<<131,90,0:16,Node,0:32>>)` succeeds and re-encodes
/// byte-identically), whose block is exactly `[header, node, creation, count = 0]`
/// — 4 words, [`external_ref_size_words`]`(0) == 4`. Nothing else changes: the
/// count word is the `data.ui32` section's first and only word, and copying zero
/// ids is a no-op. Hashing a zero-id reference mixes the (zeroed) word that
/// follows the count, i.e. 0 — see `crate::hashing`.
///
/// # Safety
///
/// `hp` must point at [`external_ref_size_words`]`(ids.len())` exclusively-owned,
/// writable words, which are all written by this call and must stay live (and
/// unmoved) for as long as the returned term is used. `ids` must be at most
/// [`MAX_REF_IDS`] long (`0 <= n <= MAX_REF_IDS`, `erl_term.h:780`); the
/// `data.ui32` array's `u32` slots are `**mut`-written by this call.
pub unsafe fn write_external_ref(hp: *mut Eterm, node: Eterm, ids: &[u32], creation: u32) -> Eterm {
    debug_assert!(
        ids.len() <= MAX_REF_IDS,
        "0 <= reference id count <= MAX_REF_IDS"
    );
    let n = ids.len();
    // `DW = n / (u32s per word) + 1` ref "words" (`bif.c:4818-4835`), i.e. one word
    // per two `u32`s plus the word that holds the id count.
    let data_words = n / U32_PER_WORD + 1;
    let words = EXTERNAL_THING_HEAD_SIZE + data_words;
    // SAFETY: the caller reserved `words = 3 + data_words` words at `hp`: word `0` is
    // the header (arity `words - 1`), word `1` the node, word `2` the creation, and the
    // `data.ui32` array starts at word `EXTERNAL_THING_HEAD_SIZE` with
    // `data_words * U32_PER_WORD` slots. The zeroing initialises the whole data section
    // (including the trailing padding slot of an even id count); the id count and the
    // ids are then written into its front.
    unsafe {
        *hp = make_header(words - 1, EXTERNAL_REF_SUBTAG);
        *hp.add(1) = node;
        *hp.add(2) = Eterm::from_raw(creation as usize);
        let data = hp.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        core::ptr::write_bytes(data, 0, data_words * U32_PER_WORD);
        *data = n as u32;
        core::ptr::copy_nonoverlapping(ids.as_ptr(), data.add(1), n);
    }
    make_boxed(hp)
}

/// Splits an external reference term into `(node, ids, creation)`.
///
/// `ids` is borrowed from the term's storage — the exact inverse of
/// [`write_external_ref`] (`erl_term.h:1137-1188`): the data section starts at word
/// `EXTERNAL_THING_HEAD_SIZE` with the id count as the first `u32`
/// (`external_ref_no_numbers(x) = external_ref_data(x)[0]`, `ARCH_64`) and the ids
/// after it, while `creation` comes from the `next` slot (the M1 substitution
/// described on [`write_external_pid`]).
///
/// # Safety
///
/// `t` must be a live external reference written by [`write_external_ref`]. The
/// returned slice borrows the term's storage, so the caller must keep the owning
/// heap alive and unmoved for `'a`.
pub unsafe fn external_ref_parts<'a>(t: Eterm) -> (Eterm, &'a [u32], u32) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live `3 + data_words`-word block, so the
    // data section at word `EXTERNAL_THING_HEAD_SIZE` — which `write_external_ref`
    // filled with `data_words * U32_PER_WORD` initialised `u32` slots, the id count
    // first — is live and the `n` ids that follow fit it.
    unsafe {
        let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
        let n = *data as usize;
        (
            *p.add(1),
            core::slice::from_raw_parts(data.add(1), n),
            (*p.add(2)).raw() as u32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{
        is_bignum_header, is_external_pid_header, is_external_port_header,
        is_external_ref_header, is_flatmap_header, is_map_header,
    };
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::AtomTable;

    /// A small bump arena, big enough for every block these tests build.
    fn new_heap() -> Heap {
        Heap::new(32)
    }

    #[test]
    fn float_block_layout_and_round_trip() {
        assert_eq!(FLOAT_SIZE_WORDS, 2);
        let mut heap = new_heap();
        let values = [
            0.0f64,
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN,
            f64::MAX,
            core::f64::consts::PI,
        ];
        for value in values {
            heap.reset();
            // SAFETY: `alloc_words` hands out exactly the words this test owns.
            let hp = unsafe { heap.alloc_words(FLOAT_SIZE_WORDS).unwrap() };
            // SAFETY: `hp` points at `FLOAT_SIZE_WORDS` writable words.
            let term = unsafe { write_float(hp, value) };
            assert_eq!(heap.used(), FLOAT_SIZE_WORDS);

            // SAFETY: `term` is the float this test just built on `heap`, so the block
            // `[HEADER_FLONUM, bits]` is live and word `1` is initialised.
            unsafe {
                let p = boxed_val(term);
                assert_eq!(p, hp, "the root points at the block base");
                assert_eq!(*p, HEADER_FLONUM);
                assert_eq!(*p.add(1), Eterm::from_raw(value.to_bits() as usize));
                // Compare by bit pattern so NaN and -0.0 round-trip too.
                assert_eq!(float_bits(term), value.to_bits());
                assert_eq!(float_value(term).to_bits(), value.to_bits());
            }
        }
    }

    #[test]
    fn big_size_words_and_block_layout() {
        assert_eq!(big_size_words(0), 1);
        assert_eq!(big_size_words(1), 2);
        assert_eq!(big_size_words(3), 4);

        let mut heap = new_heap();
        let digits = [0x0A_u64, 0x0B, 0x0C];
        // SAFETY: `alloc_words` hands out exactly `big_size_words(3) == 4` words.
        let hp = unsafe { heap.alloc_words(big_size_words(digits.len())).unwrap() };
        // SAFETY: `hp` points at `big_size_words(3)` writable words; `digits` is non-empty.
        let term = unsafe { write_big(hp, false, &digits) };
        assert_eq!(heap.used(), 4, "header word + one ErtsDigit per word");

        // SAFETY: `term` is the bignum this test just built; word `0` is its header and
        // the digit words follow it (`BIG_V(xp) = (ErtsDigit *)((xp) + 1)`).
        unsafe {
            let p = boxed_val(term);
            assert_eq!(p, hp);
            assert_eq!(*p, make_header(3, POS_BIG_SUBTAG));
            assert!(is_bignum_header(*p));
            assert_eq!(arityval(*p), digits.len());
            assert_eq!((*p).raw() & BIG_SIGN_BIT, 0, "a positive bignum has no sign bit");
            let stored = core::slice::from_raw_parts(p.add(1).cast::<u64>(), digits.len());
            assert_eq!(stored, &digits[..], "digits are stored least-significant-first");
        }

        // SAFETY: `term` is the live bignum built above; `heap` outlives the slice.
        let (negative, got) = unsafe { big_parts(term) };
        assert!(!negative);
        assert_eq!(got, &digits[..]);
    }

    #[test]
    fn big_negative_sign_bit_and_single_digit() {
        let mut heap = new_heap();
        // SAFETY: `alloc_words` hands out `big_size_words(1) == 2` words.
        let hp = unsafe { heap.alloc_words(big_size_words(1)).unwrap() };
        // SAFETY: `hp` points at those 2 writable words; one digit is non-empty.
        let term = unsafe { write_big(hp, true, &[u64::MAX]) };
        assert_eq!(heap.used(), 2, "header + one digit word");

        // SAFETY: `term` is the bignum this test just built; read its header and the
        // single digit word.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(1, NEG_BIG_SUBTAG));
            assert_ne!((*p).raw() & BIG_SIGN_BIT, 0, "the sign lives in the header");
            assert_eq!((*p.add(1)).raw() as u64, u64::MAX);
        }

        // SAFETY: `term` is the live bignum built above; `heap` outlives the slice.
        let (negative, got) = unsafe { big_parts(term) };
        assert!(negative);
        assert_eq!(got, &[u64::MAX]);
    }

    #[test]
    fn flatmap_empty_is_exactly_three_words() {
        assert_eq!(flatmap_size_words(0), 3);
        assert_eq!(flatmap_size_words(2), 8);

        let mut heap = new_heap();
        let base = heap.hp();
        // SAFETY: `alloc_words` hands out exactly the 3-word empty block.
        let hp = unsafe { heap.alloc_words(flatmap_size_words(0)).unwrap() };
        assert_eq!(hp, base);
        // SAFETY: `hp` points at the 3 writable words of the empty block.
        let map = unsafe { write_flatmap(hp, &[]) };
        assert_eq!(heap.used(), 3, "the empty flatmap is 3 words");

        // SAFETY: `base` is the first word of the 3-word block just written.
        unsafe {
            assert_eq!(*base, make_header(4, MAP_SUBTAG));
            assert!(is_map_header(*base));
            assert!(is_flatmap_header(*base));
            assert_eq!((*base.add(1)).raw(), 0);
            assert_eq!(*base.add(2), tuple0());
        }
        assert_eq!(boxed_val(map), base, "root = make_boxed(hp)");
        // SAFETY: `map` is the live empty flatmap built above.
        let (keys, values) = unsafe { flatmap_parts(map) };
        assert!(keys.is_empty());
        assert!(values.is_empty());
    }

    #[test]
    fn flatmap_two_pairs_word_by_word() {
        let mut heap = new_heap();
        let base = heap.hp();
        let k0 = make_small(10);
        let v0 = make_small(100);
        let k1 = make_small(20);
        let v1 = make_small(200);
        // SAFETY: `alloc_words` hands out exactly the `4 + 2n` = 8-word block.
        let hp = unsafe { heap.alloc_words(flatmap_size_words(2)).unwrap() };
        assert_eq!(hp, base);
        // SAFETY: `hp` points at those 8 writable words.
        let map = unsafe { write_flatmap(hp, &[(k0, v0), (k1, v1)]) };
        assert_eq!(heap.used(), 4 + 2 * 2);

        // SAFETY: `base` is the first word of the `4 + 2n` block just written; every
        // index below is within that block.
        unsafe {
            assert_eq!(*base, make_arityval(2)); // word 0: keys-tuple arity
            assert_eq!(*base.add(1), k0); // word 1: k0
            assert_eq!(*base.add(2), k1); // word 2: k1
            assert_eq!(*base.add(3), make_header(4, MAP_SUBTAG)); // word 1+n: head
            assert!(is_flatmap_header(*base.add(3)));
            assert_eq!((*base.add(4)).raw(), 2); // word 2+n: RAW size == n
            assert_eq!(*base.add(5), make_tuple(base)); // word 3+n: keys tuple
            assert_eq!(*base.add(6), v0); // word 4+n: v0
            assert_eq!(*base.add(7), v1); // word 7: v1

            // The keys tuple term (word `3+n`) points back at the block base, and the
            // flatmap root is the head at `hp + 1 + n`.
            let keys_tup = *base.add(5); // word 3+n
            assert_eq!(boxed_val(keys_tup), base);
            let head = boxed_val(map);
            assert_eq!(head, base.add(3)); // hp + 1 + n
            assert_eq!(*head.add(2), keys_tup); // head+2 == keys tuple
            assert_eq!(*head.add(3), v0); // head+3 == first value
        }

        // SAFETY: `map` is the live flatmap built above; `heap` outlives the slices.
        let (keys, values) = unsafe { flatmap_parts(map) };
        assert_eq!(keys, &[k0, k1]);
        assert_eq!(values, &[v0, v1]);
    }

    #[test]
    fn external_pid_block_layout_and_parts() {
        assert_eq!(external_pid_size_words(), 4);
        let mut heap = new_heap();
        let node = make_small(7);
        // SAFETY: `alloc_words` hands out exactly the 4-word block.
        let hp = unsafe { heap.alloc_words(external_pid_size_words()).unwrap() };
        // SAFETY: `hp` points at those 4 writable words.
        let pid = unsafe { write_external_pid(hp, node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678) };
        assert_eq!(heap.used(), 4);

        // SAFETY: `pid` is the 4-word block this test just wrote; word `3` is the
        // `data.pid = { Uint32 num; Uint32 ser; }` union.
        unsafe {
            let p = boxed_val(pid);
            assert_eq!(p, hp);
            assert_eq!(*p, make_header(3, EXTERNAL_PID_SUBTAG));
            assert!(is_external_pid_header(*p));
            assert_eq!(*p.add(1), node);
            assert_eq!(
                (*p.add(2)).raw() as u32,
                0x1234_5678,
                "creation lives in the off-heap-link slot"
            );
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 0xAAAA_BBBB, "number is the first Uint32 of data.pid");
            assert_eq!(*data.add(1), 0xCCCC_DDDD, "serial is the second");
        }

        // SAFETY: `pid` is the live external pid built above.
        let parts = unsafe { external_pid_parts(pid) };
        assert_eq!(parts, (node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678));
    }

    #[test]
    fn external_port_block_layout_and_parts() {
        assert_eq!(external_port_size_words(), 4);
        let mut heap = new_heap();
        let node = make_small(9);
        // SAFETY: `alloc_words` hands out exactly the 4-word block.
        let hp = unsafe { heap.alloc_words(external_port_size_words()).unwrap() };
        // SAFETY: `hp` points at those 4 writable words.
        let port = unsafe { write_external_port(hp, node, 0x1122_3344_5566_7788, 0x99AA_BBCC) };
        assert_eq!(heap.used(), 4);

        // SAFETY: `port` is the 4-word block this test just wrote; word `3` is the
        // 64-bit `data.port.id` (`ARCH_64`).
        unsafe {
            let p = boxed_val(port);
            assert_eq!(p, hp);
            assert_eq!(*p, make_header(3, EXTERNAL_PORT_SUBTAG));
            assert!(is_external_port_header(*p));
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0x99AA_BBCC);
            assert_eq!((*p.add(3)).raw() as u64, 0x1122_3344_5566_7788);
        }

        // SAFETY: `port` is the live external port built above.
        let (n, num, cre) = unsafe { external_port_parts(port) };
        assert_eq!((n, num, cre), (node, 0x1122_3344_5566_7788, 0x99AA_BBCC));
    }

    #[test]
    fn external_ref_block_layout_and_parts() {
        assert_eq!(external_ref_size_words(1), 4);
        assert_eq!(external_ref_size_words(3), 5);
        assert_eq!(external_ref_size_words(MAX_REF_IDS), 6);

        let mut heap = new_heap();
        let node = make_small(11);
        let ids = [0x1111_1111u32, 0x2222_2222, 0x3333_3333];
        // SAFETY: `alloc_words` hands out exactly the 5-word block for three ids.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(ids.len())).unwrap() };
        // SAFETY: `hp` points at those 5 writable words; three ids is within bounds.
        let r = unsafe { write_external_ref(hp, node, &ids, 0xABCD_1234) };
        // `EXTERNAL_THING_HEAD_SIZE` + `DW` = 3 + (3 / 2 + 1) = 5 words.
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 3 / U32_PER_WORD + 1);

        // SAFETY: `r` is the 5-word block this test just wrote. The data section is the
        // `data.ui32` array at word 3: the id count first (`bif.c:4840`), then the ids.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(p, hp);
            assert_eq!(*p, make_header(5 - 1, EXTERNAL_REF_SUBTAG));
            assert!(is_external_ref_header(*p));
            assert_eq!(arityval(*p) + 1, heap.used());
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0xABCD_1234);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, ids.len() as u32, "the id count heads the data section");
            assert_eq!(*data.add(1), ids[0]);
            assert_eq!(*data.add(2), ids[1]);
            assert_eq!(*data.add(3), ids[2]);
        }

        // SAFETY: `r` is the live external reference built above; `heap` outlives the
        // returned slice.
        let (n, got, cre) = unsafe { external_ref_parts(r) };
        assert_eq!(n, node);
        assert_eq!(got, &ids[..]);
        assert_eq!(cre, 0xABCD_1234);
    }

    #[test]
    fn external_ref_single_id_is_four_words() {
        let mut heap = new_heap();
        // SAFETY: `alloc_words` hands out `external_ref_size_words(1) == 4` words.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(1)).unwrap() };
        // SAFETY: `hp` points at those 4 writable words; one id is within bounds.
        let r = unsafe { write_external_ref(hp, NIL, &[0xDEAD_BEEF], 0x01) };
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 1);

        // SAFETY: `r` is the 4-word block this test just wrote.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(*p, make_header(3, EXTERNAL_REF_SUBTAG));
            assert_eq!(arityval(*p) + 1, 4);
            assert_eq!(*p.add(1), NIL);
            assert_eq!((*p.add(2)).raw() as u32, 0x01);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 1);
            assert_eq!(*data.add(1), 0xDEAD_BEEF);
        }

        // SAFETY: `r` is the live external reference built above.
        let (n, ids, cre) = unsafe { external_ref_parts(r) };
        assert_eq!(n, NIL);
        assert_eq!(ids, &[0xDEAD_BEEF]);
        assert_eq!(cre, 0x01);
    }

    #[test]
    fn external_ref_even_id_count_zeroes_the_padding_slot() {
        let mut heap = new_heap();
        // Two ids: `DW = 2 / 2 + 1 = 2` words = 4 `u32` slots, so slot 3 is padding.
        // SAFETY: `alloc_words` hands out `external_ref_size_words(2) == 5` words.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(2)).unwrap() };
        // SAFETY: `hp` points at those 5 writable words; two ids are within bounds.
        let r = unsafe { write_external_ref(hp, NIL, &[0xAA, 0xBB], 0x02) };
        assert_eq!(heap.used(), 5);

        // SAFETY: `r` is the 5-word block this test just wrote; the whole 4-slot data
        // section is initialised (the writer zeroes it before filling it).
        unsafe {
            let p = boxed_val(r);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 2);
            assert_eq!(*data.add(1), 0xAA);
            assert_eq!(*data.add(2), 0xBB);
            assert_eq!(*data.add(3), 0, "the trailing padding slot is zeroed");
        }

        // SAFETY: `r` is the live external reference built above.
        let (_, ids, _) = unsafe { external_ref_parts(r) };
        assert_eq!(ids, &[0xAA, 0xBB]);
    }

    #[test]
    fn external_ref_zero_ids_is_four_words_with_a_zero_count() {
        // The oracle accepts a zero-id external reference
        // (`binary_to_term(<<131,90,0:16,Node,0:32>>)`), whose block is exactly
        // `[header, node, creation, count = 0]`.
        assert_eq!(external_ref_size_words(0), 4);
        let mut heap = new_heap();
        let node = make_small(7);
        // SAFETY: `alloc_words` hands out exactly the 4-word block for zero ids.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(0)).unwrap() };
        // SAFETY: `hp` points at those 4 writable words; an empty `ids` is in bounds.
        let r = unsafe { write_external_ref(hp, node, &[], 0x0BAD_F00D) };
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 0 / U32_PER_WORD + 1);
        assert_eq!(heap.used(), 4);

        // SAFETY: `r` is the 4-word block this test just wrote.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(p, hp);
            assert_eq!(*p, make_header(3, EXTERNAL_REF_SUBTAG));
            assert!(is_external_ref_header(*p));
            assert_eq!(arityval(*p) + 1, 4);
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0x0BAD_F00D);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 0, "the id count is 0");
            assert_eq!(*data.add(1), 0, "the slot hashing reads is zeroed");
        }

        // SAFETY: `r` is the live external reference built above; `heap` outlives the
        // returned slice.
        let (n, ids, cre) = unsafe { external_ref_parts(r) };
        assert_eq!(n, node);
        assert!(ids.is_empty(), "zero ids reads back as an empty slice");
        assert_eq!(cre, 0x0BAD_F00D);
    }

    /// The hashing arms must not error on a zero-id external reference: the C reads
    /// `external_ref_numbers(x)[0]` — the word *after* the count, zeroed here — so
    /// the mixed value is 0, which is what a single `0` id mixes too.
    #[test]
    fn zero_id_external_ref_hashes_as_a_zero_first_id() {
        use crate::hashing::{make_hash, make_hash2};

        let mut atoms = AtomTable::new();
        atoms.seed(["node"]);
        let mut heap = new_heap();
        let node = make_small(3);
        let ids = [0u32];
        // SAFETY: both blocks are allocated from `heap`, which outlives the terms.
        let (empty, one_zero) = unsafe {
            let p = heap.alloc_words(external_ref_size_words(0)).unwrap();
            let empty = write_external_ref(p, node, &[], 0x11);
            let p = heap.alloc_words(external_ref_size_words(ids.len())).unwrap();
            let one_zero = write_external_ref(p, node, &ids, 0x11);
            (empty, one_zero)
        };

        assert_eq!(make_hash2(empty, &atoms).unwrap(), make_hash2(one_zero, &atoms).unwrap());
        assert_eq!(make_hash(empty, &atoms).unwrap(), make_hash(one_zero, &atoms).unwrap());
    }

    #[test]
    fn external_ref_max_ids_round_trip() {
        let mut heap = new_heap();
        let ids = [1u32, 2, 3, 4, 5];
        assert_eq!(ids.len(), MAX_REF_IDS);
        // SAFETY: `alloc_words` hands out `external_ref_size_words(5) == 6` words.
        let hp = unsafe { heap.alloc_words(external_ref_size_words(MAX_REF_IDS)).unwrap() };
        // SAFETY: `hp` points at those 6 writable words; the count is at the bound.
        let r = unsafe { write_external_ref(hp, NIL, &ids, 0x0A) };
        assert_eq!(heap.used(), 6);

        // SAFETY: `r` is the 6-word block this test just wrote.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(arityval(*p) + 1, 6);
            assert!(is_external_ref_header(*p));
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, MAX_REF_IDS as u32);
            for (i, id) in ids.iter().enumerate() {
                assert_eq!(*data.add(1 + i), *id);
            }
        }

        // SAFETY: `r` is the live external reference built above; `heap` outlives the
        // returned slice.
        let (n, got, cre) = unsafe { external_ref_parts(r) };
        assert_eq!(n, NIL);
        assert_eq!(got, &ids[..]);
        assert_eq!(cre, 0x0A);
    }

    #[test]
    fn blocks_of_the_wrong_kind_are_header_distinguishable() {
        // The predicates a reader keys off are all header-only, so a caller that mixes
        // up the kinds is caught by these header tests.
        let pid = make_header(3, EXTERNAL_PID_SUBTAG);
        let port = make_header(3, EXTERNAL_PORT_SUBTAG);
        let reference = make_header(4, EXTERNAL_REF_SUBTAG);
        assert!(is_external_pid_header(pid) && !is_external_port_header(pid));
        assert!(is_external_port_header(port) && !is_external_pid_header(port));
        assert!(is_external_ref_header(reference) && !is_external_pid_header(reference));
        assert!(!is_flatmap_header(pid));
    }
}
