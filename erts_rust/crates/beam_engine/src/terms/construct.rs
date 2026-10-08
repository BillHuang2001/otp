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

//! The heap-seam term **builders**: tuples, cons cells, lists, the `badmatch`
//! reason and the hand-rolled heap binary. Each obtains its storage through
//! [`ExecutionContext::alloc_words`](crate::context::ExecutionContext::alloc_words).
use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{make_atom, Eterm};

use super::{ERL_ONHEAP_BINARY_LIMIT, HEAP_BIN_HEAD_WORDS, WORD_BYTES};
use crate::context::ExecutionContext;

/// Builds a tuple term from `elems`.
///
/// Allocates `1 + elems.len()` words: an [`arityval`](beam_term::make_arityval)
/// header followed by the elements (the `make_tuple`/`tuple_val` layout of
/// `beam_term::tuple`). An empty slice returns the shared zero-arity literal
/// [`tuple0`](beam_term::tuple0) (C `ERTS_GLOBAL_LIT_EMPTY_TUPLE`).
///
/// More than `MAX_ARITYVAL` (`(1 << 24) - 1`) elements yields
/// [`BeamError::SystemLimit`].
pub fn make_tuple(ctx: &mut dyn ExecutionContext, elems: &[Eterm]) -> Result<Eterm> {
    if elems.is_empty() {
        return Ok(beam_term::tuple0());
    }
    if elems.len() > beam_term::MAX_ARITYVAL {
        return Err(BeamError::SystemLimit);
    }
    let hp = ctx.alloc_words(1 + elems.len())?;
    // SAFETY: `alloc_words` returned `1 + n` exclusively-owned, writable words;
    // word 0 receives the arityval header and words `1..=n` the elements.
    let root = unsafe {
        *hp = beam_term::make_arityval(elems.len());
        for (i, elem) in elems.iter().enumerate() {
            *hp.add(1 + i) = *elem;
        }
        hp
    };
    Ok(beam_term::make_tuple(root))
}

/// Builds a single cons cell `[Head | Tail]`.
///
/// Allocates two words and writes the header-less `[CAR, CDR]` cell tagged
/// `TAG_PRIMARY_LIST` (via [`make_list`](beam_term::make_list)) — exactly the
/// `put_list` layout of `emu/instrs.tab:732-737` (`HTOP[0] = $Hd; HTOP[1] = $Tl;
/// $Dst = make_list(HTOP)`). `tail` need **not** be a list: `[H | T]` with an
/// improper `T` is a legal Erlang term, so no shape check is performed here.
pub fn make_cons(ctx: &mut dyn ExecutionContext, head: Eterm, tail: Eterm) -> Result<Eterm> {
    let hp = ctx.alloc_words(2)?;
    // SAFETY: `alloc_words` returned 2 exclusively-owned, writable words — one
    // `[CAR, CDR]` cell; `make_list` tags the pointer as a list term.
    unsafe {
        *hp = head;
        *hp.add(1) = tail;
    }
    Ok(beam_term::make_list(hp))
}

/// Builds a proper list term from `elems`.
///
/// One `[CAR, CDR]` cell per element, linked front-to-back by [`make_cons`], the
/// final `CDR` being [`NIL`](beam_term::NIL). An empty slice returns `NIL`. The
/// cells are individually allocated — the C `make_list` allocates `2n` contiguous
/// words, but contiguity is *not* part of the frozen term layout, so building on
/// [`make_cons`] is equivalent.
pub fn make_list(ctx: &mut dyn ExecutionContext, elems: &[Eterm]) -> Result<Eterm> {
    let mut list = beam_term::NIL;
    for elem in elems.iter().rev() {
        list = make_cons(ctx, *elem, list)?;
    }
    Ok(list)
}

/// The `{badmatch, Value}` exception reason (class `error`) for a failed
/// tuple/list match.
///
/// ERTS tags the reason with the **global** atom `badmatch` (`am_badmatch`,
/// `beam_common.c:388`; the C `badmatch`/`get_*` opcodes store `freason = BADMATCH`
/// and the tag is materialized during error handling). This engine's atoms are
/// **module-local** indices (the frozen term model, `../CONTEXT.md` §2), so the tag
/// is resolved through `module`'s atom table — the only table whose indices the
/// loaded code compares against. The Erlang compiler never interns `badmatch` (the
/// raising opcode uses the C global constant), so a real module normally lacks it;
/// minting an arbitrary atom index there would be a *wrong result*, so this returns
/// the canonical typed [`BeamError::BadMatch`] instead.
pub fn badmatch_reason(
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    value: Eterm,
) -> Result<Eterm> {
    let tag = module
        .atom_table
        .lookup("badmatch")
        .ok_or(BeamError::BadMatch)?;
    make_tuple(ctx, &[make_atom(tag), value])
}

/// Builds an on-heap binary term from `bytes`.
///
/// # Frozen word layout
///
/// `ErlHeapBits` (`erl_bits.h:149-164`) is `[thing_word, size, data…]`, where the
/// header is `_make_header(heap_bits_size(num_bits) - 1, HEAP_BITS_SUBTAG)` and
/// `size` is the **bit** count; the data words follow at word 2 (the block is
/// `2 + ceil(num_bytes / word_bytes)` words, `erl_bits.h:156-158`).
///
/// Only *heap* binaries (at most `ERL_ONHEAP_BINARY_LIMIT`, 64, bytes) are built;
/// a longer binary needs the off-heap ref-counted representation, which is not
/// implemented and yields [`crate::error::unsupported`].
pub fn make_heap_binary(ctx: &mut dyn ExecutionContext, bytes: &[u8]) -> Result<Eterm> {
    let num_bytes = bytes.len();
    if num_bytes > ERL_ONHEAP_BINARY_LIMIT {
        return Err(crate::error::unsupported("heap binary > 64 bytes"));
    }
    let data_words = num_bytes.div_ceil(WORD_BYTES);
    let num_words = HEAP_BIN_HEAD_WORDS + data_words;
    let hp = ctx.alloc_words(num_words)?;
    // SAFETY: `alloc_words` returned `num_words = 2 + ceil(num_bytes / word_bytes)`
    // exclusively-owned, writable words. Word 0 receives the heap-bits header (the
    // block size minus the header word), word 1 the bit count and the data section
    // at word 2 has room for `num_bytes` bytes.
    unsafe {
        *hp = beam_term::make_header(num_words - 1, beam_term::HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw((num_bytes * 8) as usize);
        let data = hp.add(HEAP_BIN_HEAD_WORDS).cast::<u8>();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, num_bytes);
    }
    Ok(beam_term::make_boxed(hp))
}
