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

//! Object and key handling — the DB key rules of an ETS object.
//!
//! An ETS *object* is a tuple; its *key* is element `keypos` (1-based) of that
//! tuple. These helpers validate an object and extract its key, mirroring the
//! key handling of `../../erts/emulator/beam/erl_db_util.c`. They also expose the
//! tuple payload slice used by `select` predicates.

use beam_common::Result;
use beam_term::prelude::*;

/// The key of an ETS *object*: element `keypos` (1-based) of the tuple.
///
/// Fails with `badarg` when `object` is not a tuple, when its arity is less than
/// `keypos`, or when `keypos` is `0`.
pub fn key_of(object: Eterm, keypos: usize) -> Result<Eterm> {
    if keypos == 0 {
        return Err(crate::error::badarg());
    }
    // SAFETY: `is_tuple` only dereferences `object`'s header word when it is a
    // boxed word; any live boxed term in the runtime has a valid, aligned header.
    if !unsafe { is_tuple(object) } {
        return Err(crate::error::badarg());
    }
    // SAFETY: `object` is a live tuple (checked above), so its header word holds
    // a valid arity.
    if unsafe { tuple_size(object) } < keypos {
        return Err(crate::error::badarg());
    }
    // SAFETY: `object` is a live tuple and `keypos - 1 < tuple_size(object)` was
    // just established, so the element index is in range.
    Ok(unsafe { tuple_element(object, keypos - 1) })
}

/// Whether `object` is a valid ETS object for `keypos` (non-erroring form of
/// [`key_of`]).
pub fn is_valid_object(object: Eterm, keypos: usize) -> bool {
    key_of(object, keypos).is_ok()
}

/// The element payload slice of the tuple `obj` (the header word excluded),
/// handed to `select` predicates.
///
/// # Safety
///
/// `obj` must be a live tuple term (its boxed header word must be a valid,
/// aligned arity value). The returned slice borrows storage owned by the
/// table (or the original heap) and stays valid only for as long as that
/// storage does — the caller must not outlive it.
pub unsafe fn object_elements<'a>(obj: Eterm) -> &'a [Eterm] {
    // SAFETY: the caller guarantees `obj` is a live tuple, so its header word is a
    // valid arity value.
    let arity = unsafe { tuple_size(obj) };
    // SAFETY: `tuple_val(obj)` points at the header word of the live tuple, so the
    // `arity` element words starting one word later are in the same allocation.
    let elements = unsafe { tuple_val(obj).add(1) };
    // SAFETY: `elements` is non-null and suitably aligned (both the header pointer
    // and the +1 offset are word-aligned), and the `arity` words it points at are
    // initialised and owned by the term storage the caller vouches for. The `'a`
    // lifetime is the caller's responsibility (see `# Safety` above).
    unsafe { core::slice::from_raw_parts(elements, arity) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_term::heap::Heap;

    /// Build a 3-tuple `{10, 20, 30}` on a fresh heap, returning both so the
    /// storage stays alive for the borrow's duration.
    fn tuple3() -> (Heap, Eterm) {
        let mut heap = Heap::new(8);
        let tuple = heap
            .tuple(&[make_small(10), make_small(20), make_small(30)])
            .unwrap();
        (heap, tuple)
    }

    #[test]
    fn key_of_selects_the_keypos_element() {
        let (_heap, tuple) = tuple3();
        assert_eq!(key_of(tuple, 1).unwrap(), make_small(10));
        assert_eq!(key_of(tuple, 2).unwrap(), make_small(20));
        assert_eq!(key_of(tuple, 3).unwrap(), make_small(30));
    }

    #[test]
    fn key_of_rejects_keypos_zero() {
        let (_heap, tuple) = tuple3();
        assert!(matches!(key_of(tuple, 0), Err(BeamError::BadArg)));
    }

    #[test]
    fn key_of_rejects_non_tuples() {
        assert!(matches!(key_of(make_small(1), 1), Err(BeamError::BadArg)));
        assert!(matches!(key_of(NIL, 1), Err(BeamError::BadArg)));
        // A list is not a tuple either.
        let mut heap = Heap::new(4);
        let list = heap.cons(make_small(1), NIL).unwrap();
        assert!(matches!(key_of(list, 1), Err(BeamError::BadArg)));
        let _ = &heap;
    }

    #[test]
    fn key_of_rejects_arity_shorter_than_keypos() {
        let (_heap, tuple) = tuple3();
        assert!(matches!(key_of(tuple, 4), Err(BeamError::BadArg)));
        assert!(matches!(key_of(tuple, usize::MAX), Err(BeamError::BadArg)));
    }

    #[test]
    fn is_valid_object_mirrors_key_of() {
        let (_heap, tuple) = tuple3();
        assert!(is_valid_object(tuple, 1));
        assert!(is_valid_object(tuple, 3));
        assert!(!is_valid_object(tuple, 0));
        assert!(!is_valid_object(tuple, 4));
        assert!(!is_valid_object(make_small(1), 1));
        assert!(!is_valid_object(NIL, 1));
    }

    #[test]
    fn object_elements_returns_the_tuple_payload() {
        let (_heap, tuple) = tuple3();
        // SAFETY: `tuple` is a live tuple owned by `_heap`.
        let elements = unsafe { object_elements(tuple) };
        assert_eq!(elements.len(), 3);
        assert_eq!(elements, &[make_small(10), make_small(20), make_small(30)]);
    }

    #[test]
    fn object_elements_of_a_zero_arity_tuple_is_empty() {
        let mut heap = Heap::new(4);
        let tuple = heap.tuple(&[]).unwrap();
        // SAFETY: `tuple` is a live (empty) tuple owned by `heap`.
        let elements = unsafe { object_elements(tuple) };
        assert!(elements.is_empty());
    }

    #[test]
    fn object_elements_aliases_the_tuple_storage() {
        let (_heap, tuple) = tuple3();
        // SAFETY: `tuple` is a live tuple owned by `_heap`.
        let elements = unsafe { object_elements(tuple) };
        // The slice aliases table/heap storage: its base pointer is the tuple's
        // header word plus one word.
        // SAFETY: `tuple` is a live tuple, so `tuple_val` is valid.
        let header = tuple_val(tuple);
        assert_eq!(elements.as_ptr(), unsafe { header.add(1) } as *const Eterm);
    }
}
