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

//! Tuples — an arityval header word followed by `arity` element words.
//!
//! A tuple term is `make_boxed` over the header word; `tuple_val` returns that
//! header pointer, and element `i` lives at `tuple_val(x)[1 + i]`.

use std::sync::LazyLock;

use crate::boxed::{
    arityval, boxed_val, is_arity_value, is_boxed_term, make_arityval_unchecked, make_boxed,
};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::literal::set_literal_tag;

/// `make_tuple(x)`.
///
/// Not `const`: pointer-to-integer casts are rejected during const evaluation.
#[inline]
pub fn make_tuple(ptr: *mut Eterm) -> Eterm {
    make_boxed(ptr as *const Eterm)
}

/// `tuple_val(x)` — the header-word pointer of a boxed term.
#[inline]
pub const fn tuple_val(x: Eterm) -> *mut Eterm {
    boxed_val(x)
}

/// `is_tuple(x)`.
///
/// # Safety
///
/// If `x` is boxed, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_tuple(x: Eterm) -> bool {
    if !is_boxed_term(x) {
        return false;
    }
    unsafe { is_arity_value(*boxed_val(x)) }
}

/// `is_not_tuple(x)`.
///
/// # Safety
///
/// As [`is_tuple`].
#[inline]
pub unsafe fn is_not_tuple(x: Eterm) -> bool {
    let tuple = unsafe { is_tuple(x) };
    !tuple
}

/// `is_tuple_arity(x, a)`.
///
/// # Safety
///
/// If `x` is boxed, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_tuple_arity(x: Eterm, arity: usize) -> bool {
    if !is_boxed_term(x) {
        return false;
    }
    unsafe { *boxed_val(x) == make_arityval_unchecked(arity) }
}

/// `tuple_size(x)` — the arity of a tuple.
///
/// # Safety
///
/// `x` must be a live tuple whose header word is valid.
#[inline]
pub unsafe fn tuple_size(x: Eterm) -> usize {
    arityval(unsafe { *boxed_val(x) })
}

/// `tuple_element(x, i)` — element `i` of a tuple (unchecked).
///
/// # Safety
///
/// `x` must be a live tuple and `i` must be `< tuple_size(x)`.
#[inline]
pub unsafe fn tuple_element(x: Eterm, i: usize) -> Eterm {
    unsafe { *tuple_val(x).add(1 + i) }
}

/// Range-checked [`tuple_element`].
///
/// # Safety
///
/// `x` must be a live tuple whose header word is valid.
#[inline]
pub unsafe fn try_tuple_element(x: Eterm, i: usize) -> TermResult<Eterm> {
    if i >= unsafe { tuple_size(x) } {
        return Err(TermError::ArityOutOfRange);
    }
    Ok(unsafe { tuple_element(x, i) })
}

/// The shared empty-tuple literal (`ERTS_GLOBAL_LIT_EMPTY_TUPLE` in C).
///
/// The C global's address is only known at run time, so a `const Eterm` is
/// impossible in Rust (casting a pointer to an integer is rejected during const
/// evaluation). It is therefore a lazily-initialised process-global instead;
/// see [`tuple0`].
static TUPLE0: LazyLock<Eterm> = LazyLock::new(|| {
    let storage: Box<[Eterm; 1]> = Box::new([crate::boxed::make_arityval_zero()]);
    // Leak the storage: a literal lives for the whole process lifetime.
    let storage: &'static mut [Eterm; 1] = Box::leak(storage);
    let ptr = storage.as_mut_ptr();
    let mut term = make_tuple(ptr);
    // SAFETY: `ptr` points at 1 initialised word (an `arityval_zero` header) owned
    // by the leaked allocation, and `term` is a live local. A zero-arity tuple
    // header is transparent, so only the root word gets the literal tag.
    unsafe { set_literal_tag(&mut term, ptr, 1) };
    term
});

/// The shared empty-tuple literal.
///
/// Zero-arity heap tuples are discouraged (the C `TUPLE0` macro notes that the
/// word after the arity word is assumed allocated); use this instead.
#[inline]
pub fn tuple0() -> Eterm {
    *TUPLE0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::make_arityval;
    use crate::immediate::{make_small, NIL};
    use crate::tag::TAG_LITERAL_PTR;

    #[test]
    fn tuple_size_and_elements() {
        let mut words = [NIL; 4];
        words[0] = make_arityval(3);
        words[1] = make_small(10);
        words[2] = make_small(20);
        words[3] = make_small(30);
        let t = make_tuple(words.as_mut_ptr());

        unsafe {
            assert!(is_tuple(t));
            assert!(!is_not_tuple(t));
            assert!(is_tuple_arity(t, 3));
            assert!(!is_tuple_arity(t, 2));
            assert_eq!(tuple_size(t), 3);
            assert_eq!(tuple_element(t, 0), make_small(10));
            assert_eq!(tuple_element(t, 2), make_small(30));
            assert_eq!(try_tuple_element(t, 1).unwrap(), make_small(20));
            assert_eq!(try_tuple_element(t, 3), Err(TermError::ArityOutOfRange));
            assert_eq!(
                try_tuple_element(t, usize::MAX),
                Err(TermError::ArityOutOfRange)
            );
        }
        assert!(!unsafe { is_tuple(make_small(1)) });
        assert!(!unsafe { is_tuple(NIL) });
    }

    #[test]
    fn tuple0_is_a_zero_arity_literal() {
        let t0 = tuple0();
        assert_eq!(t0, tuple0());
        unsafe {
            assert!(is_tuple(t0));
            assert_eq!(tuple_size(t0), 0);
            assert!(is_tuple_arity(t0, 0));
            assert!(try_tuple_element(t0, 0).is_err());
        }
        assert!(crate::literal::is_literal(t0));
        assert_eq!(t0.raw() & TAG_LITERAL_PTR, TAG_LITERAL_PTR);
    }
}
