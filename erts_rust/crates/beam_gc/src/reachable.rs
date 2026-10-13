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

//! The read-only **reachability walk** a whole-heap scan is built on: the shared iterative
//! worklist with its one-bit-per-word visited set, and the enumeration
//! ([`for_each_reachable`]) that reports every reachable **boxed object** across the whole
//! per-process heap.
//!
//! Two callers share this walk, so their per-object sizing and child enumeration can never
//! drift apart: the exact-live counter a fullsweep sizes itself from ([`crate::live::live`])
//! and the public reachability enumeration the collectors expose
//! ([`crate::Collector::for_each_reachable_object`] and its siblings on
//! [`crate::GenImmixCollector`] / [`crate::AnyCollector`]). Both drive the `pub(crate)`
//! primitives of [`crate::copy`] — object sizing is
//! [`move_size`](crate::copy::move_size), reference enumeration is
//! [`for_each_ref`](crate::copy::for_each_ref) — over the canonical per-object layout of
//! [`beam_term::layout`], so no shape's size or child slots are re-derived here.
//!
//! Properties the walk has (the contract both callers rely on):
//!
//! * **Read-only.** It writes no forwarding marker, so it can run immediately before a
//!   collection over the very same from-spaces (`Copier` relies on the headers still being
//!   headers).
//! * **Iterative.** The traversal is a worklist over reference words, never the real stack,
//!   so a 50 000-deep list cannot overflow it.
//! * **Each object visited once.** A bitset over the from-space ranges (one bit per word)
//!   records the objects already admitted, so a shared subterm — or a cycle — is visited
//!   exactly once, exactly as the copier's in-place forwarding markers make it.
//! * **Out-of-from-set references skipped.** An immediate, a literal, a foreign space and an
//!   off-heap pointer are all left out, precisely as
//!   [`forward_slot`](crate::copy::forward_slot) classifies
//!   [`Target::Foreign`](crate::copy::Target::Foreign).
//!
//! `unsafe` here is confined to reading the words the walk visits — the same guarantee the
//! copier relies on ([`Target::Live`](crate::copy::Target::Live) dereferences them too) — and
//! each block carries a `// SAFETY:` rationale. This module is crate-internal; the public
//! collector methods that call [`for_each_reachable`] add no `unsafe` of their own and assume
//! no interior layout beyond the canonical shape walk.

use core::mem::size_of;

use beam_common::Result;
use beam_term::prelude::{is_header, primary_tag, ptr_val, Eterm, PrimaryTag};

use crate::copy::{for_each_ref, move_size};
use crate::space::FromSet;

/// Walks the object graph reachable from `roots` out of the evacuated ranges of `from`,
/// calling `on_object(first_word, ptr, total_words)` once per object — see the module docs.
///
/// `first_word` is the object's **first** heap word: a header word for a boxed object, or the
/// `car` of a cons cell (which carries no header). `total_words` is the object's size in words
/// (its [`move_size`](crate::copy::move_size), or `2` for a cons cell). `roots` is borrowed
/// immutably — the walk never rewrites a term — and it never dereferences an address outside
/// `from`'s ranges.
///
/// The only fallible step is the shared [`for_each_ref`](crate::copy::for_each_ref)
/// enumeration, whose signature is fallible for its other callers (a copier's forwarding
/// `visit` can fail); `on_object` itself is infallible, so in practice this never returns an
/// error.
pub(crate) fn walk_reachable<F>(from: &FromSet, roots: &[Eterm], mut on_object: F) -> Result<()>
where
    F: FnMut(Eterm, *const Eterm, usize),
{
    let mut visited = Visited::new(from);
    let mut work: Vec<*mut Eterm> = Vec::new();

    for &root in roots {
        seed(root, &mut visited, &mut work);
    }

    // A worklist, so the traversal depth is bounded by the heap and not by the real stack:
    // a 50 000-cell list is walked without recursing once.
    while let Some(obj) = work.pop() {
        // SAFETY: `obj` was admitted only through `Visited::index`, i.e. only when it
        // addresses a word of one of the caller's from-space ranges — the same guarantee
        // `Copier` relies on before it dereferences a reference word
        // (`Target::Live`/`Target::Forwarded` both read `*ptr`). Its first word is therefore
        // readable.
        let first = unsafe { *obj };
        // A cons cell carries no header word: its first word is the car, a term that never
        // has the `Header` primary tag (`beam_term::tag::is_header`). That frozen shape test
        // is the one `Copier::complete` and the mark-region `scan_young` use.
        let total = if is_header(first) {
            move_size(first, obj)
        } else {
            2
        };
        on_object(first, obj, total);

        // SAFETY: `obj` addresses a live object of exactly `total` words, all readable and
        // none of them written (this walk is read-only), so the canonical child-slot
        // enumeration is covered by this block. `for_each_ref` yields the address of a live
        // reference word inside the object `obj`, so reading it through `*slot` is covered
        // too.
        let walk: Result<()> = unsafe {
            for_each_ref(obj, total, &mut |slot| {
                let term = *slot;
                if matches!(primary_tag(term), PrimaryTag::Boxed | PrimaryTag::List) {
                    admit(ptr_val(term) as *const Eterm, &mut visited, &mut work);
                }
                Ok(())
            })
        };
        walk?;
    }

    Ok(())
}

/// Visits every **boxed object** reachable from `roots` out of the ranges of `from`, each
/// exactly once, passing the object's header word and a pointer to that header.
///
/// A cons cell is *traversed* — so the boxed objects behind it are reached — but is **not**
/// reported: it carries no header word, and every shape a reachability scan filters for is
/// boxed (a fun, a map, an `ErlSubBits`/`BinRef` off-heap handle, …).
///
/// This is deliberately **layout-agnostic**: it reports *which* objects are reachable, never
/// what they contain. The caller applies its own predicate to the reported `(header, ptr)`
/// pairs — e.g. a subtag test that picks out the `ErlSubBits`/`BinRef` handles naming an
/// off-heap payload — and reads whatever payload words its shape defines. This crate assumes
/// no interior layout of its own beyond the canonical shape walk of [`beam_term::layout`], so
/// it needs no knowledge of, and no dependency on, another crate's container interior.
pub(crate) fn for_each_reachable<F>(from: &FromSet, roots: &[Eterm], mut visit: F) -> Result<()>
where
    F: FnMut(Eterm, *const Eterm),
{
    walk_reachable(from, roots, |first, ptr, _total| {
        if is_header(first) {
            visit(first, ptr);
        }
    })
}

/// Admits `ptr` into the worklist when it names a from-space object that has not been
/// visited yet — a reference outside every range (a literal, a foreign space, an off-heap
/// pointer) is skipped, exactly as the copier's forwarding switch leaves its
/// `Target::Foreign` alone.
fn admit(ptr: *const Eterm, visited: &mut Visited, work: &mut Vec<*mut Eterm>) {
    if let Some(index) = visited.index(ptr) {
        if visited.test_and_set(index) {
            work.push(ptr as *mut Eterm);
        }
    }
}

/// Seeds the worklist with `term` when it is a reference into a from-space range.
fn seed(term: Eterm, visited: &mut Visited, work: &mut Vec<*mut Eterm>) {
    if matches!(primary_tag(term), PrimaryTag::Boxed | PrimaryTag::List) {
        admit(ptr_val(term) as *const Eterm, visited, work);
    }
}

/// The visited set: **one bit per word** of every from-space range, so an object is visited
/// exactly once however many references reach it (a shared subterm, a cycle). Keying on the
/// object's first word is the same identity the copier's in-place forwarding marker uses, so
/// the two dedup in lockstep.
struct Visited {
    /// `(lo, hi, first_bit)` per range, `lo`/`hi` in byte addresses and `first_bit` the bit
    /// index the range starts at — the ranges are packed into one bit vector.
    ranges: Vec<(usize, usize, usize)>,
    bits: Vec<u64>,
}

impl Visited {
    /// Indexes the ranges of `from` (`from.ranges()` is byte-addressed; a range added from a
    /// space's live region spans a whole number of term words).
    fn new(from: &FromSet) -> Visited {
        let word = size_of::<Eterm>();
        let mut ranges = Vec::with_capacity(from.ranges().len());
        let mut total = 0usize;
        for &(lo, hi) in from.ranges() {
            ranges.push((lo, hi, total));
            total += (hi - lo) / word;
        }
        Visited {
            ranges,
            bits: vec![0; total.div_ceil(64)],
        }
    }

    /// The bit index of the word `ptr` addresses, or `None` when it lies outside every range.
    fn index(&self, ptr: *const Eterm) -> Option<usize> {
        let addr = ptr as usize;
        self.ranges.iter().find_map(|&(lo, hi, first)| {
            (lo <= addr && addr < hi).then(|| first + (addr - lo) / size_of::<Eterm>())
        })
    }

    /// Sets the bit at `index`, reporting whether it was previously clear (a newly seen
    /// object).
    fn test_and_set(&mut self, index: usize) -> bool {
        let word = index / 64;
        let mask = 1u64 << (index % 64);
        let newly_seen = self.bits[word] & mask == 0;
        self.bits[word] |= mask;
        newly_seen
    }
}
