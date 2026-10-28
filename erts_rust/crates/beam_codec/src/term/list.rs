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
//! The **bulk list writer**: one heap reservation for a whole cons spine.
//!
//! `beam_term` owns the frozen cons layout (a header-less `[CAR, CDR]` cell, a
//! `TAG_PRIMARY_LIST` pointer, `beam_term::list`); this module is the codec's one
//! consumer of it on the *decode* side. A wire `STRING_EXT`/`LIST_EXT` arm that
//! called [`beam_alloc::Heap::cons`] once per element paid one bump reservation per
//! cons cell — this carves the whole spine in a single [`Heap::alloc_words`] call
//! instead, which is what the C's `dec_term` does (`hp += 2` per cell, with the
//! block's size computed up front).
//!
//! Both public entry points share [`write_cons_chain`], so the cell shape and the
//! forward linkage exist once.
use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{make_list, make_small, Eterm, NIL};
use crate::error;

/// Writes the cons spine `n` cells long at `hp` and returns its head term.
///
/// Cell `i` lives at `hp + 2 * i`; its `CDR` is a list term over cell `i + 1` (or
/// `tail` for the last cell) and its `CAR` is `head(i)`. The caller has reserved
/// exactly `2 * n` words, so every cell is in bounds, and every word written is
/// initialised before the term escapes.
fn write_cons_chain(
    hp: *mut Eterm,
    n: usize,
    tail: Eterm,
    mut head: impl FnMut(usize) -> Eterm,
) -> Eterm {
    for i in 0..n {
        // SAFETY: the caller reserved `2 * n` words at `hp`; cell `i` occupies words
        // `2 * i` and `2 * i + 1`, both inside that region, and both are written
        // before the returned term can be read.
        unsafe {
            let cell = hp.add(2 * i);
            *cell = head(i);
            *cell.add(1) = if i + 1 < n {
                make_list(hp.add(2 * (i + 1)) as *const Eterm)
            } else {
                tail
            };
        }
    }
    make_list(hp as *const Eterm)
}

/// The number of words a list of `n` elements occupies (`n` two-word cells).
///
/// A `None` means the multiplication overflowed `usize` — a count no real list can
/// have — and is reported as [`BeamError::SystemLimit`](beam_common::BeamError) by
/// the callers rather than wrapping into a short reservation.
fn spine_words(n: usize) -> Result<usize> {
    n.checked_mul(2).ok_or_else(error::system_limit)
}

/// Builds the proper list of the byte values `bytes` and returns it, in **one**
/// heap reservation.
///
/// This is the `STRING_EXT` decoder's constructor: the wire form is a byte count
/// followed by that many bytes, and the term is the same cons spine
/// `[b0, [b1, … [bn-1, NIL]]]` that a per-byte `heap.cons` loop would build, at one
/// reservation instead of `n`.
///
/// # Errors
///
/// [`BeamError::Alloc`](beam_common::BeamError) when the heap cannot fit `2 * n`
/// words, and `SystemLimit` for a byte count whose word size overflows `usize`.
pub fn write_byte_list(heap: &mut Heap, bytes: &[u8]) -> Result<Eterm> {
    let n = bytes.len();
    if n == 0 {
        return Ok(NIL);
    }
    let words = spine_words(n)?;
    // SAFETY: `alloc_words` returned `words == 2 * n` exclusively-owned, writable
    // words, the exact region `write_cons_chain` requires.
    let hp = unsafe { heap.alloc_words(words)? };
    Ok(write_cons_chain(hp, n, NIL, |i| make_small(i64::from(bytes[i]))))
}

/// Builds the proper list `[elems… | tail]` and returns it, in **one** heap
/// reservation.
///
/// This is the `LIST_EXT` decoder's constructor (its `tail` is the wire's next
/// term: `NIL` for a proper list, the improper tail otherwise). The result is the
/// same cons spine a per-element `heap.cons` loop builds — one reservation instead
/// of `elems.len()`.
///
/// # Errors
///
/// [`BeamError::Alloc`](beam_common::BeamError) when the heap cannot fit
/// `2 * elems.len()` words, and `SystemLimit` for a count whose word size overflows
/// `usize`.
pub fn write_list(heap: &mut Heap, elems: &[Eterm], tail: Eterm) -> Result<Eterm> {
    let n = elems.len();
    if n == 0 {
        return Ok(tail);
    }
    let words = spine_words(n)?;
    // SAFETY: `alloc_words` returned `words == 2 * n` exclusively-owned, writable
    // words, the exact region `write_cons_chain` requires.
    let hp = unsafe { heap.alloc_words(words)? };
    Ok(write_cons_chain(hp, n, tail, |i| elems[i]))
}

#[cfg(test)]
mod tests {
    //! The bulk writer must build exactly the spine a per-cell `heap.cons` loop
    //! builds — same cells, same linkage, same terms — including the improper tail
    //! and the empty cases.
    use super::*;
    use crate::term::{cons_head, cons_tail};
    use beam_common::Result;
    use beam_term::prelude::{is_list, is_nil};

    fn heap() -> Heap {
        Heap::new(64).expect("a tiny heap")
    }

    fn read_back(term: Eterm) -> (Vec<Eterm>, Eterm) {
        let mut items = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            items.push(cons_head(cursor).expect("a cons head"));
            cursor = cons_tail(cursor).expect("a cons tail");
        }
        (items, cursor)
    }

    #[test]
    fn an_empty_byte_list_is_nil() {
        let mut heap = heap();
        assert!(is_nil(write_byte_list(&mut heap, &[]).unwrap()));
        assert_eq!(heap.used(), 0, "nothing is reserved for an empty list");
    }

    #[test]
    fn a_byte_list_reads_back_in_order() {
        let mut heap = heap();
        let bytes = [0u8, 1, 127, 255];
        let term = write_byte_list(&mut heap, &bytes).unwrap();
        let (items, tail) = read_back(term);
        let values: Vec<i64> = items
            .iter()
            .map(|t| beam_term::prelude::signed_value(*t))
            .collect();
        assert_eq!(values, vec![0, 1, 127, 255]);
        assert!(is_nil(tail));
        assert_eq!(heap.used(), 2 * bytes.len(), "one reservation for the whole spine");
    }

    #[test]
    fn a_list_keeps_its_improper_tail() {
        let mut heap = heap();
        let elems = [make_small(1), make_small(2), make_small(3)];
        let tail = make_small(99);
        let term = write_list(&mut heap, &elems, tail).unwrap();
        let (items, read_tail) = read_back(term);
        assert_eq!(items, elems);
        assert_eq!(read_tail, tail);
        assert_eq!(heap.used(), 2 * elems.len());
    }

    #[test]
    fn an_empty_list_is_its_tail() {
        let mut heap = heap();
        let tail = make_small(7);
        assert_eq!(write_list(&mut heap, &[], tail).unwrap(), tail);
        assert_eq!(heap.used(), 0);
    }

    #[test]
    fn a_list_that_does_not_fit_is_a_typed_allocation_error() {
        let mut heap = Heap::new(4).expect("four words");
        let elems = [make_small(1), make_small(2), make_small(3)];
        let outcome: Result<Eterm> = write_list(&mut heap, &elems, NIL);
        assert!(matches!(outcome, Err(beam_common::BeamError::Alloc(_))));
    }
}
