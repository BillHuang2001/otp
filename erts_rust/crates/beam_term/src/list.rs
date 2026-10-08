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

//! Cons cells — the `[CAR, CDR]` pair tagged `TAG_PRIMARY_LIST`.
//!
//! A list term is a *single word* (it must stay so for the NIF/ETF/message ABI):
//! a tagged pointer to a header-less two-word cell. `NIL` (`[]`) is a distinct
//! immediate, so `is_list(NIL)` is **false** — a "list that is not nil" is
//! exactly a cons cell.

use crate::eterm::Eterm;
use crate::tag::{TAG_PRIMARY_LIST, TAG_PRIMARY_MASK, TAG_PTR_MASK};

/// `CAR` word offset inside a cons cell.
pub const CAR_OFFSET: usize = 0;
/// `CDR` word offset inside a cons cell.
pub const CDR_OFFSET: usize = 1;

/// `make_list(x)` — tag a `[CAR, CDR]` cell pointer as a list term.
///
/// Not `const`: pointer-to-integer casts are rejected during const evaluation.
#[inline]
pub fn make_list(ptr: *const Eterm) -> Eterm {
    Eterm::from_raw((ptr as usize) + TAG_PRIMARY_LIST)
}

/// `list_val(x)` — the cell pointer, with the pointer tag (incl. the literal bit) removed.
#[inline]
pub const fn list_val(x: Eterm) -> *mut Eterm {
    ptr_val(x)
}

/// `ptr_val(x)` — the generic tagged pointer behind a BOXED or LIST word.
#[inline]
pub const fn ptr_val(x: Eterm) -> *mut Eterm {
    (x.raw() & !TAG_PTR_MASK) as *mut Eterm
}

/// `is_not_list(x)`.
///
/// This is the C bit trick `(x) & (_TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)` — that
/// is `x & 0x2`: true unless the primary tag is `LIST` (or the value-invalid
/// `HEADER` pattern). Mirrored verbatim for bit-exactness.
#[inline]
pub const fn is_not_list(x: Eterm) -> bool {
    (x.raw() & (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) != 0
}

/// `is_list(x)` — true for a cons cell (and **false** for `NIL`).
#[inline]
pub const fn is_list(x: Eterm) -> bool {
    !is_not_list(x)
}

/// `is_not_nil_list(x)` — same as [`is_list`]: `NIL` is an immediate, not a LIST word.
#[inline]
pub const fn is_not_nil_list(x: Eterm) -> bool {
    is_list(x)
}

/// `CAR(x)` — the head word of a cons cell.
///
/// # Safety
///
/// `x` must be a non-`NIL` list term (a `TAG_PRIMARY_LIST` cons cell) whose cell
/// is live and 2 words long.
#[inline]
pub unsafe fn car(x: Eterm) -> Eterm {
    unsafe { *list_val(x).add(CAR_OFFSET) }
}

/// `CDR(x)` — the tail word of a cons cell.
///
/// # Safety
///
/// As [`car`]: `x` must be a live non-`NIL` list term.
#[inline]
pub unsafe fn cdr(x: Eterm) -> Eterm {
    unsafe { *list_val(x).add(CDR_OFFSET) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::immediate::{make_small, NIL};
    use crate::tag::TAG_LITERAL_PTR;

    #[test]
    fn cons_cell_construction_and_access() {
        let cell = [make_small(1), make_small(2)];
        let list = make_list(cell.as_ptr());
        assert!(is_list(list));
        assert!(is_not_nil_list(list));
        assert!(!is_list(NIL));
        assert_eq!(list.raw() & TAG_PRIMARY_MASK, TAG_PRIMARY_LIST);
        // SAFETY: `cell` is a live 2-word cons cell for the duration of the test.
        unsafe {
            assert_eq!(car(list), make_small(1));
            assert_eq!(cdr(list), make_small(2));
            assert_eq!(*list_val(list).add(CDR_OFFSET), make_small(2));
        }
        assert_eq!(cell[0], make_small(1));
    }

    #[test]
    fn ptr_val_masks_pointer_and_literal_tags() {
        let mut cell = [make_small(7), NIL];
        let base = cell.as_mut_ptr() as usize;
        let plain = make_list(cell.as_ptr());
        let literal = Eterm::from_raw(plain.raw() | TAG_LITERAL_PTR);
        assert_eq!(list_val(plain) as usize, base);
        assert_eq!(list_val(literal) as usize, base);
        assert_eq!(ptr_val(literal) as usize, base);
    }

    #[test]
    fn is_list_matches_only_list_and_header_patterns() {
        // Values are never bare headers, so this is "tag == LIST" in practice.
        assert!(is_list(Eterm::from_raw(0x1)));
        assert!(!is_list(Eterm::from_raw(0x2)));
        assert!(!is_list(Eterm::from_raw(0x3)));
        assert!(!is_list(NIL));
    }
}
