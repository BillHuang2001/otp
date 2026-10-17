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
//! [`write_cons_at`]), so the word layout of a tuple/list/cons node is defined once.

use beam_common::{BeamError, Result};
use beam_term::{make_arityval, make_tuple, Eterm, NIL};

use super::Slack;
use crate::context::ExecutionContext;

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
/// proper list or a single cons cell, and all three are expressed with the methods
/// below. The implementation chooses *where* the words come from (one rooted
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
