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

//! The **sharing-preserving** copier: `copy_struct_shared`.
//!
//! Ground truth: the C sharing-preserving copier `copy_shared_calculate` +
//! `copy_shared_perform_x` (`copy.c:1111-1460`), whose state is `erts_shcopy_t`
//! (`global.h:1061-1081`) and whose `copy_shared_perform` macro (`global.h:1147`) consults it.
//! The C keys sharing on the **from-space term address**, keeping a `SHTABLE` plus destructive
//! in-place markers in the source. Rust needs no markers: this copier keys the same identity in a
//! real [`HashMap`], so the source heap is left untouched.
//!
//! It copies *exactly* like [`copy_struct`](crate::copy_struct) — same sharing policy for
//! immediates and literals, same treatment of off-heap MSO containers, refc binaries and external
//! nodes — but materialises each compound subterm **once per source identity**: two occurrences of
//! the same cons cell / tuple / boxed address become the *same* copy, and an off-heap reference is
//! refcounted **once** per identity. It reuses `copy.rs`'s [`shallow`] primitive (no copy logic is
//! duplicated) and drives the same explicit worklist, so it is iterative and never recurses on the
//! native stack.

use std::collections::HashMap;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use crate::copy::{identity, is_shared, preflight_words, shallow_unshared, CopyContext, Fixup};
use crate::error;

/// Copies `term`'s graph into `to`, returning the copied root, **sharing repeated subterms**.
///
/// Immediates, literal-area terms and the zero-arity tuple are shared exactly as in
/// [`copy_struct`](crate::copy_struct). Every other compound subterm is copied at most once: the
/// from-space address records the already-materialised copy, and a second occurrence (in the
/// source graph) is written as that same copy. An off-heap MSO object or refc binary referenced by
/// several source terms is therefore refcounted once per source identity, not once per occurrence.
///
/// The copy is always **structurally equal** to the original (for the classes
/// [`beam_term::compare`] compares structurally); only the amount of sharing differs.
pub fn copy_struct_shared(term: Eterm, to: &mut Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm> {
    // The flat word count is a safe upper bound on the shared copy's size, so the same preflight
    // as `copy_struct` applies: the common exhaustion is attributed to the message heap
    // (`AllocKind::Message`) up front rather than surfacing mid-copy as a bare heap failure — and a
    // forged cyclic graph is a typed error (via `preflight_words`), never an unbounded walk.
    let needed = preflight_words(term, to.available(), ctx)?;
    if to.available() < needed {
        return Err(error::heap_exhausted(needed));
    }

    // The one-copy-per-source-identity memo (the `SHTABLE` of the C copier, keyed on the
    // from-space term address) and the fix-up worklist shared with `copy_struct`'s loop.
    let mut memo: HashMap<usize, Eterm> = HashMap::new();
    let mut work: Vec<Fixup> = Vec::new();

    let root = materialize(term, to, ctx, &mut memo, &mut work)?;
    while let Some((slot, src)) = work.pop() {
        let copied = materialize(src, to, ctx, &mut memo, &mut work)?;
        // SAFETY: `slot` was recorded by `shallow` as a reference word inside an object it just
        // materialised on the destination heap. That heap never reallocates, so the word is still
        // live, exclusively owned by this copy and writable.
        unsafe { *slot = copied };
    }
    Ok(root)
}

/// Returns the (single) copy of `src`, materialising it when it has not been seen before.
///
/// A shared term (immediate/literal) is returned unchanged and never memoised; a compound term is
/// keyed by its from-space address, so the second and later occurrences resolve to the copy made
/// for the first.
fn materialize(
    src: Eterm,
    to: &mut Heap,
    ctx: &mut CopyContext<'_>,
    memo: &mut HashMap<usize, Eterm>,
    work: &mut Vec<Fixup>,
) -> Result<Eterm> {
    if is_shared(src) {
        return Ok(src);
    }
    let Some(key) = identity(src) else {
        // Unreachable for a non-shared word, which is by construction a cons or boxed term.
        return shallow_unshared(to, ctx, src, work);
    };
    if let Some(&copied) = memo.get(&key) {
        return Ok(copied);
    }
    let copied = shallow_unshared(to, ctx, src, work)?;
    memo.insert(key, copied);
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use beam_alloc::Heap;
    use beam_term::prelude::*;

    use super::copy_struct_shared;
    use crate::copy::{copy_struct, size_of_copy, CopyContext};
    use crate::off_heap::OffHeapTable;

    /// Copies `term` flat into `flat_dst` and shared into `shared_dst`, returning both copies.
    ///
    /// Both destination heaps are borrowed from the caller so the returned copies stay valid: a
    /// `Heap` frees its backing block when it is dropped, so a helper that owned the heaps would
    /// hand back dangling pointers.
    fn copy_both(term: Eterm, flat_dst: &mut Heap, shared_dst: &mut Heap) -> (Eterm, Eterm) {
        let src_off = OffHeapTable::new();

        let flat = {
            let mut dst_off = OffHeapTable::new();
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct(term, flat_dst, &mut ctx).expect("the flat copy succeeds")
        };
        let shared = {
            let mut dst_off = OffHeapTable::new();
            let mut ctx = CopyContext::new(&src_off, &mut dst_off);
            copy_struct_shared(term, shared_dst, &mut ctx).expect("the shared copy succeeds")
        };
        (flat, shared)
    }

    #[test]
    fn a_repeated_tuple_is_materialised_once() {
        let mut src = Heap::new(64).expect("source heap");
        let tuple = src.tuple(&[make_small(1), make_small(2)]).expect("the tuple fits");
        let list = {
            let mut acc = NIL;
            for _ in 0..4 {
                acc = src.cons(tuple, acc).expect("each cons fits");
            }
            acc
        };

        let mut flat_dst = Heap::new(128).expect("flat destination heap");
        let mut shared_dst = Heap::new(128).expect("shared destination heap");
        let (flat, shared) = copy_both(list, &mut flat_dst, &mut shared_dst);

        assert_eq!(flat_dst.used(), size_of_copy(list));
        assert_eq!(flat_dst.used(), 4 * 2 + 4 * 3, "four cons cells and four tuples");
        assert_eq!(shared_dst.used(), 4 * 2 + 3, "four cons cells and ONE tuple");

        // Every copied occurrence resolves to the same destination tuple address.
        let mut current = shared;
        let mut addresses = Vec::new();
        while is_list(current) {
            // SAFETY: the car of a live cons cell.
            let head = unsafe { car(current) };
            addresses.push(head.raw());
            // SAFETY: the cdr of a live cons cell.
            current = unsafe { cdr(current) };
        }
        assert_eq!(addresses.len(), 4);
        assert!(
            addresses.windows(2).all(|w| w[0] == w[1]),
            "the four occurrences share one address"
        );
        assert_ne!(addresses[0], tuple.raw(), "the shared copy is a real copy");

        let atoms = AtomTable::new();
        assert!(beam_term::compare::eq(list, flat, &atoms));
        assert!(beam_term::compare::eq(list, shared, &atoms));
    }

    #[test]
    fn distinct_subterms_copy_to_the_flat_size() {
        let mut src = Heap::new(64).expect("source heap");
        let mut pairs = Vec::new();
        for i in 0..4 {
            pairs.push(src.tuple(&[make_small(i), make_small(i + 10)]).expect("fits"));
        }
        let list = {
            let mut acc = NIL;
            for &pair in &pairs {
                acc = src.cons(pair, acc).expect("each cons fits");
            }
            acc
        };

        let mut flat_dst = Heap::new(128).expect("flat destination heap");
        let mut shared_dst = Heap::new(128).expect("shared destination heap");
        let (flat, shared) = copy_both(list, &mut flat_dst, &mut shared_dst);

        assert_eq!(
            shared_dst.used(),
            flat_dst.used(),
            "a graph with no sharing copies to exactly the flat size"
        );
        assert_eq!(shared_dst.used(), size_of_copy(list));
        assert!(beam_term::compare::eq(list, flat, &AtomTable::new()));
        assert!(beam_term::compare::eq(list, shared, &AtomTable::new()));
    }

    #[test]
    fn an_immediate_heavy_graph_is_unchanged() {
        let mut src = Heap::new(16).expect("source heap");
        let tail = src.cons(make_small(2), NIL).expect("the tail fits");
        let list = src.cons(make_small(1), tail).expect("the head fits");

        let mut flat_dst = Heap::new(32).expect("flat destination heap");
        let mut shared_dst = Heap::new(32).expect("shared destination heap");
        let _ = copy_both(list, &mut flat_dst, &mut shared_dst);

        assert_eq!(flat_dst.used(), 4);
        assert_eq!(
            shared_dst.used(),
            flat_dst.used(),
            "immediates are shared in both copies"
        );
    }
}
