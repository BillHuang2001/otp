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

//! Safe wrappers over the **M1 internal-reference thing**
//! ([`beam_term::boxed::write_internal_ref`] / [`beam_term::boxed::internal_ref_id`]).
//!
//! The frozen interior layout of an internal reference is owned by
//! [`beam_term::boxed`] (workspace `CONTEXT.md` §2) — this module only adds the
//! allocation and the `TermError` → [`beam_common::BeamError`] mapping the safe
//! [`encode`](crate::encode)/[`decode`](crate::decode) layers need (they cannot call
//! the `unsafe` layout functions directly, and the whole crate is
//! `#![deny(unsafe_code)]` outside this subtree).
//!
//! Everything here is the *term* half of an internal reference; the *wire* mapping
//! (the three 32-bit numbers of a `NEWER_REFERENCE_EXT` image) lives in
//! [`crate::ref_wire`].

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::boxed::INTERNAL_REF_WORDS;
use beam_term::prelude::{Eterm, MAX_SMALL};

use crate::error;

/// Reads the numeric id of an **M1 internal-reference thing**.
///
/// Delegates to [`beam_term::boxed::internal_ref_id`]. A term that is not a
/// well-formed M1 reference — a non-term, a boxed term of another kind, or a
/// *C-layout* `ErtsORefThing` block (arity 2, whose id word does not exist) — yields
/// a typed [`BeamError`](beam_common::BeamError) via the upstream
/// `From<TermError> for BeamError` mapping (never a wrong id).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not an M1
/// internal reference.
pub fn internal_ref_id(term: Eterm) -> Result<u64> {
    // SAFETY: `internal_ref_id` validates the header word (and its arity) before
    // reading the id word, so it is sound for any `Eterm`; the caller invariant is
    // only that `term` is a live word, which is true of every `Eterm`.
    Ok(unsafe { beam_term::boxed::internal_ref_id(term) }?)
}

/// Materializes an **M1 internal-reference thing** carrying `id` on `heap`.
///
/// Allocates [`INTERNAL_REF_WORDS`] words and writes
/// `[make_header(3, REF_SUBTAG), node = 0, serial = 0, id]` via
/// [`beam_term::boxed::write_internal_ref`] — the same 4-word block the runtime's
/// `make_ref/0` producers write, so the rebuilt reference hashes/compares/encodes
/// identically.
///
/// # Errors
///
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when `id` does not
/// fit the 60-bit small the fixed id slot holds (checked *before* allocating, so no
/// words are wasted on a rejected id), and [`BeamError::Alloc`] on heap exhaustion.
pub fn make_internal_ref(heap: &mut Heap, id: u64) -> Result<Eterm> {
    // The id lives in a single small-integer word; a value past `MAX_SMALL` cannot be
    // stored (the C-layout `num[]` has no such limit, but the M1 block does). Refuse
    // before allocating rather than writing a truncated/substituted id.
    if id > MAX_SMALL as u64 {
        return Err(error::system_limit());
    }
    // SAFETY: `alloc_words` returned `INTERNAL_REF_WORDS` exclusively-owned, writable
    // words.
    let hp = unsafe { heap.alloc_words(INTERNAL_REF_WORDS)? };
    // SAFETY: `hp` owns exactly `INTERNAL_REF_WORDS` words, which `write_internal_ref`
    // fully initialises; `id <= MAX_SMALL` is its documented precondition.
    Ok(unsafe { beam_term::boxed::write_internal_ref(hp, id) }?)
}

#[cfg(test)]
mod tests {
    //! The wrappers round-trip an id through the M1 block, refuse a non-reference and
    //! an out-of-range id, and agree with the wire mapping (the encode/decode seam).

    use super::*;
    use beam_term::TermError;
    use beam_term::prelude::make_small;

    fn heap() -> Heap {
        Heap::new(1 << 12).expect("the carrier allocator never fails for a small heap")
    }

    #[test]
    fn round_trips_an_id_through_the_block() {
        let mut heap = heap();
        for id in [0u64, 1, 42, 1 << 18, 1 << 40, 1 << 45, (1u64 << 59) - 1] {
            let term = make_internal_ref(&mut heap, id).expect("id fits a small");
            assert_eq!(internal_ref_id(term).unwrap(), id, "id = {id}");
            // The block is exactly the frozen M1 shape.
            assert_eq!(INTERNAL_REF_WORDS, 4);
        }
    }

    #[test]
    fn a_non_reference_is_a_typed_error() {
        assert!(matches!(
            internal_ref_id(make_small(5)),
            Err(beam_common::BeamError::BadArg)
        ));
        // A fun/export-ish boxed term is not a reference either.
        assert!(matches!(
            internal_ref_id(make_small(0)),
            Err(beam_common::BeamError::BadArg)
        ));
    }

    #[test]
    fn an_id_past_the_small_range_is_refused() {
        let mut heap = heap();
        assert!(matches!(
            make_internal_ref(&mut heap, MAX_SMALL as u64 + 1),
            Err(beam_common::BeamError::SystemLimit)
        ));
        // The largest representable id still works.
        assert!(make_internal_ref(&mut heap, MAX_SMALL as u64).is_ok());
        // …and the term error the writer would raise is mapped (sanity check of the
        // upstream conversion, not exercised through `make_internal_ref` above).
        assert!(matches!(
            beam_common::BeamError::from(TermError::SmallIntOutOfRange),
            beam_common::BeamError::BadArith
        ));
    }
}
