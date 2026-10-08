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

//! The term backing store: a fixed-capacity bump arena.
//!
//! **Minimal and `unsafe`-marked by design.** This is *not* the production
//! allocator — `beam_alloc` (allocators/carriers/process heap) and `beam_gc`
//! (per-process copying GC) build those on top. It exists so the term
//! representation has a concrete, testable backing store. It never reallocates,
//! so handed-out pointers stay valid for the arena's lifetime.

use core::mem::MaybeUninit;

use crate::boxed::try_make_arityval;
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::list::{make_list, CAR_OFFSET, CDR_OFFSET};
use crate::tuple::make_tuple;

/// A fixed-capacity bump term arena.
pub struct Heap {
    storage: Box<[MaybeUninit<Eterm>]>,
    used: usize,
}

impl Heap {
    /// A new arena of `capacity_words` term words.
    pub fn new(capacity_words: usize) -> Heap {
        Heap {
            storage: vec![MaybeUninit::uninit(); capacity_words].into_boxed_slice(),
            used: 0,
        }
    }

    /// Total capacity in words.
    pub fn capacity(&self) -> usize {
        self.storage.len()
    }

    /// Words handed out so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Words still available.
    pub fn available(&self) -> usize {
        self.storage.len() - self.used
    }

    /// Rewind the bump pointer (invalidates all handed-out pointers).
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// The current allocation pointer (`hp`).
    pub fn hp(&self) -> *mut Eterm {
        unsafe { (self.storage.as_ptr() as *mut Eterm).add(self.used) }
    }

    /// One past the last word of the arena (`hend`).
    pub fn hend(&self) -> *mut Eterm {
        unsafe { (self.storage.as_ptr() as *mut Eterm).add(self.storage.len()) }
    }

    /// Allocate `n` contiguous, **uninitialised** words.
    ///
    /// # Safety
    ///
    /// The returned words are uninitialised; the caller must write them before
    /// they are read, and must not use them after this arena is dropped.
    pub unsafe fn alloc_words(&mut self, n: usize) -> TermResult<*mut Eterm> {
        if n > self.available() {
            return Err(TermError::HeapExhausted);
        }
        let p = self.hp();
        self.used += n;
        Ok(p)
    }

    /// Build a cons cell `[car, cdr]` and return the list term.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> TermResult<Eterm> {
        // SAFETY: the two words are fully written before the pointer escapes.
        let p = unsafe { self.alloc_words(2)? };
        unsafe {
            *p.add(CAR_OFFSET) = car;
            *p.add(CDR_OFFSET) = cdr;
        }
        Ok(make_list(p as *const Eterm))
    }

    /// Build a tuple from `elems`.
    pub fn tuple(&mut self, elems: &[Eterm]) -> TermResult<Eterm> {
        // SAFETY: `alloc_tuple` writes the header; we write every element below.
        let p = unsafe { self.alloc_tuple(elems.len())? };
        for (i, elem) in elems.iter().enumerate() {
            unsafe { *p.add(1 + i) = *elem };
        }
        Ok(make_tuple(p))
    }

    /// Allocate a tuple of arity `n`: write the arityval header and return the
    /// header pointer with `n` uninitialised element words following it.
    ///
    /// # Safety
    ///
    /// The `n` element words are uninitialised; the caller must write them before
    /// they are read, and must not use the pointer after this arena is dropped.
    pub unsafe fn alloc_tuple(&mut self, n: usize) -> TermResult<*mut Eterm> {
        let header = try_make_arityval(n)?;
        // SAFETY: the header word is written before the pointer escapes.
        let p = unsafe { self.alloc_words(n + 1)? };
        unsafe { *p = header };
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{is_arity_value, make_arityval};
    use crate::immediate::{make_small, NIL};
    use crate::list::{car, cdr, is_list};
    use crate::tuple::{is_tuple, tuple_element, tuple_size};
    use core::mem::size_of;

    #[test]
    fn cons_builds_a_list() {
        let mut heap = Heap::new(8);
        let list = heap.cons(make_small(1), make_small(2)).unwrap();
        assert!(is_list(list));
        assert_eq!(heap.used(), 2);
        // SAFETY: the list was built by this heap and the heap is still alive.
        unsafe {
            assert_eq!(car(list), make_small(1));
            assert_eq!(cdr(list), make_small(2));
        }
    }

    #[test]
    fn tuple_builds_and_reads_back() {
        let mut heap = Heap::new(8);
        let t = heap
            .tuple(&[make_small(7), make_small(8), make_small(9)])
            .unwrap();
        assert_eq!(heap.used(), 4);
        unsafe {
            assert!(is_tuple(t));
            assert_eq!(tuple_size(t), 3);
            assert_eq!(tuple_element(t, 0), make_small(7));
            assert_eq!(tuple_element(t, 2), make_small(9));
        }
    }

    #[test]
    fn zero_arity_tuple_is_buildable() {
        let mut heap = Heap::new(4);
        let t = heap.tuple(&[]).unwrap();
        assert_eq!(heap.used(), 1);
        unsafe {
            assert!(is_tuple(t));
            assert_eq!(tuple_size(t), 0);
        }
    }

    #[test]
    fn exhaustion_and_recovery() {
        let mut heap = Heap::new(4);
        assert_eq!(heap.capacity(), 4);
        assert_eq!(heap.available(), 4);
        heap.cons(NIL, NIL).unwrap();
        assert_eq!(heap.used(), 2);
        assert_eq!(heap.available(), 2);
        heap.cons(NIL, NIL).unwrap();
        assert_eq!(heap.used(), 4);
        assert_eq!(heap.available(), 0);

        // The arena is now full: both constructors and the raw allocator must fail
        // without consuming words.
        assert_eq!(heap.cons(NIL, NIL).unwrap_err(), TermError::HeapExhausted);
        assert_eq!(heap.used(), 4, "a failed allocation must not consume words");
        assert_eq!(
            unsafe { heap.alloc_words(1) },
            Err(TermError::HeapExhausted)
        );

        heap.reset();
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.available(), 4);
    }

    #[test]
    fn hp_and_hend_bracket_the_arena_and_pointers_are_stable() {
        let mut heap = Heap::new(4);
        let start = heap.hp();
        assert_eq!(heap.hend() as usize - start as usize, 4 * size_of::<Eterm>());

        let a = unsafe { heap.alloc_words(1).unwrap() };
        assert_eq!(a, start);
        assert_eq!(heap.hp() as usize - start as usize, size_of::<Eterm>());

        // The arena never reallocates, so handed-out pointers stay valid.
        let b = unsafe { heap.alloc_words(1).unwrap() };
        assert_eq!(b, unsafe { start.add(1) });
        assert_eq!(unsafe { a.add(1) }, b);
    }

    #[test]
    fn alloc_tuple_writes_the_header() {
        let mut heap = Heap::new(4);
        let p = unsafe { heap.alloc_tuple(3).unwrap() };
        assert_eq!(heap.used(), 4);
        unsafe {
            assert!(is_arity_value(*p));
            assert_eq!(*p, make_arityval(3));
        }
    }
}
