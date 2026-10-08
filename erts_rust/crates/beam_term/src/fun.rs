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

//! The on-heap **fun / export** term layout — the boxed closure term.
//!
//! A fun is the boxed thing a `make_fun3` (local closure) or an `EXPORT_EXT` /
//! `NEW_FUN_EXT` literal decodes to. The frozen layout of its header and
//! descriptor words is owned by this crate (the lowest common ancestor for term
//! layouts — workspace `CONTEXT.md` §2), so the writers and readers live here,
//! next to the predicates in [`crate::boxed`] and mirroring [`crate::thing`].
//!
//! C ground truth (read-only reference; never edited):
//!
//! * `erts/emulator/beam/erl_term.h:403-425` — the fun header bit layout
//!   (`FUN_HEADER_ARITY_OFFS` / `FUN_HEADER_ENV_SIZE_OFFS` / `FUN_HEADER_KIND_OFFS`
//!   and `MAKE_FUN_HEADER`, plus `is_fun_header`/`is_any_fun`).
//! * `erts/emulator/beam/erl_fun.h:61-99` — `ErlFunThing` (`[thing_word, entry,
//!   env[]]`), `ErlFunEntry` (`module`, `uniq[16]`, `arity`, `index`, `old_uniq`,
//!   `old_index`), `is_external_fun`/`is_local_fun`/`fun_arity`/`fun_num_free`.
//! * `erts/emulator/beam/emu/instrs.tab:545-584` — `i_make_fun3`, the local-fun
//!   writer (`funp->env[i] = <i-th Env operand>`).
//! * `erts/emulator/beam/external.c:3966-4003` — the fun encoder,
//!   `:5006-5040` — the `EXPORT_EXT` decoder, `:5102-5176` — the
//!   `NEW_FUN_EXT` decoder.
//! * `erts/emulator/beam/utils.c:2149-2210` — fun ordering inside `erts_cmp`.
//! * `erts/emulator/beam/erl_vm.h:48` — `MAX_ARG` (255), the fun arity limit.
//!
//! # Three kinds, discriminated by the header's kind bits
//!
//! ```text
//!      0000000eekkkkkkk eeeeeeeeaaaaaaaa 00010100
//!             ││ └─ num_free (8 bits)       └─ FUN_SUBTAG (0x14)
//!             │└──── arity     (8 bits)
//!             │
//!             └── imported (bit 25, our extension)
//!              └─ external (bit 24, C's FUN_HEADER_KIND_OFFS)
//! ```
//!
//! * [`FunKind::Local`] (bits 24/25 clear) — the `make_fun3` closure.
//! * [`FunKind::External`] (bit 24 set) — an `EXPORT_EXT` fun.
//! * [`FunKind::Imported`] (bit 25 set) — a `NEW_FUN_EXT` fun.
//!
//! # The inlined-descriptor substitution
//!
//! In C the fun block is `[thing_word, entry, env[0..num_free]]`
//! (`erl_fun.h:65-85`, `ERL_FUN_SIZE = 2`), and `entry` is a **pointer** to an
//! out-of-line `ErlFunEntry` (local fun, `erl_fun.h:31-59`, carrying `module`,
//! `uniq[16]`, `index`, `old_uniq`, `old_index`) or to the canonical `Export`
//! (external fun, `erl_fun.h:79-80`). This tree has no fun/export table, so —
//! exactly like the "M1 substitution" already used for the external-pid `node`
//! slot ([`crate::thing::write_external_pid`]) and by
//! `beam_engine::terms::make_fun` — the **entry fields are inlined into the term**
//! as descriptor words instead of an entry pointer. The header word keeps C's
//! exact encoding; only the single `entry` word is expanded:
//!
//! | kind | descriptor words after the header | words |
//! |---|---|---|
//! | `Local` | `uniq` (u64) · `index` (u32) | 2 |
//! | `External` | `module` (atom) · `function` (atom) | 2 |
//! | `Imported` | `module` · `uniq[0..8]` · `uniq[8..16]` · `index` · `old_index` · `old_uniq` · `pid_node` · `pid_number` · `pid_serial` · `pid_creation` | 10 |
//!
//! `env[i]` lives at `hp[1 + desc_words(kind) + i]`, in the same order
//! `i_make_fun3` stores it, and the block is `1 + desc_words(kind) + num_free`
//! words **including the header**.
//!
//! Carrying every wire field is deliberate: `EXPORT_EXT` and `NEW_FUN_EXT`
//! decode→encode stays byte-exact, a later `beam_engine`/`beam_rt` can call the
//! fun, and [`crate::compare`]/[`crate::hashing`] can be structural.
//!
//! The `Local` layout is **byte-identical** to what
//! `beam_engine::terms::make_fun` writes, so the engine needs no change.
//!
//! # The heap-agnostic contract
//!
//! `beam_term` is a dependency *sink*: it cannot allocate, because the production
//! allocators (`beam_alloc`) and the copying collector (`beam_gc`) depend
//! downward on it. Every writer here therefore does **not** allocate: the caller
//! reserves [`fun_words`]`(kind, num_free)` words (from `beam_alloc` in
//! production, [`crate::heap::Heap`] in tests), passes the `hp: *mut Eterm` base
//! of that block, and receives back the tagged root word.
//!
//! # `unsafe` contract
//!
//! Every writer and reader is `unsafe`: an `Eterm` being a live, well-formed fun
//! of the expected kind is a caller invariant, and there is no way to read or
//! write a raw heap word safely. Each item documents, in its `# Safety` section,
//! exactly how many words the caller must have reserved, which of them are
//! written, and how long a returned borrowed slice stays valid. The **pure**
//! header helpers ([`make_fun_header`], [`fun_kind`], [`fun_arity`], …) take a
//! *header word* — never a term — and so need no `unsafe` and no dereference.

use crate::atom::Atom;
use crate::boxed::{boxed_val, is_boxed_term, is_fun_header, make_boxed};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::immediate::{atom_val, make_atom};
use crate::tag::FUN_SUBTAG;

/// `FUN_HEADER_ARITY_OFFS` (`erl_term.h:413`): `_HEADER_ARITY_OFFS + 2`. The bit
/// offset of the fun header's `arity` field.
pub const FUN_HEADER_ARITY_OFFS: u32 = 8;
/// `FUN_HEADER_ENV_SIZE_OFFS` (`erl_term.h:414`): `FUN_HEADER_ARITY_OFFS + 8`. The
/// bit offset of the fun header's `num_free` (environment size) field.
pub const FUN_HEADER_ENV_SIZE_OFFS: u32 = 16;
/// `FUN_HEADER_KIND_OFFS` (`erl_term.h:415`): `FUN_HEADER_ENV_SIZE_OFFS + 8`. The
/// bit that marks a **C external** fun (`erl_fun.h:87` `is_external_fun`).
pub const FUN_HEADER_KIND_OFFS: u32 = 24;
/// The bit that marks a **wire-imported** (`NEW_FUN_EXT`) fun. This is this
/// crate's extension: it is valid only while [`FUN_HEADER_KIND_OFFS`] is clear,
/// so a C reader's `is_external_fun` still sees such a fun as a local one.
pub const FUN_HEADER_IMPORTED_OFFS: u32 = 25;
/// The largest value the 8-bit `arity` / `num_free` header fields hold
/// (`erl_term.h:403-411`: "the arity and number of free variables are both
/// limited to 255").
pub const FUN_MAX_FIELD: usize = 0xFF;
/// `MAX_ARG` (`erts/emulator/beam/erl_vm.h:48`): the largest arity a fun may
/// have. It is the same 255 the header's 8-bit `arity` field can hold.
pub const MAX_ARG: usize = 255;
/// The descriptor words a [`FunKind::Local`] fun carries — `uniq` (u64) and
/// `index` (u32) — in place of the C `entry` pointer (`erl_fun.h:65-85`).
pub const FUN_DESC_WORDS_LOCAL: usize = 2;
/// The descriptor words a [`FunKind::External`] fun carries — the `Export`'s
/// `module` and `function` atoms (`optimize.h`/`erl_fun.h:79-80`).
pub const FUN_DESC_WORDS_EXTERNAL: usize = 2;
/// The descriptor words a [`FunKind::Imported`] fun carries — the whole
/// `NEW_FUN_EXT` signature (`external.c:5102-5176`): `module`, the two packed
/// `uniq` words, `index`, `old_index`, `old_uniq` and the creator pid.
pub const FUN_DESC_WORDS_IMPORTED: usize = 10;

/// Which of the three fun representations a header word describes.
///
/// Discriminated by the header's kind bits ([`FUN_HEADER_KIND_OFFS`],
/// [`FUN_HEADER_IMPORTED_OFFS`]); see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FunKind {
    /// A local closure as built by `make_fun3` ([`write_fun_local`]).
    Local,
    /// An exported function literal decoded from `EXPORT_EXT`
    /// ([`write_fun_external`]).
    External,
    /// A wire fun decoded from `NEW_FUN_EXT` ([`write_fun_imported`]).
    Imported,
}

/// The creator pid of a [`FunKind::Imported`] fun — the `NEW_FUN_EXT` trailer
/// (`external.c:5153-5160` decodes `erts_init_process_id`; the C discards it, we
/// keep it so the layout round-trips).
///
/// Mirrors the [`crate::thing`] "`node` slot holds the sysname atom" substitution:
/// `node` is the atom, the remaining fields are the process id's scalars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunPid {
    /// The node's sysname atom.
    pub node: Atom,
    /// The process number.
    pub number: u32,
    /// The process serial.
    pub serial: u32,
    /// The node creation counter.
    pub creation: u32,
}

// ---------------------------------------------------------------------------
// Pure header-word helpers (no term, no dereference)
// ---------------------------------------------------------------------------

/// `MAKE_FUN_HEADER(Arity, NumFree, External)` (`erl_term.h:417-422`) extended
/// with the [`FunKind::Imported`] bit.
///
/// `arity` and `num_free` must each fit the 8-bit header fields
/// ([`FUN_MAX_FIELD`] / [`MAX_ARG`]); a larger value yields
/// [`TermError::ArityOutOfRange`] instead of silently truncating.
///
/// C's macro `ASSERT`s `NumFree == 0` for an external fun. That is an invariant
/// of the *writers* here ( [`write_fun_external`] has no environment), not of
/// the layout, so it is not enforced: [`fun_block_words`] sizes any combination
/// consistently.
pub fn make_fun_header(arity: usize, num_free: usize, kind: FunKind) -> TermResult<Eterm> {
    if arity > MAX_ARG || num_free > FUN_MAX_FIELD {
        return Err(TermError::ArityOutOfRange);
    }
    let mut raw = FUN_SUBTAG
        | (arity << FUN_HEADER_ARITY_OFFS)
        | (num_free << FUN_HEADER_ENV_SIZE_OFFS);
    match kind {
        FunKind::Local => {}
        FunKind::External => raw |= 1 << FUN_HEADER_KIND_OFFS,
        FunKind::Imported => raw |= 1 << FUN_HEADER_IMPORTED_OFFS,
    }
    Ok(Eterm::from_raw(raw))
}

/// The [`FunKind`] a **fun header word** encodes.
///
/// This is the unchecked bit extraction; the caller must already know `hdr` is a
/// fun header (`is_fun_header(hdr)`). [`fun_kind`] is the checking entry point.
pub const fn fun_kind_of_header(hdr: Eterm) -> FunKind {
    if (hdr.raw() >> FUN_HEADER_IMPORTED_OFFS) & 1 != 0 {
        FunKind::Imported
    } else if (hdr.raw() >> FUN_HEADER_KIND_OFFS) & 1 != 0 {
        FunKind::External
    } else {
        FunKind::Local
    }
}

/// The [`FunKind`] of a fun header word, or [`TermError::BadFun`] if `hdr` is not
/// a fun header (`is_fun_header`, `erl_term.h:424`).
pub fn fun_kind(hdr: Eterm) -> TermResult<FunKind> {
    if !is_fun_header(hdr) {
        return Err(TermError::BadFun);
    }
    Ok(fun_kind_of_header(hdr))
}

/// `fun_arity(FunThing)` (`erl_fun.h:92-93`) — the fun's arity from its header
/// word. `hdr` must be a fun header word.
pub const fn fun_arity(hdr: Eterm) -> usize {
    ((hdr.raw() >> FUN_HEADER_ARITY_OFFS) & FUN_MAX_FIELD) as usize
}

/// `fun_num_free(FunThing)` (`erl_fun.h:94-95`) — the size of the fun's
/// environment from its header word. `hdr` must be a fun header word.
pub const fn fun_num_free(hdr: Eterm) -> usize {
    ((hdr.raw() >> FUN_HEADER_ENV_SIZE_OFFS) & FUN_MAX_FIELD) as usize
}

/// The descriptor words a fun of `kind` carries between the header and the
/// environment ([`FUN_DESC_WORDS_LOCAL`] / [`FUN_DESC_WORDS_EXTERNAL`] /
/// [`FUN_DESC_WORDS_IMPORTED`]) — the M1 inlining of the C `entry` pointer.
pub const fn fun_desc_words(kind: FunKind) -> usize {
    match kind {
        FunKind::Local => FUN_DESC_WORDS_LOCAL,
        FunKind::External => FUN_DESC_WORDS_EXTERNAL,
        FunKind::Imported => FUN_DESC_WORDS_IMPORTED,
    }
}

/// The words a fun block occupies **including the header word** —
/// `1 + desc_words(kind) + num_free`. This is the exact size a caller must
/// reserve before calling a `write_fun_*`.
pub const fn fun_words(kind: FunKind, num_free: usize) -> usize {
    1 + fun_desc_words(kind) + num_free
}

/// [`fun_desc_words`] for a fun header word, or [`TermError::BadFun`] if `hdr` is
/// not a fun header.
pub fn fun_desc_words_header(hdr: Eterm) -> TermResult<usize> {
    Ok(fun_desc_words(fun_kind(hdr)?))
}

/// The word offset of `env[0]` for a fun header word: `1 + desc_words(kind)`, or
/// [`TermError::BadFun`] if `hdr` is not a fun header.
pub fn fun_env_offset_header(hdr: Eterm) -> TermResult<usize> {
    Ok(1 + fun_desc_words_header(hdr)?)
}

/// The block size **including the header word** for a fun header word:
/// `1 + desc_words(kind) + num_free`, or [`TermError::BadFun`] if `hdr` is not a
/// fun header.
pub fn fun_block_words_header(hdr: Eterm) -> TermResult<usize> {
    Ok(fun_env_offset_header(hdr)? + fun_num_free(hdr))
}

/// The block size **including the header word** of a live fun **term** —
/// [`fun_block_words_header`] applied to the term's header word.
///
/// A non-fun (`NIL`, an immediate, a tuple, …) yields [`TermError::BadFun`].
pub fn fun_block_words(t: Eterm) -> TermResult<usize> {
    fun_block_words_header(fun_term_header(t)?)
}

/// The header word of a live fun term, or [`TermError::BadFun`].
///
/// Safe to call: the boxed flag and the fun subtag are both tested before the
/// header word is read, so a non-fun term is reported as an error. The read
/// itself assumes the usual caller invariant that a boxed `Eterm` points at a
/// live, initialised header word — exactly as [`crate::size::size_object`] and
/// [`crate::compare::cmp`] do.
fn fun_term_header(t: Eterm) -> TermResult<Eterm> {
    if !is_boxed_term(t) {
        return Err(TermError::BadFun);
    }
    // SAFETY: `t` is a live boxed term, so `boxed_val(t)` points at its live,
    // initialised header word.
    let header = unsafe { *boxed_val(t) };
    if !is_fun_header(header) {
        return Err(TermError::BadFun);
    }
    Ok(header)
}

/// The kind of a live fun term, or [`TermError::BadFun`].
///
/// The term-level counterpart of [`fun_kind`]; like [`fun_term_header`] it tests
/// the boxed flag and the fun subtag itself, so it carries the same "live boxed
/// term" caller invariant as [`crate::size::size_object`].
fn term_fun_kind(t: Eterm) -> TermResult<FunKind> {
    Ok(fun_kind_of_header(fun_term_header(t)?))
}

// ---------------------------------------------------------------------------
// Writers (heap-agnostic: the caller reserves the words)
// ---------------------------------------------------------------------------

/// Writes a [`FunKind::Local`] fun block at `hp` and returns the tagged root.
///
/// Layout (module docs; pinned to `i_make_fun3`, `emu/instrs.tab:545-584`, and
/// byte-identical to `beam_engine::terms::make_fun`):
///
/// ```text
/// word 0            MAKE_FUN_HEADER(arity, num_free, external = 0)
/// word 1            uniq   (u64)  — replaces the C `entry` pointer
/// word 2            index  (u32)  — the lambda-table index
/// words 3 .. 3+n    env[0..num_free]
/// ```
///
/// # Safety
///
/// `hp` must point at `fun_words(FunKind::Local, num_free)` exclusively-owned,
/// writable words (where `num_free == env.len()`); all of them are written by
/// this call and must stay live (and unmoved) for as long as the returned term
/// is used. An `env` term that is boxed must itself be a live term.
pub unsafe fn write_fun_local(
    hp: *mut Eterm,
    arity: usize,
    uniq: u64,
    index: u32,
    env: &[Eterm],
) -> TermResult<Eterm> {
    let num_free = env.len();
    let header = make_fun_header(arity, num_free, FunKind::Local)?;
    // SAFETY: the caller reserved `fun_words(Local, num_free) == 1 + 2 + num_free`
    // words. Word 0 receives the header, word 1 the `uniq`, word 2 the `index` and
    // words `3..3+num_free` the free variables — every write is in bounds.
    unsafe {
        *hp = header;
        *hp.add(1) = Eterm::from_raw(uniq as usize);
        *hp.add(2) = Eterm::from_raw(index as usize);
        for (i, var) in env.iter().enumerate() {
            *hp.add(FUN_DESC_WORDS_LOCAL + 1 + i) = *var;
        }
    }
    Ok(make_boxed(hp))
}

/// Writes a [`FunKind::External`] (`EXPORT_EXT`) fun block at `hp` and returns
/// the tagged root.
///
/// Layout (module docs; the `Export`'s `info.mfa`, `erl_fun.h:79-80`,
/// `external.c:3994-4002`):
///
/// ```text
/// word 0   MAKE_FUN_HEADER(arity, 0, external = 1)
/// word 1   module    (atom term)
/// word 2   function  (atom term)
/// ```
///
/// `num_free` is zero for an external fun, so there is no environment.
///
/// # Safety
///
/// `hp` must point at `fun_words(FunKind::External, 0)` (3) exclusively-owned,
/// writable words; all of them are written by this call and must stay live (and
/// unmoved) for as long as the returned term is used.
pub unsafe fn write_fun_external(
    hp: *mut Eterm,
    arity: usize,
    module: Atom,
    function: Atom,
) -> TermResult<Eterm> {
    let header = make_fun_header(arity, 0, FunKind::External)?;
    // SAFETY: the caller reserved `fun_words(External, 0) == 1 + 2` words. Word 0
    // receives the header, words 1 and 2 the module and function atoms.
    unsafe {
        *hp = header;
        *hp.add(1) = make_atom(module);
        *hp.add(2) = make_atom(function);
    }
    Ok(make_boxed(hp))
}

/// Writes a [`FunKind::Imported`] (`NEW_FUN_EXT`) fun block at `hp` and returns
/// the tagged root.
///
/// Layout (module docs; the whole wire signature, `external.c:5102-5176`):
///
/// ```text
/// word 0            MAKE_FUN_HEADER(arity, num_free, imported = 1)
/// word 1            module        (atom term)
/// word 2            uniq[0..8]    (first 8 MD5 bytes, little-endian in one u64 word)
/// word 3            uniq[8..16]   (last 8 MD5 bytes, little-endian in one u64 word)
/// word 4            index         (u32, new style)
/// word 5            old_index     (u32)
/// word 6            old_uniq      (u32)
/// word 7            pid_node      (atom term)
/// word 8            pid_number    (u32)
/// word 9            pid_serial    (u32)
/// word 10           pid_creation  (u32)
/// words 11 .. 11+n  env[0..num_free]
/// ```
///
/// # Safety
///
/// `hp` must point at `fun_words(FunKind::Imported, num_free)` exclusively-owned,
/// writable words (where `num_free == env.len()`); all of them are written by
/// this call and must stay live (and unmoved) for as long as the returned term
/// is used. An `env` term that is boxed must itself be a live term.
#[allow(clippy::too_many_arguments)]
pub unsafe fn write_fun_imported(
    hp: *mut Eterm,
    arity: usize,
    module: Atom,
    uniq: &[u8; 16],
    index: u32,
    old_index: u32,
    old_uniq: u32,
    creator: FunPid,
    env: &[Eterm],
) -> TermResult<Eterm> {
    let num_free = env.len();
    let header = make_fun_header(arity, num_free, FunKind::Imported)?;
    let uniq_lo = pack_le_u64(uniq, 0);
    let uniq_hi = pack_le_u64(uniq, 8);
    // SAFETY: the caller reserved `fun_words(Imported, num_free) == 1 + 10 +
    // num_free` words. Words 0..=10 receive the header, the module atom, the two
    // packed `uniq` words, `index`, `old_index`, `old_uniq` and the four creator-pid
    // scalars; words `11..11+num_free` the free variables — every write is in bounds.
    unsafe {
        *hp = header;
        *hp.add(1) = make_atom(module);
        *hp.add(2) = Eterm::from_raw(uniq_lo as usize);
        *hp.add(3) = Eterm::from_raw(uniq_hi as usize);
        *hp.add(4) = Eterm::from_raw(index as usize);
        *hp.add(5) = Eterm::from_raw(old_index as usize);
        *hp.add(6) = Eterm::from_raw(old_uniq as usize);
        *hp.add(7) = make_atom(creator.node);
        *hp.add(8) = Eterm::from_raw(creator.number as usize);
        *hp.add(9) = Eterm::from_raw(creator.serial as usize);
        *hp.add(10) = Eterm::from_raw(creator.creation as usize);
        for (i, var) in env.iter().enumerate() {
            *hp.add(FUN_DESC_WORDS_IMPORTED + 1 + i) = *var;
        }
    }
    Ok(make_boxed(hp))
}

/// Packs `bytes[offset..offset + 8]` into a `u64`, **little-endian** (the first
/// byte is the least significant), matching how the `NEW_FUN_EXT` decoder treats
/// the wire's `uniq[16]` as two machine words.
///
/// `offset` must leave eight readable bytes (`offset + 8 <= bytes.len()`).
fn pack_le_u64(bytes: &[u8; 16], offset: usize) -> u64 {
    let mut word = 0u64;
    let mut i = 0;
    while i < 8 {
        word |= (bytes[offset + i] as u64) << (8 * i);
        i += 1;
    }
    word
}

// ---------------------------------------------------------------------------
// Accessors (each reports a wrong kind/type as a typed error, never a panic)
// ---------------------------------------------------------------------------

/// The `uniq` (fun identity) of a [`FunKind::Local`] fun term — descriptor word
/// 1. A non-fun, or a fun of another kind, yields [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_uniq(t: Eterm) -> TermResult<u64> {
    if term_fun_kind(t)? != FunKind::Local {
        return Err(TermError::BadFun);
    }
    // SAFETY: `t` is a live local fun, so descriptor word 1 (the `uniq`) is live and
    // initialised.
    Ok(unsafe { (*boxed_val(t).add(1)).raw() } as u64)
}

/// The 16-byte `uniq` (MD5) of a [`FunKind::Imported`] fun term — descriptor
/// words 2 and 3, unpacked little-endian in the same order
/// [`write_fun_imported`] packed them. A non-fun, or a fun of another kind,
/// yields [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_uniq_bytes(t: Eterm) -> TermResult<[u8; 16]> {
    if term_fun_kind(t)? != FunKind::Imported {
        return Err(TermError::BadFun);
    }
    // SAFETY: `t` is a live imported fun, so descriptor words 2 and 3 (the packed
    // `uniq[0..8]` / `uniq[8..16]`) are live and initialised.
    let (lo, hi) = unsafe {
        (
            (*boxed_val(t).add(2)).raw() as u64,
            (*boxed_val(t).add(3)).raw() as u64,
        )
    };
    let mut out = [0u8; 16];
    for i in 0..8 {
        out[i] = (lo >> (8 * i)) as u8;
        out[8 + i] = (hi >> (8 * i)) as u8;
    }
    Ok(out)
}

/// The lambda-table `index` of a fun term — descriptor word 2 for a
/// [`FunKind::Local`] fun, word 4 for a [`FunKind::Imported`] one.
///
/// A [`FunKind::External`] fun has no index, so it yields
/// [`TermError::BadFun`], as does any non-fun.
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_index(t: Eterm) -> TermResult<u32> {
    let word = match term_fun_kind(t)? {
        FunKind::Local => 2,
        FunKind::Imported => 4,
        FunKind::External => return Err(TermError::BadFun),
    };
    // SAFETY: `t` is a live fun of a kind whose descriptor covers `word` (2 for
    // `Local`, 4 for `Imported`), so it is live and initialised.
    Ok(unsafe { (*boxed_val(t).add(word)).raw() } as u32)
}

/// The `old_index` of a [`FunKind::Imported`] fun term — descriptor word 5. A
/// non-fun, or a fun of another kind, yields [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_old_index(t: Eterm) -> TermResult<u32> {
    // SAFETY: `t` is a live boxed term (caller invariant); the helper rejects a
    // non-fun or a fun of another kind before reading.
    unsafe { imported_descriptor_word(t, 5) }
}

/// The `old_uniq` of a [`FunKind::Imported`] fun term — descriptor word 6. A
/// non-fun, or a fun of another kind, yields [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_old_uniq(t: Eterm) -> TermResult<u32> {
    // SAFETY: `t` is a live boxed term (caller invariant); the helper rejects a
    // non-fun or a fun of another kind before reading.
    unsafe { imported_descriptor_word(t, 6) }
}

/// Descriptor word `word` (1-based after the header) of an **imported** fun, or
/// [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
unsafe fn imported_descriptor_word(t: Eterm, word: usize) -> TermResult<u32> {
    if term_fun_kind(t)? != FunKind::Imported {
        return Err(TermError::BadFun);
    }
    debug_assert!((1..=FUN_DESC_WORDS_IMPORTED).contains(&word));
    // SAFETY: `t` is a live imported fun, whose block is `1 + 10 + num_free` words,
    // so descriptor words `1..=10` are live and initialised; `word` is one of them.
    Ok(unsafe { (*boxed_val(t).add(word)).raw() } as u32)
}

/// The `module` atom of a fun term — descriptor word 1 for a
/// [`FunKind::External`] or [`FunKind::Imported`] fun.
///
/// A [`FunKind::Local`] fun inlines only `uniq`/`index` (the C `ErlFunEntry`
/// `module` field is not carried), so it yields [`TermError::BadFun`], as does
/// any non-fun.
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_module(t: Eterm) -> TermResult<Atom> {
    match term_fun_kind(t)? {
        FunKind::External | FunKind::Imported => {
            // SAFETY: descriptor word 1 (the module atom) is live for both kinds.
            Ok(atom_val(unsafe { *boxed_val(t).add(1) }))
        }
        FunKind::Local => Err(TermError::BadFun),
    }
}

/// The `function` (name) atom of a [`FunKind::External`] fun term —
/// descriptor word 2. A non-fun, or a fun of another kind, yields
/// [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_function(t: Eterm) -> TermResult<Atom> {
    if term_fun_kind(t)? != FunKind::External {
        return Err(TermError::BadFun);
    }
    // SAFETY: `t` is a live external fun, so descriptor word 2 (the function atom)
    // is live and initialised.
    Ok(atom_val(unsafe { *boxed_val(t).add(2) }))
}

/// The creator pid of a [`FunKind::Imported`] fun term — descriptor words 7..=10.
/// A non-fun, or a fun of another kind, yields [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live.
pub unsafe fn fun_creator_pid(t: Eterm) -> TermResult<FunPid> {
    if term_fun_kind(t)? != FunKind::Imported {
        return Err(TermError::BadFun);
    }
    // SAFETY: `t` is a live imported fun, so descriptor words 7..=10 (the creator
    // pid's atom and three scalars) are live and initialised.
    unsafe {
        let p = boxed_val(t);
        Ok(FunPid {
            node: atom_val(*p.add(7)),
            number: (*p.add(8)).raw() as u32,
            serial: (*p.add(9)).raw() as u32,
            creation: (*p.add(10)).raw() as u32,
        })
    }
}

/// The environment (free variables) of a fun term, in `env` order — the
/// `num_free` words starting at `hp[1 + desc_words(kind)]`.
///
/// An external fun has no environment, so the slice is empty. A non-fun yields
/// [`TermError::BadFun`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a fun, its whole block must be live
/// **and stay unmoved** for `'a`. The returned slice borrows the term's storage.
pub unsafe fn fun_env<'a>(t: Eterm) -> TermResult<&'a [Eterm]> {
    let header = fun_term_header(t)?;
    let num_free = fun_num_free(header);
    if num_free == 0 {
        return Ok(&[]);
    }
    let offset = 1 + fun_desc_words(fun_kind_of_header(header));
    // SAFETY: `header` encodes `num_free` free variables that start at descriptor
    // offset `offset`; the block is `offset + num_free` words, so the slice is live
    // and in bounds. The caller keeps the owning heap alive and unmoved for `'a`.
    Ok(unsafe { core::slice::from_raw_parts(boxed_val(t).add(offset), num_free) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{
        is_any_fun, is_external_fun, is_imported_fun, is_local_fun, make_arityval, make_header,
    };
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::tag::FLOAT_SUBTAG;
    use crate::AtomTable;

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["erlang", "module", "function", "node"]);
        t
    }

    /// The `u64` a little-endian 8-byte window decodes to (test-side oracle for
    /// [`pack_le_u64`]).
    fn from_le(bytes: &[u8]) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        u64::from_le_bytes(buf)
    }

    /// Allocate `words` from `heap` and return the writable base pointer.
    fn hp(heap: &mut Heap, words: usize) -> *mut Eterm {
        // SAFETY: the test writes the whole block through a `write_fun_*` before it
        // is read; the heap outlives every term built from it.
        unsafe { heap.alloc_words(words).unwrap() }
    }

    // -- constants and header-word helpers ------------------------------------

    #[test]
    fn header_bit_layout_is_pinned_to_the_c() {
        // erl_term.h:413-415 / erl_vm.h:48.
        assert_eq!(FUN_HEADER_ARITY_OFFS, 8);
        assert_eq!(FUN_HEADER_ENV_SIZE_OFFS, 16);
        assert_eq!(FUN_HEADER_KIND_OFFS, 24);
        assert_eq!(FUN_HEADER_IMPORTED_OFFS, 25);
        assert_eq!(FUN_MAX_FIELD, 0xFF);
        assert_eq!(MAX_ARG, 255);
        assert_eq!(FUN_DESC_WORDS_LOCAL, 2);
        assert_eq!(FUN_DESC_WORDS_EXTERNAL, 2);
        assert_eq!(FUN_DESC_WORDS_IMPORTED, 10);
        assert_eq!(FUN_SUBTAG, 0x14);
    }

    #[test]
    fn header_words_for_each_kind_are_pinned() {
        // MAKE_FUN_HEADER(arity, num_free, external) | our imported bit.
        let local = make_fun_header(3, 2, FunKind::Local).unwrap();
        assert_eq!(local.raw(), FUN_SUBTAG | (3 << 8) | (2 << 16));
        assert!(is_fun_header(local));
        assert!(is_local_fun(local));
        assert!(!is_external_fun(local));
        assert!(!is_imported_fun(local));
        assert_eq!(fun_kind(local).unwrap(), FunKind::Local);

        let external = make_fun_header(1, 0, FunKind::External).unwrap();
        assert_eq!(external.raw(), FUN_SUBTAG | (1 << 8) | (1 << 24));
        assert!(is_external_fun(external));
        assert!(!is_local_fun(external));
        assert!(!is_imported_fun(external));
        assert_eq!(fun_kind(external).unwrap(), FunKind::External);

        let imported = make_fun_header(0, 5, FunKind::Imported).unwrap();
        assert_eq!(imported.raw(), FUN_SUBTAG | (5 << 16) | (1 << 25));
        assert!(is_imported_fun(imported));
        // A C reader's `is_external_fun` only looks at bit 24.
        assert!(!is_external_fun(imported));
        assert!(!is_local_fun(imported));
        assert_eq!(fun_kind(imported).unwrap(), FunKind::Imported);
    }

    #[test]
    fn header_fields_over_the_8_bit_limit_are_rejected() {
        assert!(make_fun_header(255, 255, FunKind::Local).is_ok());
        assert_eq!(
            make_fun_header(256, 0, FunKind::Local),
            Err(TermError::ArityOutOfRange)
        );
        assert_eq!(
            make_fun_header(0, 256, FunKind::Local),
            Err(TermError::ArityOutOfRange)
        );
        assert_eq!(
            make_fun_header(MAX_ARG + 1, 0, FunKind::External),
            Err(TermError::ArityOutOfRange)
        );
    }

    #[test]
    fn header_fields_round_trip_over_the_range() {
        for arity in [0usize, 1, 2, 127, 254, 255] {
            for num_free in [0usize, 1, 200, 255] {
                for kind in [FunKind::Local, FunKind::External, FunKind::Imported] {
                    let hdr = make_fun_header(arity, num_free, kind).unwrap();
                    assert_eq!(fun_arity(hdr), arity, "{kind:?} arity");
                    assert_eq!(fun_num_free(hdr), num_free, "{kind:?} num_free");
                    assert_eq!(fun_kind(hdr).unwrap(), kind);
                    assert_eq!(fun_kind_of_header(hdr), kind);
                    assert!(is_fun_header(hdr));
                }
            }
        }
    }

    #[test]
    fn kind_helpers_reject_a_non_fun_header() {
        assert_eq!(fun_kind(make_header(1, FLOAT_SUBTAG)), Err(TermError::BadFun));
        assert_eq!(fun_kind(make_arityval(1)), Err(TermError::BadFun));
        assert_eq!(fun_desc_words_header(NIL), Err(TermError::BadFun));
        assert_eq!(fun_env_offset_header(NIL), Err(TermError::BadFun));
        assert_eq!(fun_block_words_header(NIL), Err(TermError::BadFun));
        // The header-only predicates are pure bit tests: `is_fun_header` is the real
        // gate, while the kind tests only look at bits 24/25.
        assert!(!is_fun_header(make_arityval(1)));
        assert!(!is_external_fun(make_arityval(1)));
        assert!(!is_imported_fun(make_arityval(1)));
        let kind_bit_set = |bit: u32| Eterm::from_raw(make_arityval(1).raw() | (1 << bit));
        assert!(is_external_fun(kind_bit_set(FUN_HEADER_KIND_OFFS)));
        assert!(is_imported_fun(kind_bit_set(FUN_HEADER_IMPORTED_OFFS)));
    }

    #[test]
    fn sizing_helpers_agree_with_fun_words() {
        let local = make_fun_header(2, 3, FunKind::Local).unwrap();
        assert_eq!(fun_desc_words_header(local).unwrap(), 2);
        assert_eq!(fun_env_offset_header(local).unwrap(), 3);
        assert_eq!(fun_block_words_header(local).unwrap(), 6);
        assert_eq!(fun_words(FunKind::Local, 3), 6);
        assert_eq!(fun_desc_words(FunKind::Local), FUN_DESC_WORDS_LOCAL);

        let external = make_fun_header(2, 0, FunKind::External).unwrap();
        assert_eq!(fun_desc_words_header(external).unwrap(), 2);
        assert_eq!(fun_env_offset_header(external).unwrap(), 3);
        assert_eq!(fun_block_words_header(external).unwrap(), 3);
        assert_eq!(fun_words(FunKind::External, 0), 3);

        let imported = make_fun_header(0, 4, FunKind::Imported).unwrap();
        assert_eq!(fun_desc_words_header(imported).unwrap(), 10);
        assert_eq!(fun_env_offset_header(imported).unwrap(), 11);
        assert_eq!(fun_block_words_header(imported).unwrap(), 15);
        assert_eq!(fun_words(FunKind::Imported, 4), 15);
        assert_eq!(fun_words(FunKind::Imported, 0), 11);
    }

    // -- writers / accessors --------------------------------------------------

    #[test]
    fn local_fun_layout_is_header_uniq_index_env() {
        let mut heap = Heap::new(16);
        let free = [make_small(7), make_small(8)];
        let p = hp(&mut heap, fun_words(FunKind::Local, free.len()));
        // SAFETY: `p` addresses exactly `fun_words(Local, 2) == 5` written words.
        let f = unsafe { write_fun_local(p, 3, 0xDEAD_BEEF_CAFE, 9, &free) }.unwrap();
        assert_eq!(heap.used(), 5);
        // SAFETY: `f` is the live local fun just written.
        assert!(unsafe { is_any_fun(f) });
        assert_eq!(fun_block_words(f).unwrap(), 5);

        // SAFETY: `f` is the live local fun just written; every index is in block.
        unsafe {
            let q = boxed_val(f);
            assert_eq!(q, p, "the root points at the block base");
            assert_eq!(*q, make_fun_header(3, 2, FunKind::Local).unwrap());
            assert_eq!((*q.add(1)).raw() as u64, 0xDEAD_BEEF_CAFE);
            assert_eq!((*q.add(2)).raw() as u32, 9);
            assert_eq!(*q.add(3), make_small(7));
            assert_eq!(*q.add(4), make_small(8));
        }
        // SAFETY: `f` is a live local fun and the heap outlives the borrow.
        unsafe {
            assert_eq!(fun_uniq(f).unwrap(), 0xDEAD_BEEF_CAFE);
            assert_eq!(fun_index(f).unwrap(), 9);
            assert_eq!(fun_env(f).unwrap(), &free[..]);
        }
    }

    #[test]
    fn local_fun_without_free_variables_has_an_empty_env() {
        let mut heap = Heap::new(4);
        let p = hp(&mut heap, fun_words(FunKind::Local, 0));
        // SAFETY: `p` addresses exactly the 3 words of the block.
        let f = unsafe { write_fun_local(p, 0, 1, 2, &[]) }.unwrap();
        assert_eq!(heap.used(), 3);
        assert_eq!(fun_block_words(f).unwrap(), 3);
        // SAFETY: `f` is a live local fun.
        unsafe {
            assert_eq!(fun_env(f).unwrap(), &[] as &[Eterm]);
            assert_eq!(fun_uniq(f).unwrap(), 1);
            assert_eq!(fun_index(f).unwrap(), 2);
        }
    }

    #[test]
    fn external_fun_layout_is_header_module_function() {
        let a = atoms();
        let module = a.lookup("module").unwrap();
        let function = a.lookup("function").unwrap();
        let mut heap = Heap::new(8);
        let p = hp(&mut heap, fun_words(FunKind::External, 0));
        // SAFETY: `p` addresses exactly `fun_words(External, 0) == 3` words.
        let f = unsafe { write_fun_external(p, 2, module, function) }.unwrap();
        assert_eq!(heap.used(), 3);
        assert_eq!(fun_block_words(f).unwrap(), 3);
        // SAFETY: `f` is the live external fun just written.
        unsafe {
            let q = boxed_val(f);
            assert_eq!(*q, make_fun_header(2, 0, FunKind::External).unwrap());
            assert_eq!(*q.add(1), make_atom(module));
            assert_eq!(*q.add(2), make_atom(function));
        }
        // SAFETY: `f` is a live external fun.
        unsafe {
            assert_eq!(fun_module(f).unwrap(), module);
            assert_eq!(fun_function(f).unwrap(), function);
            assert_eq!(fun_env(f).unwrap(), &[] as &[Eterm]);
        }
    }

    #[test]
    fn imported_fun_layout_is_the_ten_word_descriptor() {
        let a = atoms();
        let module = a.lookup("module").unwrap();
        let node = a.lookup("node").unwrap();
        let mut uniq = [0u8; 16];
        for (i, byte) in uniq.iter_mut().enumerate() {
            *byte = i as u8 + 1; // 1..=16
        }
        let creator = FunPid {
            node,
            number: 0xAAAA_BBBB,
            serial: 0xCCCC_DDDD,
            creation: 0x1122_3344,
        };
        let free = [make_small(11)];
        let mut heap = Heap::new(16);
        let p = hp(&mut heap, fun_words(FunKind::Imported, free.len()));
        // SAFETY: `p` addresses exactly `fun_words(Imported, 1) == 12` words.
        let f =
            unsafe { write_fun_imported(p, 1, module, &uniq, 42, 7, 3, creator, &free) }.unwrap();
        assert_eq!(heap.used(), 12);
        assert_eq!(fun_block_words(f).unwrap(), 12);

        // SAFETY: `f` is the live imported fun just written; every index is in block.
        unsafe {
            let q = boxed_val(f);
            assert_eq!(*q, make_fun_header(1, 1, FunKind::Imported).unwrap());
            assert_eq!(*q.add(1), make_atom(module));
            assert_eq!((*q.add(2)).raw() as u64, from_le(&uniq[..8]));
            assert_eq!((*q.add(3)).raw() as u64, from_le(&uniq[8..]));
            assert_eq!((*q.add(4)).raw() as u32, 42);
            assert_eq!((*q.add(5)).raw() as u32, 7);
            assert_eq!((*q.add(6)).raw() as u32, 3);
            assert_eq!(*q.add(7), make_atom(node));
            assert_eq!((*q.add(8)).raw() as u32, 0xAAAA_BBBB);
            assert_eq!((*q.add(9)).raw() as u32, 0xCCCC_DDDD);
            assert_eq!((*q.add(10)).raw() as u32, 0x1122_3344);
            assert_eq!(*q.add(11), make_small(11));
        }

        // SAFETY: `f` is a live imported fun and the heap outlives the borrow.
        unsafe {
            assert_eq!(fun_module(f).unwrap(), module);
            assert_eq!(fun_index(f).unwrap(), 42);
            assert_eq!(fun_old_index(f).unwrap(), 7);
            assert_eq!(fun_old_uniq(f).unwrap(), 3);
            assert_eq!(fun_creator_pid(f).unwrap(), creator);
            assert_eq!(fun_uniq_bytes(f).unwrap(), uniq);
            assert_eq!(fun_env(f).unwrap(), &free[..]);
        }
    }

    #[test]
    fn imported_fun_without_free_variables_is_eleven_words() {
        let a = atoms();
        let mut heap = Heap::new(16);
        let p = hp(&mut heap, fun_words(FunKind::Imported, 0));
        let creator = FunPid {
            node: a.lookup("node").unwrap(),
            number: 1,
            serial: 2,
            creation: 3,
        };
        // SAFETY: `p` addresses exactly the 11 words of the block.
        let f = unsafe {
            write_fun_imported(
                p,
                0,
                a.lookup("module").unwrap(),
                &[0u8; 16],
                9,
                9,
                9,
                creator,
                &[],
            )
        }
        .unwrap();
        assert_eq!(heap.used(), 11);
        assert_eq!(fun_block_words(f).unwrap(), 11);
        // SAFETY: `f` is a live imported fun.
        unsafe {
            assert_eq!(fun_env(f).unwrap(), &[] as &[Eterm]);
        }
    }

    #[test]
    fn uniq_bytes_round_trip_over_every_byte_value() {
        let a = atoms();
        for value in [0x00u8, 0x01, 0x7F, 0x80, 0xFF] {
            let mut uniq = [value; 16];
            uniq[0] = 0x5A;
            uniq[15] = 0xA5;
            let mut heap = Heap::new(16);
            let p = hp(&mut heap, fun_words(FunKind::Imported, 0));
            let creator = FunPid {
                node: a.lookup("node").unwrap(),
                number: 0,
                serial: 0,
                creation: 0,
            };
            // SAFETY: `p` addresses exactly the 11 words of the block.
            let f = unsafe {
                write_fun_imported(
                    p,
                    0,
                    a.lookup("module").unwrap(),
                    &uniq,
                    1,
                    2,
                    3,
                    creator,
                    &[],
                )
            }
            .unwrap();
            // SAFETY: `f` is a live imported fun.
            assert_eq!(unsafe { fun_uniq_bytes(f) }.unwrap(), uniq, "{value:#04x}");
        }
    }

    #[test]
    fn local_writer_reports_fields_over_255() {
        let mut heap = Heap::new(600);
        let p = hp(&mut heap, 300);
        // SAFETY: the error is returned before any word is written.
        assert_eq!(
            unsafe { write_fun_local(p, 256, 0, 0, &[]) },
            Err(TermError::ArityOutOfRange)
        );
        let free = vec![make_small(0); 256];
        // SAFETY: as above — the header check rejects `num_free = 256` first.
        assert_eq!(
            unsafe { write_fun_local(p, 1, 0, 0, &free) },
            Err(TermError::ArityOutOfRange)
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { write_fun_external(p, 256, Atom::from_raw(0), Atom::from_raw(1)) },
            Err(TermError::ArityOutOfRange)
        );
    }

    // -- wrong kind / wrong type ----------------------------------------------

    #[test]
    fn accessors_reject_the_wrong_fun_kind() {
        let a = atoms();
        let module = a.lookup("module").unwrap();
        let function = a.lookup("function").unwrap();
        let mut heap = Heap::new(64);

        let p = hp(&mut heap, fun_words(FunKind::Local, 0));
        // SAFETY: `p` addresses exactly the 3 words of the local block.
        let local = unsafe { write_fun_local(p, 0, 1, 2, &[]) }.unwrap();
        let p = hp(&mut heap, fun_words(FunKind::External, 0));
        // SAFETY: `p` addresses exactly the 3 words of the external block.
        let ext = unsafe { write_fun_external(p, 0, module, function) }.unwrap();
        let p = hp(&mut heap, fun_words(FunKind::Imported, 0));
        // SAFETY: `p` addresses exactly the 11 words of the imported block.
        let imp = unsafe {
            write_fun_imported(
                p,
                0,
                module,
                &[0u8; 16],
                1,
                2,
                3,
                FunPid {
                    node: module,
                    number: 4,
                    serial: 5,
                    creation: 6,
                },
                &[],
            )
        }
        .unwrap();

        // SAFETY: all three terms are live funs the heap owns.
        unsafe {
            // A local fun carries neither module, function, old_* nor creator pid.
            assert_eq!(fun_module(local), Err(TermError::BadFun));
            assert_eq!(fun_function(local), Err(TermError::BadFun));
            assert_eq!(fun_old_index(local), Err(TermError::BadFun));
            assert_eq!(fun_old_uniq(local), Err(TermError::BadFun));
            assert_eq!(fun_creator_pid(local), Err(TermError::BadFun));
            assert_eq!(fun_uniq_bytes(local), Err(TermError::BadFun));

            // An external fun has no index/uniq/old_*/creator pid.
            assert_eq!(fun_index(ext), Err(TermError::BadFun));
            assert_eq!(fun_uniq(ext), Err(TermError::BadFun));
            assert_eq!(fun_old_index(ext), Err(TermError::BadFun));
            assert_eq!(fun_old_uniq(ext), Err(TermError::BadFun));
            assert_eq!(fun_creator_pid(ext), Err(TermError::BadFun));
            assert_eq!(fun_uniq_bytes(ext), Err(TermError::BadFun));
            assert_eq!(fun_module(ext).unwrap(), module);
            assert_eq!(fun_function(ext).unwrap(), function);

            // An imported fun has no function name and no local `uniq`.
            assert_eq!(fun_function(imp), Err(TermError::BadFun));
            assert_eq!(fun_uniq(imp), Err(TermError::BadFun));
            assert_eq!(fun_module(imp).unwrap(), module);
            assert_eq!(fun_index(imp).unwrap(), 1);
        }
    }

    #[test]
    fn term_level_readers_reject_non_funs() {
        assert_eq!(fun_block_words(make_small(1)), Err(TermError::BadFun));
        assert_eq!(fun_block_words(NIL), Err(TermError::BadFun));
        let mut heap = Heap::new(8);
        // A boxed non-fun reaches the header read and is rejected by the subtag test.
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_eq!(fun_block_words(tuple), Err(TermError::BadFun));
        // SAFETY: every argument below is a live term the heap owns (or an immediate).
        unsafe {
            assert_eq!(fun_uniq(tuple), Err(TermError::BadFun));
            assert_eq!(fun_uniq_bytes(tuple), Err(TermError::BadFun));
            assert_eq!(fun_index(tuple), Err(TermError::BadFun));
            assert_eq!(fun_module(tuple), Err(TermError::BadFun));
            assert_eq!(fun_function(tuple), Err(TermError::BadFun));
            assert_eq!(fun_creator_pid(tuple), Err(TermError::BadFun));
            assert_eq!(fun_env(tuple), Err(TermError::BadFun));
            assert_eq!(fun_uniq(NIL), Err(TermError::BadFun));
            assert_eq!(fun_index(make_small(3)), Err(TermError::BadFun));
        }
    }

    #[test]
    fn fun_env_returns_compound_free_variables_in_order() {
        let mut heap = Heap::new(32);
        let inner = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let free = [inner, make_small(9), NIL];
        let p = hp(&mut heap, fun_words(FunKind::Local, free.len()));
        // SAFETY: `p` addresses exactly `fun_words(Local, 3) == 6` words.
        let f = unsafe { write_fun_local(p, 1, 0x42, 0, &free) }.unwrap();
        // SAFETY: `f` is a live local fun and the heap outlives the borrow.
        let env = unsafe { fun_env(f) }.unwrap();
        assert_eq!(env, &free[..]);
        assert_eq!(env[0], inner);
        assert_eq!(fun_block_words(f).unwrap(), 6);
    }
}
