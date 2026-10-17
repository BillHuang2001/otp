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
//! reason and the hand-rolled heap binary.
//!
//! Two families live here, one per allocation seam:
//!
//! * the **classic** builders (`make_tuple`, `make_cons`, `make_list`,
//!   `badmatch_reason`, `make_heap_binary`) take already-materialized operands and
//!   obtain their storage with a plain
//!   [`ExecutionContext::alloc_words`](crate::context::ExecutionContext::alloc_words)
//!   call. They are the *public* API consumed cross-crate (by `beam_rt` and the
//!   integration tests); they hold no term across the allocation, so they are safe
//!   against a host whose `alloc_words` never collects (which the seam contract
//!   reserves for [`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted)).
//! * the **rooted** `*_rooted` builders, which follow the module's frozen **reserve,
//!   then read, then write** ordering ([`super::slack`]): they reserve their whole
//!   block with **one** rooted allocation
//!   ([`Machine::alloc_rooted`](crate::machine::Machine::alloc_rooted) →
//!   [`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted)),
//!   then invoke a reader closure (which reads operands from the machine's
//!   *post-collection* state and may materialize a non-small integer into the block's
//!   spare tail via [`Slack`]), then write the header/element words. Nothing
//!   allocates from the BEAM heap between the reserve and the final write.
use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{make_atom, Eterm};

use super::{reserve, Slack, ERL_ONHEAP_BINARY_LIMIT, HEAP_BIN_HEAD_WORDS, WORD_BYTES};
use crate::context::ExecutionContext;
use crate::machine::Machine;

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
/// and the tag is materialized during error handling). The tag is resolved through
/// [`crate::machine::namespace_atom`], i.e. the host's live atom namespace
/// ([`ExecutionContext::atoms`]) **first** and `module`'s load-time snapshot
/// **second**: a host that owns ONE shared namespace (the runtime) can name an atom
/// interned *after* the module was loaded, and the Erlang compiler never interns
/// `badmatch` (the raising opcode uses the C global constant), so a real module
/// normally lacks it in its snapshot. A name **neither** source knows yields the
/// canonical typed [`BeamError::BadMatch`] — minting an arbitrary atom index would be
/// a *wrong result*, because an index is valid only against the table it was read
/// with.
pub fn badmatch_reason(
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    value: Eterm,
) -> Result<Eterm> {
    let tag = crate::machine::namespace_atom(ctx, module, "badmatch").ok_or(BeamError::BadMatch)?;
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
///
/// The whole data section is **zeroed before** the payload is copied in, so the
/// trailing padding bytes of the last data word are zero rather than stale heap
/// content — the same discipline as
/// [`beam_term::bitstring::write_heap_bits`], which the tuple/map/fun writers also
/// follow. Without it a sub-word binary would carry heap garbage in the bits past its
/// bit count.
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
    // block size minus the header word), word 1 the bit count, and the data section at
    // word 2 is written in full: its `data_words * WORD_BYTES` bytes are first zeroed
    // (which initialises the padding tail of the last word) and the first `num_bytes`
    // of them then receive the payload. Every write stays inside the same
    // exclusively-owned block, and `data_words * WORD_BYTES` is at most `num_bytes + 7`.
    unsafe {
        *hp = beam_term::make_header(num_words - 1, beam_term::HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw((num_bytes * 8) as usize);
        let data = hp.add(HEAP_BIN_HEAD_WORDS).cast::<u8>();
        core::ptr::write_bytes(data, 0, data_words * WORD_BYTES);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, num_bytes);
    }
    Ok(beam_term::make_boxed(hp))
}

/// The **rooted** twin of [`make_tuple`]: builds a tuple from `n_elems` elements
/// the `read` closure supplies.
///
/// Reserves `1 + n_elems + extra` words **rooted** (an [`arityval`](beam_term::make_arityval)
/// header, the elements, then an `extra`-word spare tail) and then calls
/// `read(&Machine, &mut Slack)`, which must return exactly `n_elems` elements read
/// from the machine's post-collection state. Reserving before reading is what makes
/// the build correct under a moving collector: the operands are read *after* the one
/// collecting allocation, into a block whose own pointer is never a root.
///
/// An empty tuple (`n_elems == 0`) returns the shared zero-arity literal
/// [`tuple0`](beam_term::tuple0) (C `ERTS_GLOBAL_LIT_EMPTY_TUPLE`) and allocates
/// nothing. More than `MAX_ARITYVAL` (`(1 << 24) - 1`) elements yields
/// [`BeamError::SystemLimit`]; a closure returning the wrong element count is a typed
/// [`crate::error::unsupported`], never a wrong term.
pub fn make_tuple_rooted<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    n_elems: usize,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Vec<Eterm>>,
{
    if n_elems == 0 {
        return Ok(beam_term::tuple0());
    }
    if n_elems > beam_term::MAX_ARITYVAL {
        return Err(BeamError::SystemLimit);
    }
    let hp = reserve(machine, ctx, 1 + n_elems + extra, live)?;
    // SAFETY: the reserve returned `1 + n_elems + extra` exclusively-owned, writable
    // words; the `extra`-word tail at `hp + 1 + n_elems` is exactly what the slack
    // hands the reader (it is not aliased by the machine).
    let mut slack = unsafe { Slack::new(hp.add(1 + n_elems), extra) };
    let elems = read(machine, &mut slack)?;
    if elems.len() != n_elems {
        return Err(crate::error::unsupported(
            "make_tuple: element count mismatch",
        ));
    }
    // SAFETY: the reserve block is live and exclusively owned and holds
    // `1 + n_elems` words; `elems` was checked to hold exactly `n_elems`.
    Ok(unsafe { super::write_tuple_at(hp, &elems) })
}

/// The **rooted** twin of [`make_cons`]: builds a single cons cell `[Head | Tail]`
/// the `read` closure supplies.
///
/// Reserves two words **rooted**, then reads `(head, tail)` and writes the
/// header-less `[CAR, CDR]` cell tagged `TAG_PRIMARY_LIST` (via
/// [`make_list`](beam_term::make_list)) — exactly the `put_list` layout of
/// `emu/instrs.tab:732-737` (`HTOP[0] = $Hd; HTOP[1] = $Tl; $Dst = make_list(HTOP)`).
/// `tail` need **not** be a list: `[H | T]` with an improper `T` is a legal Erlang
/// term, so no shape check is performed here. `extra` words of spare tail are handed
/// to the reader for a bignum operand.
pub fn make_cons_rooted<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<(Eterm, Eterm)>,
{
    let hp = reserve(machine, ctx, 2 + extra, live)?;
    // SAFETY: the reserve returned `2 + extra` exclusively-owned, writable words; the
    // `extra`-word tail after the two cell words is the slack.
    let mut slack = unsafe { Slack::new(hp.add(2), extra) };
    let (head, tail) = read(machine, &mut slack)?;
    // SAFETY: the reserve block is live and exclusively owned; both cell words are
    // written and `make_list` tags the pointer as a list term.
    unsafe {
        *hp = head;
        *hp.add(1) = tail;
    }
    Ok(beam_term::make_list(hp))
}

/// The **rooted** twin of [`make_list`]: builds a proper list term from `n` elements
/// the `read` closure supplies.
///
/// Reserves `2 * n + extra` words **rooted** in **one** allocation and lays the `n`
/// `[CAR, CDR]` cells out **contiguously**, linked front-to-back, the final `CDR`
/// being [`NIL`](beam_term::NIL). Contiguity is now a **requirement**, not merely an
/// equivalence: a single reservation is what lets the owner survive the (single)
/// collecting allocation, whereas the previous per-cell fold would allocate a fresh
/// block for every cell and strand the earlier ones. An empty list (`n == 0`) returns
/// `NIL` and allocates nothing; a closure returning the wrong element count is a typed
/// [`crate::error::unsupported`].
pub fn make_list_rooted<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    n: usize,
    extra: usize,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Vec<Eterm>>,
{
    if n == 0 {
        return Ok(beam_term::NIL);
    }
    let hp = reserve(machine, ctx, 2 * n + extra, live)?;
    // SAFETY: the reserve returned `2 * n + extra` exclusively-owned, writable words;
    // the `extra`-word tail after the `2 * n` cell words is the slack.
    let mut slack = unsafe { Slack::new(hp.add(2 * n), extra) };
    let elems = read(machine, &mut slack)?;
    if elems.len() != n {
        return Err(crate::error::unsupported(
            "make_list: element count mismatch",
        ));
    }
    // SAFETY: the reserve block holds `2 * n` live, exclusively-owned cell words (the
    // `extra` tail follows them) and `elems` was checked to hold exactly `n`
    // elements, so the cells are `n` proper `[CAR, CDR]` pairs.
    Ok(unsafe { super::write_list_at(hp, &elems) })
}

/// The **rooted** twin of [`badmatch_reason`]: the `{badmatch, Value}` exception
/// reason (class `error`) for a failed tuple/list match; `read` supplies the
/// offending `Value`.
///
/// ERTS tags the reason with the **global** atom `badmatch` (`am_badmatch`,
/// `beam_common.c:388`; the C `badmatch`/`get_*` opcodes store `freason = BADMATCH`
/// and the tag is materialized during error handling). The tag is resolved through
/// [`crate::machine::namespace_atom`] — the host's live atom namespace
/// ([`ExecutionContext::atoms`]) first, `module`'s load-time snapshot second — so a
/// host that owns ONE shared namespace can name a tag the module's snapshot lacks
/// (the Erlang compiler never interns `badmatch`). A name **neither** source knows
/// yields the canonical typed [`BeamError::BadMatch`], never a substituted index.
///
/// The reason is a 3-word tuple reserved **rooted** before the closure reads the
/// value from the machine's post-collection state. The lookup deliberately happens
/// **before** the [`reserve`], so the `&dyn ExecutionContext` borrow `namespace_atom`
/// takes ends before the `&mut` reservation.
pub fn badmatch_reason_rooted<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    module: &Module,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Eterm>,
{
    let tag = crate::machine::namespace_atom(ctx, module, "badmatch").ok_or(BeamError::BadMatch)?;
    let hp = reserve(machine, ctx, 3, live)?;
    // SAFETY: the reserve returned 3 exclusively-owned, writable words and a
    // zero-length tail (the slack has no room, which is correct: the closure's value
    // is one term the caller has already materialized as a live root).
    let mut slack = unsafe { Slack::new(hp.add(3), 0) };
    let value = read(machine, &mut slack)?;
    // SAFETY: the reserve block is live and exclusively owned; word 0 receives the
    // arityval header, word 1 the `badmatch` atom and word 2 the value.
    unsafe {
        *hp = beam_term::make_arityval(2);
        *hp.add(1) = make_atom(tag);
        *hp.add(2) = value;
    }
    Ok(beam_term::make_tuple(hp))
}

/// The **rooted** twin of [`make_heap_binary`]: builds an on-heap binary term from
/// `bytes`.
///
/// # Frozen word layout
///
/// `ErlHeapBits` (`erl_bits.h:149-164`) is `[thing_word, size, data…]`, where the
/// header is `_make_header(heap_bits_size(num_bits) - 1, HEAP_BITS_SUBTAG)` and
/// `size` is the **bit** count; the data words follow at word 2 (the block is
/// `2 + ceil(num_bytes / word_bytes)` words, `erl_bits.h:156-158`).
///
/// The block has no `Eterm` operand, so it is reserved **rooted** with the whole X
/// file declared live ([`RootSet::ALL_LIVE`](crate::context::RootSet::ALL_LIVE)) and
/// then written. Only *heap* binaries (at most `ERL_ONHEAP_BINARY_LIMIT`, 64, bytes)
/// are built; a longer binary needs the off-heap ref-counted representation, which
/// yields [`crate::error::unsupported`].
///
/// The data section is zeroed before the payload copy exactly as in
/// [`make_heap_binary`], so the padding tail of the last data word never carries
/// stale heap content.
pub fn make_heap_binary_rooted(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    bytes: &[u8],
) -> Result<Eterm> {
    let num_bytes = bytes.len();
    if num_bytes > ERL_ONHEAP_BINARY_LIMIT {
        return Err(crate::error::unsupported("heap binary > 64 bytes"));
    }
    let data_words = num_bytes.div_ceil(WORD_BYTES);
    let num_words = HEAP_BIN_HEAD_WORDS + data_words;
    let hp = reserve(
        machine,
        ctx,
        num_words,
        crate::context::RootSet::ALL_LIVE,
    )?;
    // SAFETY: the reserve returned `num_words = 2 + ceil(num_bytes / word_bytes)`
    // exclusively-owned, writable words. Word 0 receives the heap-bits header (the
    // block size minus the header word), word 1 the bit count, and the data section at
    // word 2 is written in full: its `data_words * WORD_BYTES` bytes are first zeroed
    // (which initialises the padding tail of the last word) and the first `num_bytes`
    // of them then receive the payload. Every write stays inside the same
    // exclusively-owned block, and `data_words * WORD_BYTES` is at most `num_bytes + 7`.
    unsafe {
        *hp = beam_term::make_header(num_words - 1, beam_term::HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw((num_bytes * 8) as usize);
        let data = hp.add(HEAP_BIN_HEAD_WORDS).cast::<u8>();
        core::ptr::write_bytes(data, 0, data_words * WORD_BYTES);
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, num_bytes);
    }
    Ok(beam_term::make_boxed(hp))
}
