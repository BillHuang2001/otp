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
//! ([`for_each_reachable`] / [`for_each_reachable_view`]) that reports every reachable
//! **boxed object** across the whole per-process heap.
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
//! [`for_each_reachable`] hands its visitor a `(header, ptr)` **raw-pointer** pair; the
//! [`for_each_reachable_view`] sibling hands it a safe, read-only [`ReachableObject`] view of
//! the very same object instead, so a host that only wants to *read* an object's words needs
//! no `unsafe` of its own. The two are one walk: the raw form is expressed in terms of the view
//! form, so they can never disagree about which objects are reachable.
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
        seed(root, from, &mut visited, &mut work);
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
                    admit(ptr_val(term) as *const Eterm, from, &mut visited, &mut work);
                }
                Ok(())
            })
        };
        walk?;
    }

    Ok(())
}

/// A **read-only view of one reachable boxed object** — the safe form of the `(header, ptr)`
/// pair the raw reachability walk reports.
///
/// [`words`](Self::words)`()[0]` is the object's header word and the payload words follow it;
/// [`word`](Self::word) reads payload word `index` with **no `unsafe` at the call site**, so a
/// host that inspects a reported object (e.g. a subtag test that reads an off-heap handle's
/// payload pointer word) needs no raw-pointer arithmetic of its own.
///
/// The view borrows the underlying heap for the duration of the visit only: a visitor is
/// `FnMut(ReachableObject<'_>)` (higher-ranked over the borrow), so it **cannot retain** the
/// slice past the callback — the heap may be collected as soon as the walk returns.
///
/// The view is equally **layout-agnostic**: it reports the object's words, never their
/// meaning, so this crate assumes no interior layout beyond the canonical shape walk of
/// [`beam_term::layout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReachableObject<'a> {
    /// The object's whole word run: `[header, w1, w2, …]`.
    words: &'a [Eterm],
}

impl<'a> ReachableObject<'a> {
    /// The view of an object whose whole word run is `words` (`words[0]` is the header).
    ///
    /// Private: the only producer is [`for_each_reachable_view`], which knows the run heads a
    /// live boxed object of exactly that length.
    fn new(words: &'a [Eterm]) -> Self {
        ReachableObject { words }
    }

    /// The object's **header word** — `words()[0]`.
    pub fn header(&self) -> Eterm {
        self.words[0]
    }

    /// The object's **whole word run**: `[header, w1, w2, …]`.
    pub fn words(&self) -> &'a [Eterm] {
        self.words
    }

    /// Payload word `index` (`0` is the header), or `None` when `index` is out of range.
    pub fn word(&self, index: usize) -> Option<Eterm> {
        self.words.get(index).copied()
    }

    /// The object's size in words, header included.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether the object has no words — never true for a reported object, which always
    /// carries at least a header word; present because [`len`](Self::len) exists.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The **address of the header word** — the object's identity, the same value the raw
    /// form reports as its `ptr`.
    pub fn address(&self) -> usize {
        self.words.as_ptr() as usize
    }
}

/// Visits every **boxed object** reachable from `roots` out of the ranges of `from`, each
/// exactly once, passing a safe read-only [`ReachableObject`] view of it.
///
/// This is [`for_each_reachable`]'s safe sibling: the same walk, the same dedup, the same
/// objects in the same order, but the visitor reads the object's words through
/// [`ReachableObject`] instead of a raw pointer, so an inspection-only caller needs no
/// `unsafe`. A cons cell is *traversed* (so the boxed objects behind it are reached) but is
/// **not** reported — it carries no header word.
pub(crate) fn for_each_reachable_view<F>(
    from: &FromSet,
    roots: &[Eterm],
    mut visit: F,
) -> Result<()>
where
    F: FnMut(ReachableObject<'_>),
{
    walk_reachable(from, roots, |first, ptr, total| {
        if is_header(first) {
            // SAFETY: `ptr` heads a live boxed object of exactly `total` words — the walk
            // admitted it only through `Visited::index`, i.e. only when it addresses a word of
            // one of the caller's from-space ranges, and `total` is that object's
            // `move_size`. Those `total` words are all readable and none is written (the walk
            // is read-only), so they form a valid `&[Eterm]` for the duration of the visit.
            let slice = unsafe { core::slice::from_raw_parts(ptr, total) };
            visit(ReachableObject::new(slice));
        }
    })
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
///
/// For a visitor that need not touch raw pointers, prefer [`for_each_reachable_view`], which
/// reports the same objects as a safe [`ReachableObject`].
pub(crate) fn for_each_reachable<F>(from: &FromSet, roots: &[Eterm], mut visit: F) -> Result<()>
where
    F: FnMut(Eterm, *const Eterm),
{
    for_each_reachable_view(from, roots, |obj| visit(obj.header(), obj.words().as_ptr()))
}

/// Admits `ptr` into the worklist when it names a from-space object that has not been
/// visited yet — a reference outside every range (a literal, a foreign space, an off-heap
/// pointer) is skipped, exactly as the copier's forwarding switch leaves its
/// `Target::Foreign` alone.
fn admit(ptr: *const Eterm, from: &FromSet, visited: &mut Visited, work: &mut Vec<*mut Eterm>) {
    if let Some(index) = visited.index(ptr, from) {
        if visited.test_and_set(index) {
            work.push(ptr as *mut Eterm);
        }
    }
}

/// Seeds the worklist with `term` when it is a reference into a from-space range.
fn seed(term: Eterm, from: &FromSet, visited: &mut Visited, work: &mut Vec<*mut Eterm>) {
    if matches!(primary_tag(term), PrimaryTag::Boxed | PrimaryTag::List) {
        admit(ptr_val(term) as *const Eterm, from, visited, work);
    }
}

/// The visited set: **one bit per word** of every from-space range, so an object is visited
/// exactly once however many references reach it (a shared subterm, a cycle). Keying on the
/// object's first word is the same identity the copier's in-place forwarding marker uses, so
/// the two dedup in lockstep.
struct Visited {
    /// `first[k]` is the bit index range `from.ranges()[k]` starts at — the ranges (in
    /// [`FromSet::ranges`]'s order) are packed into one bit vector, so a range's `lo`/`hi` are
    /// read from the from-set itself and never copied here.
    first: Vec<usize>,
    bits: Vec<u64>,
}

impl Visited {
    /// Indexes the ranges of `from` (`from.ranges()` is byte-addressed; a range added from a
    /// space's live region spans a whole number of term words).
    fn new(from: &FromSet) -> Visited {
        let word = size_of::<Eterm>();
        let mut first = Vec::with_capacity(from.ranges().len());
        let mut total = 0usize;
        for &(lo, hi) in from.ranges() {
            first.push(total);
            total += (hi - lo) / word;
        }
        Visited {
            first,
            bits: vec![0; total.div_ceil(64)],
        }
    }

    /// The bit index of the word `ptr` addresses, or `None` when it lies outside every range.
    ///
    /// The range is resolved by [`FromSet::locate`] — the one place the range geometry is
    /// interpreted — so the walk's dedup admits exactly the addresses the copier's
    /// [`FromSet::contains`] classifies as from-space, in `O(log #ranges)` (this runs per
    /// reference word).
    fn index(&self, ptr: *const Eterm, from: &FromSet) -> Option<usize> {
        let addr = ptr as usize;
        let range = from.locate(addr)?;
        let lo = from.ranges()[range].0;
        debug_assert!(addr >= lo, "a located range starts at or below the address");
        Some(self.first[range] + (addr - lo) / size_of::<Eterm>())
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

#[cfg(test)]
mod tests {
    //! Unit tests for the walk's visited set: the bit index a range's word resolves to, which is
    //! the one per-reference-word computation of the traversal. It must partition the from-set's
    //! ranges **exactly** (no two addresses sharing a bit, no address without one), because a
    //! shared bit would silently drop an object a collection keeps alive and a missing bit would
    //! visit one twice.

    use super::*;
    use beam_term::prelude::make_small;

    /// Ranges the test indexes — far more than a hand-written case would build, so the bit index
    /// is exercised across many ranges and many bit-vector words.
    const RANGES: usize = 1200;

    #[test]
    fn the_visited_bit_index_partitions_the_ranges_exactly() {
        let word = size_of::<Eterm>();

        // `RANGES` disjoint ranges of 1..=4 words, each separated from the next by a one-word
        // hole (so the holes are addresses the walk must refuse).
        let mut from = FromSet::new();
        let mut ranges = Vec::with_capacity(RANGES);
        let mut next = 0x10_000usize;
        for i in 0..RANGES {
            let lo = next;
            let hi = lo + (1 + i % 4) * word;
            from.add_range(lo, hi);
            ranges.push((lo, hi));
            next = hi + word;
        }

        let visited = Visited::new(&from);
        assert_eq!(visited.first.len(), RANGES, "one bit offset per range");
        let total_words: usize = ranges.iter().map(|&(lo, hi)| (hi - lo) / word).sum();
        assert_eq!(
            visited.bits.len(),
            total_words.div_ceil(64),
            "the bit vector is exactly as long as the ranges need"
        );

        // Every word of every range resolves to its own bit — the offsets are the ranges' prefix
        // sums, so no two words share a bit and the last word's bit is the vector's last bit.
        let mut first = 0usize;
        let mut seen = std::collections::BTreeSet::new();
        for &(lo, hi) in &ranges {
            let mut addr = lo;
            let mut offset = 0usize;
            while addr < hi {
                let index = visited
                    .index(addr as *const Eterm, &from)
                    .unwrap_or_else(|| panic!("{addr:#x} is inside [{lo:#x}, {hi:#x})"));
                assert_eq!(index, first + offset, "{addr:#x} resolves to its own bit");
                assert!(seen.insert(index), "the bit {index} was not used yet");
                addr += word;
                offset += 1;
            }
            first += (hi - lo) / word;
        }
        assert_eq!(seen.len(), total_words, "exactly one bit per covered word");

        // An address in a hole between ranges, at a range's exclusive `hi`, below the first range
        // and a foreign address all resolve to no bit — the walk skips them, exactly as the
        // copier leaves a `Target::Foreign` alone.
        let mut outside = Vec::new();
        for &(lo, hi) in &ranges {
            outside.push(lo - word); // the hole before the range
            outside.push(hi); // the range's exclusive end
        }
        outside.push(0);
        outside.push(0x10_000usize - word);
        let local: Eterm = make_small(1);
        outside.push(core::ptr::addr_of!(local) as usize);
        for addr in outside {
            assert_eq!(
                visited.index(addr as *const Eterm, &from),
                None,
                "{addr:#x} must resolve to no bit"
            );
        }
    }

    #[test]
    fn the_visited_bit_is_set_once_per_address() {
        let word = size_of::<Eterm>();
        let mut from = FromSet::new();
        from.add_range(0x1000, 0x1000 + 4 * word);
        from.add_range(0x2000, 0x2000 + 2 * word);
        let mut visited = Visited::new(&from);

        // The first sighting of an address is new, every later one is not — the dedup the
        // traversal's "each object exactly once" rests on.
        let addr = (0x1000 + 2 * word) as *const Eterm;
        let index = visited.index(addr, &from).expect("covered");
        assert!(visited.test_and_set(index), "the first sighting is new");
        assert!(!visited.test_and_set(index), "the second is not");
        // A different word of the same range has its own, still-clear bit.
        let other = visited
            .index(0x1000 as *const Eterm, &from)
            .expect("covered");
        assert_ne!(other, index);
        assert!(visited.test_and_set(other));
        // So does a word of the other range — the second range starts where the first's
        // allocation left off, not at bit 0.
        let second = visited
            .index((0x2000 + word) as *const Eterm, &from)
            .expect("covered");
        assert_eq!(
            second,
            4 + 1,
            "the second range starts past the first's words"
        );
        assert!(visited.test_and_set(second));
    }
}
