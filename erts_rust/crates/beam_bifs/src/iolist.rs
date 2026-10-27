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

//! The crate's **one** iolist walk, its byte-list builder and the
//! `erts_binary_part` byte-range rule.
//!
//! Everything a `Unit = 8` iolist can be flattened into lives here, so the
//! `binary`/`erlang` BIF families cannot drift apart:
//!
//! * [`walk`] — the single iterative traversal (`erl_iolist.c`'s
//!   `generic_iolist_size`/`generic_iolist_copy` acceptance rules), over a
//!   [`WalkStack`] that keeps the ordinary case allocation-free;
//! * [`byte_len`] / [`bytes`] / [`bytes_and_leaves`] — the three things a caller
//!   wants from it: a **measured** size (no buffer at all), the flattened bytes in
//!   one exactly-reserved buffer, or the bytes plus the leaf count;
//! * [`part_range`] — the `[Start, Start + Len)` rule of `erts_binary_part`
//!   (`erts/emulator/beam/erl_bif_binary.c:2137`), shared by `erlang:binary_part/2,3`,
//!   `binary:part/2,3`, `binary:bin_to_list/2,3` and `binary:match/2,3`'s
//!   `{scope, …}` option;
//! * [`byte_list`] — the one byte-list builder (one small integer per byte).
//!
//! # Why a single walk
//!
//! The crate used to carry two iolist flatteners with the same acceptance set — a
//! work-stack + growth-capacity bit buffer in `binary.rs` and an exact-capacity
//! two-pass walk in `binconv.rs` — plus two spellings of the `binary:part` range
//! rule. Both differences were pure cost, and a divergence between them would be a
//! *wrong answer* for the same term depending on which BIF saw it, so they are one
//! implementation here. The remaining crate-local walks are deliberately **not**
//! this walk: `erlang:list_to_bitstring/1`'s (`Unit = 1`) assembles bits, `port.rs`'s
//! `intlist_to_buf` accepts only a proper list of bytes, and `re`'s option iodata
//! accepts an improper byte tail (`[1|2]`) that an iolist rejects.
//!
//! # The measured cost
//!
//! [`bytes`] measures first and then fills a buffer reserved to the iolist's exact
//! byte count, and [`walk`] copies a binary element straight into the caller's
//! destination through the closure-scoped borrow [`bits::with_bytes`] (never a
//! per-element temporary). `tests/binconv_alloc.rs` pins the resulting counts.
use beam_common::Result;
use beam_term::{signed_value, Eterm, NIL};
use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, system_limit};

/// The [`Eterm`] slots the iolist walk keeps **inline** — a plain array on the Rust
/// stack — before it spills to a heap `Vec` (see [`WalkStack`]).
///
/// One list level costs one frame (its pending tail) plus one for the element being
/// expanded, so 128 frames cover an iolist nested roughly 127 levels deep: far past
/// any real iolist, while the whole stack stays a 1 KiB stack footprint. A deeper
/// iolist is not rejected — the frames past this depth go into the spill `Vec`, the
/// walk's only possible allocation.
const INLINE_WALK_FRAMES: usize = 128;

/// The iolist walk's work stack: [`INLINE_WALK_FRAMES`] slots inline, spilling to a
/// `Vec` only when a deeper iolist needs them — so a flat or moderately nested iolist
/// allocates **nothing** for the walk itself.
///
/// LIFO order is preserved across the spill: a frame reaches the `Vec` only once the
/// inline array is full, and is popped from the `Vec` first for the same reason.
struct WalkStack {
    inline: [Eterm; INLINE_WALK_FRAMES],
    len: usize,
    spill: Vec<Eterm>,
}

impl WalkStack {
    /// An empty stack (each pass pushes its own root).
    fn empty() -> Self {
        Self {
            inline: [NIL; INLINE_WALK_FRAMES],
            len: 0,
            spill: Vec::new(),
        }
    }

    fn push(&mut self, term: Eterm) {
        if self.len < INLINE_WALK_FRAMES {
            self.inline[self.len] = term;
            self.len += 1;
        } else {
            self.spill.push(term);
        }
    }

    fn pop(&mut self) -> Option<Eterm> {
        if let Some(term) = self.spill.pop() {
            return Some(term);
        }
        self.len = self.len.checked_sub(1)?;
        Some(self.inline[self.len])
    }
}

/// The shape an iolist argument's **top level** may take.
///
/// The two BIF families disagree only here: `erlang:list_to_binary/1` and
/// `binary:list_to_bin/1` require a list or `[]`, while `erlang:iolist_to_binary/1`,
/// `erlang:iolist_size/1` and every iodata consumer (the checksums, `zlib`) also
/// accept a bare **byte-aligned** binary.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopLevel {
    /// A list or `[]`.
    List,
    /// A list, `[]` or a byte-aligned binary.
    ListOrBinary,
}

/// One leaf of an iolist walk: a byte element or a binary element.
///
/// A `Binary` leaf borrows the term's own storage and is only valid for the duration
/// of the [`walk`] callback that received it.
pub(crate) enum Leaf<'a> {
    /// An integer element in `0..=255`.
    Byte(u8),
    /// A byte-aligned bitstring element.
    Binary(&'a [u8]),
}

impl Leaf<'_> {
    /// The number of bytes this leaf contributes.
    fn len(&self) -> usize {
        match self {
            Leaf::Byte(_) => 1,
            Leaf::Binary(bytes) => bytes.len(),
        }
    }

    /// Appends this leaf's bytes to `out`.
    fn append_to(&self, out: &mut Vec<u8>) {
        match self {
            Leaf::Byte(byte) => out.push(*byte),
            Leaf::Binary(bytes) => out.extend_from_slice(bytes),
        }
    }
}

/// Walks the iolist `root`, handing every leaf to `visit` in order.
///
/// The accepted set is `erl_iolist.c`'s (`generic_iolist_size` /
/// `generic_iolist_copy`): a list element is a byte (`0..=255`), a byte-aligned
/// bitstring, or a nested list; a cons **tail** may be `[]`, a cons, or a byte-aligned
/// bitstring (`[1|<<2>>]` is a valid iolist), and anything else is `badarg`. Under
/// [`TopLevel::List`] the top level itself must be a list or `[]`.
///
/// The walk is iterative (a [`WalkStack`], exactly as C's `erts_iolist_to_buf` is),
/// so a deeply nested iolist cannot overflow the Rust stack, and it allocates nothing
/// for an iolist nested within [`INLINE_WALK_FRAMES`] levels.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an element that is none of
/// the three shapes — including a non-byte-aligned bitstring, an integer outside
/// `0..=255` and an improper tail that is not a binary; otherwise whatever `visit`
/// returns.
pub(crate) fn walk<F>(root: Eterm, top: TopLevel, mut visit: F) -> Result<()>
where
    F: FnMut(Leaf<'_>) -> Result<()>,
{
    let top_ok = access::is_nil(root)
        || access::is_list(root)
        || match top {
            TopLevel::List => false,
            TopLevel::ListOrBinary => bits::is_binary(root),
        };
    if !top_ok {
        return Err(badarg());
    }
    let mut stack = WalkStack::empty();
    stack.push(root);
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            // A byte. The range check is the validation, not just a conversion.
            let byte = u8::try_from(signed_value(term)).map_err(|_| badarg())?;
            visit(Leaf::Byte(byte))?;
            continue;
        }
        if bits::is_binary(term) {
            // A binary element is handed over **without** copying it out of the term.
            bits::with_bytes(term, |bytes| visit(Leaf::Binary(bytes)))?;
            continue;
        }
        if access::is_list(term) {
            // The tail of a list is a *tail* position: for an iolist (as in C's
            // `erts_iolist_to_buf`) it may only be another list, `[]` or a binary.
            let tail = access::cdr(term);
            if !access::is_list(tail) && !access::is_nil(tail) && !bits::is_binary(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is popped (and appended) first.
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    Ok(())
}

/// The byte length of the iolist `root`, **validating** every element on the way.
///
/// No buffer is built, so this is the whole cost of `erlang:iolist_size/1`: a
/// zero-allocation walk for an iolist nested within [`INLINE_WALK_FRAMES`] levels.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) exactly where [`walk`]
/// documents it; `system_limit` only for a byte count that cannot fit a `usize` (an
/// iolist of more than `usize::MAX` bytes, which no heap could hold).
pub(crate) fn byte_len(root: Eterm, top: TopLevel) -> Result<usize> {
    let mut total = 0usize;
    walk(root, top, |leaf| {
        total = total.checked_add(leaf.len()).ok_or_else(system_limit)?;
        Ok(())
    })?;
    Ok(total)
}

/// The bytes the iolist `root` denotes, or an empty `Vec` for an empty iolist.
///
/// The buffer is reserved to the iolist's exact byte count (a pre-pass), so it never
/// reallocates, and no per-element temporary is built.
///
/// # Errors
///
/// As [`walk`].
pub(crate) fn bytes(root: Eterm, top: TopLevel) -> Result<Vec<u8>> {
    let total = byte_len(root, top)?;
    let mut out = Vec::with_capacity(total);
    walk(root, top, |leaf| {
        leaf.append_to(&mut out);
        Ok(())
    })?;
    Ok(out)
}

/// The bytes the iolist `root` denotes, plus the number of **leaf** components the
/// walk visited (a byte or a binary element, an empty binary included).
///
/// The count is what `erlang:adler32/2`'s input flattening observes: each leaf is one
/// `sumfun` call in `do_chksum` (`erts/emulator/beam/erl_bif_chksum.c`), so a
/// zero-length call — which reduces a non-canonical caller-supplied accumulator —
/// happens exactly when the count is non-zero.
///
/// # Errors
///
/// As [`walk`].
pub(crate) fn bytes_and_leaves(root: Eterm, top: TopLevel) -> Result<(Vec<u8>, usize)> {
    let total = byte_len(root, top)?;
    let mut out = Vec::with_capacity(total);
    let mut leaves = 0usize;
    walk(root, top, |leaf| {
        leaves += 1;
        leaf.append_to(&mut out);
        Ok(())
    })?;
    Ok((out, leaves))
}

/// Validates the `[Start, Start + Len)` byte range against a buffer of `size` bytes,
/// exactly like `erts_binary_part` (`erts/emulator/beam/erl_bif_binary.c:2137`): a
/// negative `Len` is the reverse-part form (`|Len|` bytes *ending* at `Start`), and a
/// range that is negative or reaches past the end is `badarg`.
///
/// `Start` is a non-negative integer and `Len` an integer of either sign; a
/// non-integer of either kind is `badarg`, as is a `Start` that is negative.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any of the above;
/// `system_limit` never (every accepted small integer fits a `usize` on a 64-bit
/// target, and an out-of-range one is `badarg` in the oracle too).
pub(crate) fn part_range(size: usize, epos: Eterm, elen: Eterm) -> Result<(usize, usize)> {
    let mut start = as_uint(epos).ok_or_else(badarg)?;
    let mut len = as_sint(elen).ok_or_else(badarg)?;
    if len < 0 {
        // `-(i64::MIN)` overflows: `checked_neg` reports that as `badarg`.
        let back = u64::try_from(len.checked_neg().ok_or_else(badarg)?).map_err(|_| badarg())?;
        if back > start {
            return Err(badarg());
        }
        len = -len;
        start -= back;
    }
    let len = u64::try_from(len).map_err(|_| badarg())?;
    let end = start.checked_add(len).ok_or_else(badarg)?;
    if end > size as u64 {
        return Err(badarg());
    }
    Ok((start as usize, len as usize))
}

/// The byte list `[b0, b1, …]` of `bytes`, built on the context heap — the C's
/// `erts_bld_string_n`/`buf_to_intlist` shape: one small integer per byte, so a
/// non-ASCII character becomes its UTF-8 bytes in a caller that renders one.
///
/// # Errors
///
/// Propagates the heap's allocation failure.
pub(crate) fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// `term_to_Uint`: a non-negative integer as a `u64`.
///
/// A bignum answers `None`: the frozen term's small range covers every offset a
/// binary this runtime can hold needs, and the oracle's `term_to_Uint` rejects a
/// bignum that does not fit a machine word anyway.
fn as_uint(term: Eterm) -> Option<u64> {
    if access::is_small(term) {
        u64::try_from(signed_value(term)).ok()
    } else {
        None
    }
}

/// `term_to_Sint`: an integer of either sign (a float is not one).
fn as_sint(term: Eterm) -> Option<i64> {
    if access::is_small(term) {
        Some(signed_value(term))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn heap() -> Heap {
        Heap::new(1024).expect("test heap")
    }

    fn atoms() -> AtomTable {
        AtomTable::new()
    }

    /// The byte list `[b0, b1, …]` of `bytes` on `heap` (the walk's own input shape).
    fn byte_iolist(heap: &mut Heap, bytes: &[u8]) -> Eterm {
        let mut list = NIL;
        for &byte in bytes.iter().rev() {
            list = heap.cons(make_small(i64::from(byte)), list).expect("fits");
        }
        list
    }

    #[test]
    fn a_flat_byte_list_measures_and_flattens_to_its_bytes() {
        let mut heap = heap();
        let list = byte_iolist(&mut heap, &[1, 2, 3, 255]);
        assert_eq!(byte_len(list, TopLevel::List).unwrap(), 4);
        assert_eq!(bytes(list, TopLevel::List).unwrap(), vec![1, 2, 3, 255]);
        // `[]` is the empty iolist: no bytes, and the `Vec` reserves nothing.
        assert_eq!(byte_len(NIL, TopLevel::List).unwrap(), 0);
        assert_eq!(bytes(NIL, TopLevel::List).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn the_top_level_rule_splits_the_two_families() {
        let mut heap = heap();
        let bin = beam_codec::term::make_binary(&mut heap, b"ab").expect("fits");
        // `list_to_binary/1`'s rule: a bare binary is not a list.
        assert!(matches!(bytes(bin, TopLevel::List), Err(BeamError::BadArg)));
        // `iolist_to_binary/1`'s rule: a bare byte-aligned binary is accepted.
        assert_eq!(bytes(bin, TopLevel::ListOrBinary).unwrap(), b"ab".to_vec());
        // A bare *non-byte-aligned* bitstring is `badarg` under both.
        let odd = beam_codec::term::make_bitstring(&mut heap, &[0b1000_0000], 3).expect("fits");
        assert!(matches!(bytes(odd, TopLevel::ListOrBinary), Err(BeamError::BadArg)));
        // ... and so is a bare integer under either rule, while `[]` is the empty
        // iolist (accepted by both).
        assert!(matches!(bytes(make_small(7), TopLevel::List), Err(BeamError::BadArg)));
        assert!(matches!(bytes(make_small(7), TopLevel::ListOrBinary), Err(BeamError::BadArg)));
        assert!(bytes(NIL, TopLevel::ListOrBinary).unwrap().is_empty());
    }

    #[test]
    fn an_improper_tail_must_be_a_binary() {
        let mut heap = heap();
        let bin = beam_codec::term::make_binary(&mut heap, b"ab").expect("fits");
        // `[1|<<2>>]` is a valid iolist ...
        let with_bin_tail = heap.cons(make_small(1), bin).expect("fits");
        assert_eq!(bytes(with_bin_tail, TopLevel::List).unwrap(), b"\x01ab".to_vec());
        // ... a byte tail (`[1|2]`) is not.
        let byte_tail = heap.cons(make_small(1), make_small(2)).expect("fits");
        assert!(matches!(bytes(byte_tail, TopLevel::List), Err(BeamError::BadArg)));
    }

    #[test]
    fn the_leaf_count_distinguishes_an_empty_leaf_from_no_leaf() {
        let mut heap = heap();
        let empty_bin = beam_codec::term::make_binary(&mut heap, b"").expect("fits");
        let (bytes_out, leaves) = bytes_and_leaves(NIL, TopLevel::List).unwrap();
        assert!(bytes_out.is_empty());
        assert_eq!(leaves, 0, "an empty iolist visits no leaf");
        let one_empty = heap.cons(empty_bin, NIL).expect("fits");
        let (bytes_out, leaves) = bytes_and_leaves(one_empty, TopLevel::List).unwrap();
        assert!(bytes_out.is_empty());
        assert_eq!(leaves, 1, "an empty binary leaf is still one leaf (one sumfun call)");
        let mixed = heap.cons(make_small(9), one_empty).expect("fits");
        assert_eq!(bytes_and_leaves(mixed, TopLevel::List).unwrap(), (vec![9], 2));
    }

    #[test]
    fn the_part_range_rule_handles_the_reverse_form() {
        // `binary_part(<<1,2,3,4,5>>, 4, -2)` ⇒ `<<3,4>>`: 2 bytes *ending* at 4.
        assert_eq!(
            part_range(5, make_small(4), make_small(-2)).unwrap(),
            (2, 2)
        );
        // The whole binary, the empty range at the end and the empty binary.
        assert_eq!(part_range(5, make_small(0), make_small(5)).unwrap(), (0, 5));
        assert_eq!(part_range(5, make_small(5), make_small(0)).unwrap(), (5, 0));
        assert_eq!(part_range(0, make_small(0), make_small(0)).unwrap(), (0, 0));
        // Out of range: past the end, a negative start, a reversed window, and a
        // non-integer / bignum argument.
        for (size, start, len) in [
            (5, 0, 6),
            (5, -1, 1),
            (5, 1, -2),
            (0, 0, 1),
        ] {
            assert!(
                matches!(part_range(size, make_small(start), make_small(len)), Err(BeamError::BadArg)),
                "part_range({size}, {start}, {len}) must be badarg"
            );
        }
        assert!(matches!(part_range(5, make_small(0), NIL), Err(BeamError::BadArg)));
    }

    #[test]
    fn a_deeply_nested_iolist_is_walked_iteratively() {
        let mut heap = heap();
        // 300 one-element lists around a single byte: past `INLINE_WALK_FRAMES`, so the
        // spill is exercised (and the Rust stack is never used for the nesting).
        let mut term = make_small(7);
        for _ in 0..300 {
            term = heap.cons(term, NIL).expect("fits");
        }
        assert_eq!(bytes(term, TopLevel::List).unwrap(), vec![7]);
    }

    #[test]
    fn the_byte_list_builder_round_trips_through_the_walk() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let list = byte_list(&mut ctx, &[0, 1, 254, 255]).expect("fits");
        assert_eq!(bytes(list, TopLevel::List).unwrap(), vec![0, 1, 254, 255]);
    }
}
