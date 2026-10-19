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

//! The **wire form of an outbound payload**: a message term that carries one of this
//! runtime's own pids, ports or references re-materialised with them as *external*
//! identifiers.
//!
//! # The problem
//!
//! `beam_dist` encodes every outbound control message with an [`Encoder`] built from the
//! message's atom table and **no** [`Encoder::with_local_node`] identity
//! (`beam_dist/src/control.rs::encode_control_cached`). An **internal**
//! pid/port/reference term carries no node of its own — its node is the runtime's, which
//! the codec deliberately does not know — so such a term has no wire form and the encode
//! refuses with `beam_codec`'s `unsupported_external_term`. That makes the canonical
//! distributed idiom `Peer ! {reply, self()}` fail with a typed capability gap even
//! though the connection, the `From` slot and the framing are all correct.
//!
//! # The two halves
//!
//! * [`holds_local_id`] — the **gate**. A cheap, allocation-free walk that answers "does
//!   this payload carry an internal pid, port or reference?". A payload that does not —
//!   the overwhelmingly common case: atoms, numbers, lists of them, binaries, another
//!   node's pids — is sent exactly as before, with no copy and no allocation.
//! * [`PayloadArena::externalize`] — the **rewrite**, for a payload the gate accepted.
//!   It encodes the term with this node's identity
//!   ([`Encoder::with_local_node`]) and decodes the image back onto its own arena, so
//!   every internal identifier comes back as the external thing the wire carries while
//!   every other subterm is the same term it was.
//!
//! # Why an encode/decode round trip instead of a structural rewrite
//!
//! A hand-written rewriter would have to reproduce the interior of **every** term shape
//! (`beam_term` owns those layouts, and this crate forbids `unsafe` outside its two
//! documented zones) — or copy-paste `beam_message`'s copier. The codec already walks
//! every shape correctly, in both directions, and its `with_local_node` path *is* the
//! mapping this module needs (`enc_internal_pid` → `NEW_PID_EXT` → a decoded external
//! pid). The round trip is therefore the one existing implementation of the transform,
//! and it is exact by construction: no shape is special-cased, and nothing is invented.
//!
//! A payload the round trip cannot carry (a `make_fun3` local fun, whose wire form needs
//! a module identity no seam supplies here) is a typed error from the codec — never a
//! silently altered payload. A payload the gate cannot *see into* (below) is returned
//! unchanged, so the encoder's own honest `unsupported_external_term` refusal stands.
//!
//! # Coverage, and the honest residuals
//!
//! The gate walks **lists**, **tuples** and **flat maps** — the shapes a message payload
//! is built from. It deliberately treats three shapes as leaves, so a local identifier
//! hidden inside one of them keeps today's honest `external_term` refusal rather than
//! risking a wrong term:
//!
//! * a **hash map** (> [`MAP_SMALL_MAP_LIMIT`](beam_term::map::MAP_SMALL_MAP_LIMIT)
//!   pairs — inspecting one needs an allocated pair list);
//! * a **fun's environment** (and an *export* fun therefore narrows to a leaf);
//! * a **native record**'s field values.
//!
//! The same is true of anything the *encoder* cannot render: both routes end in a typed
//! `BeamError`, never in a different term.
//!
//! # The arena's lifetime contract
//!
//! A term [`PayloadArena::externalize`] returns lives on the arena, so the arena may only
//! be reset or replaced at the **start** of an externalize — never while a term it
//! returned is still live. The single call site
//! ([`DistClientInner::outbound_payload`](crate::dist_client)) consumes the term
//! synchronously: the driver encodes the control into its own output buffer inside the
//! same call, so nothing outlives the send.

use beam_alloc::Heap;
use beam_codec::{decode_term_body, Encoder};
use beam_common::{BeamError, Result};
use beam_term::prelude::{atom_val, is_atom, is_internal_pid, is_internal_port, is_list, Eterm};
use beam_term::{size_object, AtomTable};

/// The words a fresh [`PayloadArena`] starts with (8 KiB): enough for the canonical
/// `{reply, self()}` payload, grown on demand for anything larger.
const INITIAL_ARENA_WORDS: usize = 1 << 10;

/// The largest arena a payload may be materialised in (4 M words = 32 MiB). A payload
/// that needs more is a typed error from the decode, never an unbounded allocation.
const MAX_ARENA_WORDS: usize = 1 << 22;

/// The head-room the first estimate adds: an external identifier is wider than the
/// internal one it replaces (0 → 4 words for a pid), so the decoded term is never
/// smaller than the term it came from.
const ARENA_SLACK_WORDS: usize = 1 << 10;

/// Whether `term` carries one of this runtime's **own** identifiers — an internal pid,
/// an internal port or an internal reference — in a position this module rewrites.
///
/// This is the gate the outbound send path runs first, so it never allocates: it walks
/// lists iteratively (a long spine costs no native stack) and recurses only through
/// tuple and flat-map elements — the same recursion the ETF encoder itself performs over
/// the shapes it accepts.
///
/// A word that is not a term at all, and every leaf family (atoms, numbers, binaries,
/// external identifiers, funs, records, hash maps), answers `false`: the gate is allowed
/// to *miss* a local identifier (the encoder then refuses that payload exactly as it does
/// today) but it must never claim one is absent from a term that has none.
pub(crate) fn holds_local_id(term: Eterm) -> bool {
    if is_internal_pid(term) || is_internal_port(term) {
        return true;
    }
    // `beam_term`'s own `is_internal_ref` is `unsafe`, and this crate forbids `unsafe`
    // outside its two documented zones; `beam_codec::internal_ref_id` is the safe
    // frozen-layout probe over the same block and answers `Ok` for an internal
    // reference and only for one.
    if beam_codec::internal_ref_id(term).is_ok() {
        return true;
    }
    if is_list(term) {
        return list_holds_local_id(term);
    }
    // Each probe below matches its own header subtag, so none can fire for a shape
    // already classified above or for a non-term word.
    if let Ok(arity) = beam_codec::term::tuple_arity(term) {
        return (0..arity).any(|index| {
            matches!(beam_codec::term::tuple_element(term, index), Ok(elem) if holds_local_id(elem))
        });
    }
    if let Ok((keys, values)) = beam_codec::term::flatmap_parts(term) {
        return keys.iter().chain(values).any(|&elem| holds_local_id(elem));
    }
    false
}

/// [`holds_local_id`] over a cons cell's spine, iteratively.
///
/// An unreadable cell or an absent tail cannot happen for a word `is_list` accepted, so
/// the `Err` arms are a decline (the payload keeps today's honest refusal), never a
/// panic and never a substituted answer.
fn list_holds_local_id(term: Eterm) -> bool {
    let mut cell = term;
    loop {
        let Ok(head) = beam_codec::term::cons_head(cell) else {
            return false;
        };
        if holds_local_id(head) {
            return true;
        }
        let Ok(tail) = beam_codec::term::cons_tail(cell) else {
            return false;
        };
        if !is_list(tail) {
            // A proper list ends in `[]` (an immediate, never a local id); an improper
            // tail is any other term, walked once here.
            return holds_local_id(tail);
        }
        cell = tail;
    }
}

/// The arena an externalised outbound payload is materialised on.
///
/// One per [`DistClient`](crate::dist_client::DistClient): kept (and only grown) across
/// sends, because the term it returns is consumed by the encoder inside the same call —
/// see the module docs' lifetime contract.
pub(crate) struct PayloadArena {
    /// The bump heap the decoded payload is written on.
    heap: Heap,
}

impl PayloadArena {
    /// An empty arena at [`INITIAL_ARENA_WORDS`].
    ///
    /// # Errors
    ///
    /// The carrier allocator's own failure.
    pub(crate) fn new() -> Result<PayloadArena> {
        Ok(PayloadArena {
            heap: Heap::new(INITIAL_ARENA_WORDS)?,
        })
    }

    /// The **wire form** of `payload`: `payload` itself when the gate finds no local
    /// identifier in it, else a copy whose internal pids/ports/references are the
    /// external terms the wire carries.
    ///
    /// `node` is this runtime's node **atom term** and `creation` its creation — the same
    /// pair [`DistClientInner::local_pid_term`](crate::dist_client) builds the message's
    /// `From` slot from.
    ///
    /// # Errors
    ///
    /// A codec failure on the payload's own shape (an encoding the wire has no image for,
    /// a `make_fun3` local fun without a module identity) and the arena's allocation
    /// failures. A payload that cannot be rewritten is **never** silently altered: the
    /// caller sees the typed error, and the send fails exactly as an unencodable payload
    /// does today.
    pub(crate) fn externalize(
        &mut self,
        payload: Eterm,
        atoms: &mut AtomTable,
        node: Eterm,
        creation: u32,
    ) -> Result<Eterm> {
        if !holds_local_id(payload) {
            return Ok(payload);
        }
        // Without a node identity there is nothing to write into the id's node slot, so
        // the payload keeps its internal terms and the encoder's own refusal stands.
        if !is_atom(node) {
            return Ok(payload);
        }
        let node_atom = atom_val(node);
        // The image this runtime's own encoder *would* have written had `beam_dist` known
        // this node's identity: one encode, whatever the arena ends up needing.
        let image = {
            let mut encoder = Encoder::new_body(atoms).with_local_node(node_atom, creation);
            encoder.encode(payload)?;
            encoder.finish()
        };
        let mut words = initial_words(payload);
        loop {
            self.reserve(words)?;
            self.heap.reset();
            match decode_term_body(&image, &mut self.heap, atoms) {
                Ok(term) => return Ok(term),
                // The first estimate cannot be exact — a decoded term is not the shape it
                // came from (an internal pid is 0 words and its external form 4; an
                // off-heap refc binary is 8 and the on-heap bitstring it decodes to is
                // `2 + bytes/8`) — so exhaustion grows the arena and retries instead of
                // reporting a failure.
                Err(BeamError::Alloc(_)) if words < MAX_ARENA_WORDS => {
                    words = words.saturating_mul(2).min(MAX_ARENA_WORDS);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Grows the arena to at least `words` (never shrinks it).
    ///
    /// Only ever called at the start of [`PayloadArena::externalize`], because replacing
    /// the heap invalidates every term an earlier call returned.
    fn reserve(&mut self, words: usize) -> Result<()> {
        if self.heap.capacity() < words {
            self.heap = Heap::new(words)?;
        }
        Ok(())
    }
}

/// The arena size the first decode attempt is given: the payload's own word count with
/// head-room for the wider external identifiers, clamped to the arena's own bounds.
fn initial_words(payload: Eterm) -> usize {
    size_object(payload)
        .saturating_mul(2)
        .saturating_add(ARENA_SLACK_WORDS)
        .clamp(INITIAL_ARENA_WORDS, MAX_ARENA_WORDS)
}

/// The module's suite: the gate over every walkable shape and the round trip that turns a
/// local identifier into its wire form (see `dist/externalize/tests.rs`).
#[cfg(test)]
mod tests;
