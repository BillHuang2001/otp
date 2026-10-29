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

//! `pub(crate)` layout helpers for the copier: the beam_message-specific **identification** of
//! off-heap containers and external things, the **shape-validated** classification of a `SUB_BITS`
//! term ([`classify_sub_bits`], the copier's one validation point — `copy.rs` and `preflight.rs`
//! both route through it), plus the M1 off-heap reference encoding.
//!
//! Per-object word counts and per-object child-slot enumeration are **not** owned here. They are
//! the lowest-common-ancestor walk published by `beam_term::layout` — the owner of the frozen term
//! layout — and re-exported below, so every copier/sizer in the workspace shares one definition
//! instead of transcribing it per crate. The copier routes each shape through those canonical
//! enumerators: a `SUB_BITS` uses the **sizing** variant (an opaque leaf, because
//! `copy::copy_refc_binary` / `copy::copy_match_context` handle both `ErlSubBits` shapes as special
//! cases before the generic arm), while every other class uses the copy variant, which is identical
//! apart from that `SUB_BITS` divergence.
//!
//! Ground truth: the frozen `BinRef` container thing `[HEADER_BIN_REF, val, next]`
//! (`ERL_BIN_REF_SIZE = 3`, `beam_term::boxed`), where M1 stores the MSO slot in `val` and leaves
//! `next` unused (see the crate `CONTEXT.md`, Design Decisions).

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

/// The canonical per-object word count and child-slot enumerators, owned by the term-layout LCA
/// (`beam_term::layout`) and re-exported so the copier keeps a single `layout::…` prefix and the
/// definition lives in one spot. See that module for the per-shape tables and the `# Safety`
/// contract.
///
/// The **sizing** child-slot enumerator is deliberately absent: since the destination-pricing walk
/// became `beam_term::size`'s policy-parameterised walk, the only consumer of the sizing variant is
/// the retired reference walk (`copy_reference`, `#[cfg(test)]`-gated), which names
/// `beam_term::layout::for_each_sizing_child_slot` directly.
pub(crate) use beam_term::layout::{for_each_child_slot, object_words};

/// Whether `header` is the head word of an off-heap reference container.
pub(crate) fn is_off_heap_container(header: Eterm) -> bool {
    header.raw() == HEADER_BIN_REF.raw()
}

/// Whether the compound term `term` is an off-heap reference container.
///
/// Precondition (as [`is_bin_ref`]): when `term` is boxed, its header word must be a live, aligned
/// term word. The copier only calls this on a word whose primary tag is BOXED.
pub(crate) fn is_off_heap_term(term: Eterm) -> bool {
    if !is_boxed(term) {
        return false;
    }
    // SAFETY: `term` is boxed, so its header word is a live, initialized term word.
    is_off_heap_container(unsafe { *boxed_val(term) })
}

/// The two shapes a `SUB_BITS` term can take at the copier's dispatch point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubBitsKind {
    /// `orig` is a `BinRef`: a refc binary (or a window of one) whose payload is shared.
    RefcBinary,
    /// `orig` is an on-heap bitstring: an `ErlSubBits` match context, flattened on copy.
    MatchContext,
}

/// Classifies a `SUB_BITS` term after **validating** its shape — the copier's ONE `SUB_BITS`
/// validation point.
///
/// `Ok(None)` when `term` is not a `HEADER_SUB_BITS` block, so the copier goes on to treat it as an
/// ordinary boxed object. Otherwise the block's shape is proven **once** by the checked reader
/// [`try_sub_bits_parts`] (a `start <= end` window, a non-null masked base, and an `orig` that is a
/// `BinRef` with a non-null `val` or a well-formed on-heap bitstring), and only then is the `orig`
/// slot classified: a boxed `BinRef` is a refc binary (whose payload `copy_refc_binary` shares),
/// anything else is a match context (`copy_match_context` flattens it).
///
/// Both [`crate::copy`] and [`crate::preflight`] route through this function, so the price and the
/// copy can never disagree about what a `SUB_BITS` is, and no site dereferences `orig` before the
/// shape proof.
///
/// Distinct from the standalone MSO container ([`is_off_heap_term`], a bare `HEADER_BIN_REF` thing
/// whose `val` is an MSO slot): neither kind here is that.
pub(crate) fn classify_sub_bits(term: Eterm) -> Result<Option<SubBitsKind>> {
    if !is_boxed_term(term) {
        return Ok(None);
    }
    // SAFETY: `term` is a boxed term, so its header word is a live term word.
    if unsafe { *boxed_val(term) } != HEADER_SUB_BITS {
        return Ok(None);
    }
    // SAFETY: the header fixes `term` at the 5-word `ErlSubBits` layout, so the block's words are
    // readable; the reader validates the shape (window order, non-null base, a recognised `orig`)
    // and reports a mis-shaped block as a typed error instead of reading it.
    let (_base_flags, _start, _end, _orig) = unsafe { try_sub_bits_parts(term) }?;
    // SAFETY: the shape proof above pinned word 0 at the canonical `HEADER_SUB_BITS` and word 4 at
    // a recognised `orig`, so the test dereferences only a header word the proof made readable — a
    // shape guarantee, not a provenance one.
    Ok(Some(if unsafe { beam_term::refc::is_refc_binary(term) } {
        SubBitsKind::RefcBinary
    } else {
        SubBitsKind::MatchContext
    }))
}

/// Whether `header` is the head word of an external pid, port or reference (`ExternalThing`).
pub(crate) fn is_external_header(header: Eterm) -> bool {
    let subtag = header.raw() & TAG_HEADER_MASK;
    subtag == EXTERNAL_PID_SUBTAG || subtag == EXTERNAL_PORT_SUBTAG || subtag == EXTERNAL_REF_SUBTAG
}

/// The `(node, creation)` identity of the external pid/port/reference `term`, or `None` when
/// `header` is not an `ExternalThing` head word.
///
/// `term` must be a live external thing. The node is returned as the sysname atom term (the M1
/// substitution `beam_term::thing` documents) and `creation` from the thing's `next` slot.
pub(crate) fn external_node_creation(header: Eterm, term: Eterm) -> Option<(Eterm, u32)> {
    match header.raw() & TAG_HEADER_MASK {
        // SAFETY: `header` proves `term` is a live external thing of that kind, so the
        // `external_*_parts` preconditions hold.
        EXTERNAL_PID_SUBTAG => {
            let (node, _number, _serial, creation) = unsafe { external_pid_parts(term) };
            Some((node, creation))
        }
        EXTERNAL_PORT_SUBTAG => {
            let (node, _number, creation) = unsafe { external_port_parts(term) };
            Some((node, creation))
        }
        EXTERNAL_REF_SUBTAG => {
            let (node, _ids, creation) = unsafe { external_ref_parts(term) };
            Some((node, creation))
        }
        _ => None,
    }
}

/// The off-heap slot stored in the container `container` (its `val` word).
///
/// Precondition: `container` is a live off-heap reference container (`is_off_heap_term`).
pub(crate) fn off_heap_slot(container: Eterm) -> usize {
    // SAFETY: `container` is a live `BinRef` thing; word 1 is its `val`.
    unsafe { *boxed_val(container).add(1) }.raw()
}

/// Builds an off-heap reference container for `slot` on the destination heap `to`.
///
/// The M1 encoding is the frozen `BinRef` thing `[HEADER_BIN_REF, val, next]` where `val` is the
/// MSO slot index in the owning process's off-heap table and `next` is unused (`0`).
pub(crate) fn make_off_heap_container(to: &mut Heap, slot: usize) -> Result<Eterm> {
    // SAFETY: `alloc_words` returns `ERL_BIN_REF_SIZE` exclusively-owned, writable words.
    let p = unsafe { to.alloc_words(ERL_BIN_REF_SIZE)? };
    // SAFETY: `p` is the base of those three words.
    unsafe {
        *p = HEADER_BIN_REF;
        *p.add(1) = Eterm::from_raw(slot);
        *p.add(2) = Eterm::from_raw(0);
    }
    Ok(make_boxed(p as *const Eterm))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_heap_container_round_trip() {
        let mut heap = Heap::new(8).expect("test heap");
        let term = make_off_heap_container(&mut heap, 5).expect("the bin-ref fits");
        assert!(is_off_heap_term(term));
        // SAFETY: `term` is a live off-heap container whose header word is readable.
        assert!(is_off_heap_container(unsafe { *boxed_val(term) }));
        assert_eq!(off_heap_slot(term), 5);
        assert_eq!(heap.used(), ERL_BIN_REF_SIZE);
    }

    #[test]
    fn non_container_terms_are_not_off_heap() {
        let mut heap = Heap::new(8).expect("test heap");
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        assert!(!is_off_heap_term(tuple));
        assert!(!is_off_heap_term(make_small(1)));
        assert!(!is_off_heap_term(NIL));
    }
}
