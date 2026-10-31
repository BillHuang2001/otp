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

//! The **node sink**: the storage abstraction a shape writer carves the nodes of a
//! multi-node term out of.
//!
//! Two implementations, one per allocation discipline, so a shape (a
//! `{M,F,A,Location}` stacktrace item, its `[{file,File},{line,N}]` location, a
//! proper list) has exactly **one** definition regardless of which the caller can
//! use:
//!
//! * [`Slack`] — the **rooted** sink. The caller reserves the whole graph in ONE
//!   rooted allocation ([`reserve_slack`], [`crate::context::RootSet::ALL_LIVE`]) and
//!   every node is carved out of that single region via [`Slack::alloc_words`]. It
//!   never touches the context heap, so nothing can collect between nodes and the
//!   nodes already written (whose pointers are not roots) stay put — which is what
//!   makes a multi-node build correct under a **moving** collector.
//! * [`ContextSink`] — the **unrooted** sink. Each node is one
//!   [`ExecutionContext::alloc_words`], which never collects (only `ensure_heap`/
//!   `alloc_words_rooted` do). It is the one sanctioned exception for callers that
//!   hold **no** machine roots at all — the read-only `&self` execution-position
//!   accessors, whose `&self` receiver cannot lend the machine's `&mut` root slices
//!   ([`crate::machine::location`]).
//!
//! Both share the same low-level writers ([`write_tuple_at`], [`write_list_at`],
//! [`write_cons_at`], [`write_map_at`]), so the word layout of a tuple/list/cons/map
//! node is defined once.

use beam_common::{BeamError, Result};
use beam_term::{make_arityval, make_tuple, AtomTable, Eterm, NIL};

use super::Slack;
use crate::context::ExecutionContext;
use crate::error::unsupported;

/// Reject a tuple arity the frozen header cannot encode.
///
/// An `arityval` header stores the arity in 24 bits, so the runtime ceiling is
/// `MAX_ARITYVAL = (1 << 24) - 1` (`erl_term.h`, owned by [`beam_term::MAX_ARITYVAL`]).
/// A longer node would write a bogus header and read back as a different tuple, so the
/// [`Sink`] entry points that take an arbitrary slice raise the C's `system_limit`
/// instead. Both sinks call it **before** carving, so an oversized node is rejected
/// even when the sink has no room for it.
fn check_tuple_arity(n: usize) -> Result<()> {
    if n > beam_term::MAX_ARITYVAL {
        return Err(BeamError::SystemLimit);
    }
    Ok(())
}

/// A sink a term-shape writer carves its nodes out of.
///
/// The trait is deliberately tiny: every node a shape writer builds is a tuple, a
/// proper list, a single cons cell or a map, and all four are expressed with the
/// methods below. The implementation chooses *where* the words come from (one rooted
/// reservation carved node-by-node, or one `alloc_words` per node) and nothing else.
pub trait Sink {
    /// Build the tuple `{Elem, …}` out of `elems`, carving the `1 + elems.len()`-word
    /// block (`arityval` header + elements) from the sink.
    ///
    /// More than `MAX_ARITYVAL` (`(1 << 24) - 1`) elements cannot be encoded in an
    /// `arityval` header and is the typed [`BeamError::SystemLimit`].
    fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm>;

    /// Build the proper list `[Elem, …]` out of `elems` as `elems.len()` contiguous
    /// `[CAR, CDR]` cells (`2 * elems.len()` words) carved from the sink, linked
    /// front-to-back with the final `CDR` being [`NIL`]. The empty list is [`NIL`] and
    /// carves nothing.
    fn list(&mut self, elems: &[Eterm]) -> Result<Eterm>;

    /// Build the single cons cell `[Car | Cdr]` (`2` words) carved from the sink.
    ///
    /// The one-node **prepend** primitive: a shape writer that cannot express its
    /// whole graph as a proper list (a synthesized head item consed in front of a
    /// carved list) builds the cell with this, exactly once, so the carve order and
    /// the size computation cannot diverge. `Cdr` may be any term — an improper tail
    /// is representable.
    fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm>;

    /// Build the map `#{K => V, …}` out of `pairs`, carving
    /// [`beam_term::map::map_words_capacity`]`(pairs.len())` words from the sink.
    ///
    /// Delegates to [`beam_term::map::write_map`], which reproduces
    /// `erts_map_from_ks_and_vs`: duplicate keys collapse **last value wins**, at most
    /// `MAP_SMALL_MAP_LIMIT` (32) distinct keys become a flat map (keys in
    /// `erts_cmp_flatmap_keys` order) and a larger one a hash map — so the carve size
    /// is sound for either layout.
    ///
    /// `atoms` is the table `write_map` orders and compares keys with, and it must be
    /// the table `pairs`' atom words were read against (the same discipline
    /// [`beam_term::cmp`] follows). A one-pair map — what the synthesized BIF head
    /// item's `{error_info, #{module => FormatModule}}` extension builds — never
    /// compares a key, so the reservation is exactly consumed.
    fn map(&mut self, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Result<Eterm>;
}

/// The **rooted** sink: the slack of one whole-region rooted reservation
/// ([`reserve_slack`]), carved node-by-node.
impl Sink for Slack {
    fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        check_tuple_arity(elems.len())?;
        let hp = self.alloc_words(elems.len() + 1)?;
        // SAFETY: `alloc_words` returned `1 + elems.len()` exclusively-owned, writable
        // words inside this slack's reserved region, which nothing else writes.
        Ok(unsafe { write_tuple_at(hp, elems) })
    }

    fn list(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        if elems.is_empty() {
            return Ok(NIL);
        }
        let hp = self.alloc_words(2 * elems.len())?;
        // SAFETY: `alloc_words` returned `2 * elems.len()` exclusively-owned, writable
        // words inside this slack's reserved region, which nothing else writes.
        Ok(unsafe { write_list_at(hp, elems) })
    }

    fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        let hp = self.alloc_words(2)?;
        // SAFETY: `alloc_words` returned 2 exclusively-owned, writable words inside
        // this slack's reserved region, which nothing else writes.
        Ok(unsafe { write_cons_at(hp, car, cdr) })
    }

    fn map(&mut self, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Result<Eterm> {
        let words = beam_term::map::map_words_capacity(pairs.len());
        let hp = self.alloc_words(words)?;
        // SAFETY: `alloc_words` returned `map_words_capacity(pairs.len())`
        // exclusively-owned, writable words inside this slack's reserved region, which
        // nothing else writes — the precondition `write_map` documents.
        unsafe { write_map_at(hp, pairs, atoms) }
    }
}

/// The **unrooted** sink: one [`ExecutionContext::alloc_words`] per node.
///
/// # Moving safety
///
/// [`ExecutionContext::alloc_words`] hands out a stable pointer to `n` uninitialized
/// words that stay valid and unmoved until the next `ensure_heap`/collecting
/// allocation; it never collects itself. A caller may therefore only use this sink
/// for a shape whose operands are **not machine-heap terms held across an
/// allocation** — everything it builds must come from module metadata and immediates
/// (atoms, small ints, and nodes this same sink already built). A caller that *does*
/// hold machine-heap operands must use the rooted [`Slack`] sink instead, which
/// reserves the whole graph before reading any operand.
pub struct ContextSink<'a> {
    ctx: &'a mut dyn ExecutionContext,
}

impl<'a> ContextSink<'a> {
    /// A sink allocating each node through `ctx`.
    pub fn new(ctx: &'a mut dyn ExecutionContext) -> ContextSink<'a> {
        ContextSink { ctx }
    }
}

impl Sink for ContextSink<'_> {
    fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        check_tuple_arity(elems.len())?;
        let hp = self.ctx.alloc_words(elems.len() + 1)?;
        // SAFETY: `alloc_words` returned `1 + elems.len()` words valid until the next
        // collecting allocation; the write below is the very next thing this sink
        // does, and `alloc_words` never collects.
        Ok(unsafe { write_tuple_at(hp, elems) })
    }

    fn list(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        if elems.is_empty() {
            return Ok(NIL);
        }
        let hp = self.ctx.alloc_words(2 * elems.len())?;
        // SAFETY: `alloc_words` returned `2 * elems.len()` words valid until the next
        // collecting allocation; the write below is the very next thing this sink
        // does, and `alloc_words` never collects.
        Ok(unsafe { write_list_at(hp, elems) })
    }

    fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        let hp = self.ctx.alloc_words(2)?;
        // SAFETY: `alloc_words` returned 2 words valid until the next collecting
        // allocation; the write below is the very next thing this sink does, and
        // `alloc_words` never collects.
        Ok(unsafe { write_cons_at(hp, car, cdr) })
    }

    fn map(&mut self, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Result<Eterm> {
        let words = beam_term::map::map_words_capacity(pairs.len());
        let hp = self.ctx.alloc_words(words)?;
        // SAFETY: `alloc_words` returned `map_words_capacity(pairs.len())` words valid
        // until the next collecting allocation; the write below is the very next thing
        // this sink does, and `alloc_words` never collects — the precondition
        // `write_map` documents.
        unsafe { write_map_at(hp, pairs, atoms) }
    }
}

/// The words [`deep_copy`] reserves for a boxed object headed by `header` at `ptr` —
/// the **copy** shape, which differs from
/// [`beam_term::layout::object_words`] for an `ErlSubBits` exactly as that module
/// documents: a refc binary's `ErlSubBits` block is copied verbatim as its own
/// [`beam_term::ERL_SUB_BITS_SIZE`] words while its `orig` (the `BinRef`) is re-pointed
/// at a separately deep-copied `BinRef` — so the pair still costs
/// [`beam_term::ERL_REFC_BITS_SIZE`] in total, but is carved as two blocks.
fn copy_words(header: Eterm, ptr: *const Eterm) -> usize {
    if header.raw() & beam_term::TAG_HEADER_MASK == beam_term::SUB_BITS_SUBTAG {
        beam_term::ERL_SUB_BITS_SIZE
    } else {
        // SAFETY: `ptr` addresses a live object headed by `header` — the caller
        // (`deep_copy`) only invokes this for a live, boxed term, which is the same
        // caller contract `object_words` documents.
        unsafe { beam_term::layout::object_words(header, ptr) }
    }
}

/// Whether `term` is a **match context** — an `ErlSubBits` whose `orig` is boxed and
/// not a `BinRef` ([`beam_term::HEADER_BIN_REF`]).
///
/// [`deep_copy_budget`] declines these (see there). [`deep_copy`] is never handed one
/// by a caller that has sized with that budget: a match context nested inside a bigger
/// argument graph instead surfaces as a slack overrun (the sizing shape under-counts
/// the copy shape), which is still a typed decline rather than a short block.
fn is_match_context(term: Eterm) -> bool {
    if !beam_term::is_boxed_term(term) {
        return false;
    }
    let src = beam_term::boxed_val(term);
    // SAFETY: `term` is a live boxed term, so its header word is readable.
    let header = unsafe { *src };
    if header.raw() & beam_term::TAG_HEADER_MASK != beam_term::SUB_BITS_SUBTAG {
        return false;
    }
    // SAFETY: an `ErlSubBits` header proves word `SUB_BITS_ORIG_WORD` is initialized.
    let orig = unsafe { *src.add(beam_term::SUB_BITS_ORIG_WORD) };
    beam_term::is_boxed_term(orig)
        && {
            // SAFETY: `orig` is boxed, so its header word is live.
            unsafe { *beam_term::boxed_val(orig) != beam_term::HEADER_BIN_REF }
        }
}

/// The words [`deep_copy`] will carve out of `slack` for `term` — its copy budget — or
/// `None` when the graph cannot be sized to the copy shape.
///
/// Sized from [`beam_term::size::size_object`] (the `size_object_x(t, NULL)` source
/// occupancy count): an immediate and a literal are **shared verbatim** by
/// [`deep_copy`], so they cost `0`, while every compound object is priced once per
/// occurrence — the same count the deep copy's per-occurrence carve needs, so the
/// budget is never **below** what the copy actually carves.
///
/// The one graph whose sizing shape disagrees with the copy shape is a **match
/// context** — an `ErlSubBits` whose `orig` is boxed and not a `BinRef`:
/// `size_object` prices its fresh copied window
/// (`heap_bits_size_words(end - start)`, `copy.c:232-239`) while [`deep_copy`] copies
/// the 5-word block plus the **full** underlying heap-bits — an under-reserve. It is
/// declined here (a typed `None`) rather than trusted.
pub(crate) fn deep_copy_budget(term: Eterm) -> Option<usize> {
    match beam_term::primary_tag(term) {
        // An immediate is shared verbatim and costs nothing.
        beam_term::PrimaryTag::Immed1 => Some(0),
        // A literal is shared verbatim (it is never relocated by a moving collector).
        beam_term::PrimaryTag::List if beam_term::is_literal(term) => Some(0),
        beam_term::PrimaryTag::Boxed if beam_term::is_literal(term) => Some(0),
        beam_term::PrimaryTag::List => Some(beam_term::size::size_object(term)),
        beam_term::PrimaryTag::Boxed => {
            if is_match_context(term) {
                None
            } else {
                Some(beam_term::size::size_object(term))
            }
        }
        // Not a term (a bare header word): nothing to copy, and never a valid argument.
        beam_term::PrimaryTag::Header => None,
    }
}

/// Deep-copy `term` into `slack`, making its whole graph self-contained inside the
/// slack's reserved region so it moves **with** the item that references it.
///
/// * an **immediate** and a **literal** ([`beam_term::is_literal`]) are shared
///   verbatim — neither is ever relocated by a moving collector;
/// * a **cons cell** is copied as its header-less 2-word `[CAR, CDR]` pair, with the
///   two words re-pointed at their own deep copies;
/// * a **boxed object** is copied verbatim as [`copy_words`] words and every copy
///   child slot (via [`beam_term::layout::for_each_child_slot`]) re-pointed at its own
///   deep copy.
///
/// The caller has reserved at least [`deep_copy_budget`]`(term)` words, so no carve
/// overruns the slack; an overrun is the typed `unsupported` from [`Slack::alloc_words`],
/// never a short block. This is the engine's own **copy shape** — the sibling of
/// `beam_gc::copy`'s move/copy, which is `pub(crate)` to that crate and therefore not
/// reachable here — used where a baked term must carry its boxed subgraphs with it.
pub(crate) fn deep_copy(slack: &mut Slack, term: Eterm) -> Result<Eterm> {
    match beam_term::primary_tag(term) {
        beam_term::PrimaryTag::Immed1 => Ok(term),
        beam_term::PrimaryTag::List if beam_term::is_literal(term) => Ok(term),
        beam_term::PrimaryTag::Boxed if beam_term::is_literal(term) => Ok(term),
        beam_term::PrimaryTag::List => {
            let src = beam_term::list_val(term);
            let hp = slack.alloc_words(2)?;
            // SAFETY: `hp` is 2 exclusively-owned words inside the slack's reserved
            // region, and `src` is a live 2-word cons cell; the two ranges are distinct
            // (the slack's region is freshly reserved, never the source term's block).
            unsafe { core::ptr::copy_nonoverlapping(src, hp, 2) };
            // SAFETY: the 2 words at `hp` are now the verbatim `[CAR, CDR]` pair.
            let (car, cdr) = unsafe { (*hp, *hp.add(1)) };
            let car = deep_copy(slack, car)?;
            let cdr = deep_copy(slack, cdr)?;
            // SAFETY: the 2 words at `hp` are exclusively owned by this cell.
            unsafe {
                *hp = car;
                *hp.add(1) = cdr;
            }
            Ok(beam_term::make_list(hp))
        }
        beam_term::PrimaryTag::Boxed => {
            let src = beam_term::boxed_val(term);
            // SAFETY: `term` is a live boxed term, so its header word is readable.
            let header = unsafe { *src };
            let words = copy_words(header, src);
            let hp = slack.alloc_words(words)?;
            // SAFETY: `hp` is `words` exclusively-owned words inside the slack's reserved
            // region, and `src` is a live object of `header`'s shape of exactly `words`
            // words; the ranges are distinct for the same reason as the cons arm.
            unsafe { core::ptr::copy_nonoverlapping(src, hp, words) };

            // Re-point each copy child slot at its own deep copy. The offsets are
            // collected first, so the copy and the recursion borrow the slack disjointly.
            let mut offsets = Vec::new();
            // SAFETY: `hp` now holds a verbatim copy of the live object headed by
            // `header`, so its child-slot offsets are the source's too.
            unsafe {
                beam_term::layout::for_each_child_slot(header, hp, |offset| {
                    offsets.push(offset);
                });
            }
            for offset in offsets {
                // SAFETY: `offset` is one of the object's copy child-slot offsets, so
                // `hp.add(offset)` addresses a slot of the copied object.
                let child = unsafe { *hp.add(offset) };
                let copied = deep_copy(slack, child)?;
                // SAFETY: as above — the slot is exclusively owned by the copied object.
                unsafe { *hp.add(offset) = copied };
            }
            Ok(beam_term::make_boxed(hp))
        }
        // Not a term (a bare header word): nothing to copy, and never a valid argument.
        beam_term::PrimaryTag::Header => Err(unsupported("deep_copy: not a boxed/list term")),
    }
}

/// Write the tuple `{elems…}` into `1 + elems.len()` words at `hp`.
///
/// # Safety
///
/// `hp` must point at `1 + elems.len()` exclusively-owned, writable words that
/// nothing else writes and that stay put for as long as the returned term is live.
/// `elems.len()` must not exceed `MAX_ARITYVAL` (the header's 24-bit arity field): both
/// [`Sink::tuple`] entry points enforce that through [`check_tuple_arity`] before
/// carving, and the one other caller
/// ([`make_tuple_rooted`](crate::terms::make_tuple_rooted)) rejects it up front too.
pub(crate) unsafe fn write_tuple_at(hp: *mut Eterm, elems: &[Eterm]) -> Eterm {
    // SAFETY: the caller guarantees `1 + elems.len()` exclusive words at `hp`, with
    // `elems.len() <= MAX_ARITYVAL`; word 0 receives the `arityval` header and words
    // `1..=n` the elements.
    unsafe {
        *hp = make_arityval(elems.len());
        for (i, elem) in elems.iter().enumerate() {
            *hp.add(1 + i) = *elem;
        }
    }
    make_tuple(hp)
}

/// Write the proper list `[elems…]` into `2 * elems.len()` words at `hp`.
///
/// # Safety
///
/// `hp` must point at `2 * elems.len()` exclusively-owned, writable words that
/// nothing else writes and that stay put for as long as the returned term is live.
/// `elems` must not be empty (the empty list is [`NIL`] and has no block).
pub(crate) unsafe fn write_list_at(hp: *mut Eterm, elems: &[Eterm]) -> Eterm {
    let n = elems.len();
    // SAFETY: the caller guarantees `2 * n` exclusive words at `hp`; the cells are
    // `[CAR, CDR]` pairs tagged `TAG_PRIMARY_LIST`, the last `CDR` is `NIL` and every
    // earlier `CDR` points at the next contiguous cell inside the same block.
    unsafe {
        for (i, elem) in elems.iter().enumerate() {
            *hp.add(2 * i) = *elem;
            *hp.add(2 * i + 1) = if i + 1 == n {
                NIL
            } else {
                beam_term::make_list(hp.add(2 * (i + 1)))
            };
        }
    }
    beam_term::make_list(hp)
}

/// Write the cons cell `[car | cdr]` into `2` words at `hp`.
///
/// # Safety
///
/// `hp` must point at `2` exclusively-owned, writable words that nothing else writes
/// and that stay put for as long as the returned term is live.
pub(crate) unsafe fn write_cons_at(hp: *mut Eterm, car: Eterm, cdr: Eterm) -> Eterm {
    // SAFETY: the caller guarantees 2 exclusive words at `hp`; word 0 receives the
    // `CAR` and word 1 the `CDR`, which is exactly the header-less 2-word cons cell
    // tagged by `make_list`.
    unsafe {
        *hp = car;
        *hp.add(1) = cdr;
    }
    beam_term::make_list(hp)
}

/// Write the map `#{pairs…}` into
/// [`beam_term::map::map_words_capacity`]`(pairs.len())` words at `hp`.
///
/// The one definition of the map node both sinks carve, so the rooted and the
/// unrooted path cannot differ in layout (they differ only in *where* the words
/// come from).
///
/// # Safety
///
/// `hp` must point at [`beam_term::map::map_words_capacity`]`(pairs.len())`
/// exclusively-owned, writable words that nothing else writes and that stay put for
/// as long as the returned term is live — the precondition
/// [`beam_term::map::write_map`] documents.
pub(crate) unsafe fn write_map_at(
    hp: *mut Eterm,
    pairs: &[(Eterm, Eterm)],
    atoms: &AtomTable,
) -> Result<Eterm> {
    // SAFETY: the caller guarantees `map_words_capacity(pairs.len())` exclusive words
    // at `hp`; `write_map` writes every word of the produced map and never more than
    // that (deduplication can only shrink it).
    let map = unsafe { beam_term::map::write_map(hp, pairs, atoms) }?;
    Ok(map)
}

