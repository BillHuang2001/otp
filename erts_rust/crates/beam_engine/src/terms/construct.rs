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
//! Every builder follows the module's frozen **reserve, then read, then write**
//! ordering ([`super::slack`]): it reserves its whole block with **one** rooted
//! allocation
//! ([`Machine::alloc_rooted`](crate::machine::Machine::alloc_rooted) →
//! [`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted)),
//! then invokes its reader closure (which reads operands from the machine's
//! *post-collection* state and may materialize a non-small integer into the block's
//! spare tail via [`Slack`]), then writes the header/element words. Nothing
//! allocates from the BEAM heap between the reserve and the final write.
use beam_common::{BeamError, Result};
use beam_loader::Module;
use beam_term::{make_atom, Eterm};

use super::{reserve, Slack, ERL_ONHEAP_BINARY_LIMIT, HEAP_BIN_HEAD_WORDS, WORD_BYTES};
use crate::context::ExecutionContext;
use crate::machine::Machine;

/// Builds a tuple term from `n_elems` elements the `read` closure supplies.
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
pub fn make_tuple<R>(
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
    // SAFETY: the reserve block is live and exclusively owned; word 0 receives the
    // arityval header and words `1..=n_elems` the elements read after the reserve.
    unsafe {
        *hp = beam_term::make_arityval(n_elems);
        for (i, elem) in elems.iter().enumerate() {
            *hp.add(1 + i) = *elem;
        }
    }
    Ok(beam_term::make_tuple(hp))
}

/// Builds a single cons cell `[Head | Tail]` the `read` closure supplies.
///
/// Reserves two words **rooted**, then reads `(head, tail)` and writes the
/// header-less `[CAR, CDR]` cell tagged `TAG_PRIMARY_LIST` (via
/// [`make_list`](beam_term::make_list)) — exactly the `put_list` layout of
/// `emu/instrs.tab:732-737` (`HTOP[0] = $Hd; HTOP[1] = $Tl; $Dst = make_list(HTOP)`).
/// `tail` need **not** be a list: `[H | T]` with an improper `T` is a legal Erlang
/// term, so no shape check is performed here. `extra` words of spare tail are handed
/// to the reader for a bignum operand.
pub fn make_cons<R>(
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

/// Builds a proper list term from `n` elements the `read` closure supplies.
///
/// Reserves `2 * n + extra` words **rooted** in **one** allocation and lays the `n`
/// `[CAR, CDR]` cells out **contiguously**, linked front-to-back, the final `CDR`
/// being [`NIL`](beam_term::NIL). Contiguity is now a **requirement**, not merely an
/// equivalence: a single reservation is what lets the owner survive the (single)
/// collecting allocation, whereas the previous per-cell fold would allocate a fresh
/// block for every cell and strand the earlier ones. An empty list (`n == 0`) returns
/// `NIL` and allocates nothing; a closure returning the wrong element count is a typed
/// [`crate::error::unsupported`].
pub fn make_list<R>(
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
        return Err(crate::error::unsupported("make_list: element count mismatch"));
    }
    // SAFETY: the reserve block holds `2 * n` live, exclusively-owned cell words and
    // the `extra` tail; the cells are `[CAR, CDR]` pairs tagged `TAG_PRIMARY_LIST`, the
    // last `CDR` is `NIL` and every earlier `CDR` points at the next contiguous cell.
    unsafe {
        for (i, elem) in elems.iter().enumerate() {
            *hp.add(2 * i) = *elem;
            *hp.add(2 * i + 1) = if i + 1 == n {
                beam_term::NIL
            } else {
                beam_term::make_list(hp.add(2 * (i + 1)))
            };
        }
    }
    Ok(beam_term::make_list(hp))
}

/// The `{badmatch, Value}` exception reason (class `error`) for a failed
/// tuple/list match; `read` supplies the offending `Value`.
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
///
/// The reason is a 3-word tuple reserved **rooted** before the closure reads the
/// value from the machine's post-collection state.
pub fn badmatch_reason<R>(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    live: u32,
    module: &Module,
    read: R,
) -> Result<Eterm>
where
    R: FnOnce(&Machine, &mut Slack) -> Result<Eterm>,
{
    let tag = module
        .atom_table
        .lookup("badmatch")
        .ok_or(BeamError::BadMatch)?;
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

/// Builds an on-heap binary term from `bytes`.
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
pub fn make_heap_binary(
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
