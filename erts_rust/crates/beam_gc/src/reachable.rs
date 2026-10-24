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
//! collector methods that call [`Scratch::for_each_view`] add no `unsafe` of their own and assume
//! no interior layout beyond the canonical shape walk.
//!
//! # [`Scratch`] — the reused, and therefore clearable, walk state
//!
//! A walk needs three allocations: the visited bitmap (one bit per from-set word), the worklist,
//! and the from-set's own range vector. A collector that walks at every collection would allocate
//! all three again each round, so [`Scratch`] owns them once and reuses them
//! ([`crate::Collector`] and [`crate::GenImmixCollector`] each hold one; the one-shot public
//! wrappers — [`for_each_reachable_in`], [`crate::live_words_in`], the off-heap liveness scan and
//! [`FromSet::new`](crate::FromSet::new) — allocate a fresh `Scratch` internally instead).
//!
//! **Reuse is keyed on addresses, so it is only sound if the state is cleared.** The bitmap is
//! indexed by from-set *address*, so a stale set bit makes a second walk skip an object it has
//! never visited (a wrong count, and — through the collector's room figures — a stranded live
//! term), and a stale range makes a collection evacuate a space that may already be freed. The
//! reset is therefore not optional bookkeeping and cannot be skipped:
//!
//! * there is **no `pub` constructor of a dirty scratch**: [`Scratch::new`] is empty, and
//! * every walk entry point ([`Scratch::walk`], and the two enumerations built on it) zeroes the
//!   **whole used prefix** of the bitmap and truncates the worklist *before* it admits anything,
//!   while the range vector is only ever installed through [`Scratch::begin`] /
//!   [`Scratch::begin_with`], which clear it first.
//!
//! `tests/scratch_reuse.rs` pins the consequence from outside: two consecutive walks over the
//! **same** address range with **different** roots report exactly what a fresh scratch reports.

use core::mem::size_of;

use beam_common::Result;
use beam_term::prelude::{is_header, primary_tag, ptr_val, Eterm, PrimaryTag};

use crate::copy::{for_each_ref, move_size};
use crate::space::FromSet;

/// The reusable state of a reachability walk: the visited bitmap, the worklist and the from-set
/// range vector — see the module docs for why every entry point clears what it reuses.
pub(crate) struct Scratch {
    /// One bit per word of the installed from-set, plus the per-range bit offsets.
    visited: Visited,
    /// The traversal worklist, reused across walks.
    work: Vec<*mut Eterm>,
    /// The from-set the walk (and, for a collection, the copier) reads.
    from: FromSet,
}

impl Scratch {
    /// An empty scratch: no ranges, no bits, no work — allocating nothing.
    pub(crate) fn new() -> Scratch {
        Scratch {
            visited: Visited::default(),
            work: Vec::new(),
            from: FromSet::new(),
        }
    }

    /// Installs a from-set for the next walk (or copy), **clearing the previous one** and letting
    /// `build` fill it. The only way to put ranges into a scratch, so a stale range — a range
    /// naming a region a previous collection may have freed — cannot survive into a later walk.
    pub(crate) fn begin<F>(&mut self, build: F)
    where
        F: FnOnce(&mut FromSet),
    {
        self.from.clear();
        build(&mut self.from);
    }

    /// Installs `from` as this scratch's from-set, **replacing** (never merging with) whatever was
    /// there — the reuse form for a caller that already owns the range set it wants to walk.
    pub(crate) fn begin_with(&mut self, from: &FromSet) {
        self.from.copy_from(from);
    }

    /// The installed from-set (empty until [`begin`](Scratch::begin) or
    /// [`begin_with`](Scratch::begin_with) filled it).
    pub(crate) fn from(&self) -> &FromSet {
        &self.from
    }

    /// Walks the object graph reachable from `roots` out of the installed from-set's ranges,
    /// calling `on_object(first_word, ptr, total_words)` once per object — see the module docs.
    ///
    /// `first_word` is the object's **first** heap word: a header word for a boxed object, or the
    /// `car` of a cons cell (which carries no header). `total_words` is the object's size in words
    /// (its [`move_size`](crate::copy::move_size), or `2` for a cons cell). `roots` is borrowed
    /// immutably — the walk never rewrites a term — and it never dereferences an address outside
    /// the from-set's ranges.
    ///
    /// The visited bitmap and the worklist are **cleared** here, so a walk over the same
    /// from-set may be repeated (with any roots) and reports exactly what a fresh scratch would.
    ///
    /// The only fallible step is the shared [`for_each_ref`](crate::copy::for_each_ref)
    /// enumeration, whose signature is fallible for its other callers (a copier's forwarding
    /// `visit` can fail); `on_object` itself is infallible, so in practice this never returns an
    /// error.
    pub(crate) fn walk<F>(&mut self, roots: &[Eterm], mut on_object: F) -> Result<()>
    where
        F: FnMut(Eterm, *const Eterm, usize),
    {
        // The whole reused state is reset *before* anything is admitted: the bitmap is keyed on
        // addresses of the installed from-set, so a bit left over from a previous walk would skip
        // an object this walk has never seen, and a worklist entry would point into a range that
        // may be a different allocation by now.
        self.visited.reset(&self.from);
        self.work.clear();
        let (visited, work) = (&mut self.visited, &mut self.work);
        let from = &self.from;

        for &root in roots {
            seed(root, from, visited, work);
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
                        admit(ptr_val(term) as *const Eterm, from, visited, work);
                    }
                    Ok(())
                })
            };
            walk?;
        }

        Ok(())
    }

    /// Visits every **boxed object** reachable from `roots` out of the installed from-set, each
    /// exactly once, passing a safe read-only [`ReachableObject`] view of it — the safe sibling of
    /// [`walk`](Scratch::walk), over the same walk and the same dedup.
    ///
    /// A cons cell is *traversed* (so the boxed objects behind it are reached) but is **not**
    /// reported: it carries no header word.
    pub(crate) fn for_each_view<F>(&mut self, roots: &[Eterm], mut visit: F) -> Result<()>
    where
        F: FnMut(ReachableObject<'_>),
    {
        self.walk(roots, |first, ptr, total| {
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

    /// Visits every **boxed object** reachable from `roots` out of the installed from-set, each
    /// exactly once, passing the object's header word and a pointer to that header — the raw
    /// sibling of [`for_each_view`](Scratch::for_each_view).
    pub(crate) fn for_each_boxed<F>(&mut self, roots: &[Eterm], mut visit: F) -> Result<()>
    where
        F: FnMut(Eterm, *const Eterm),
    {
        self.for_each_view(roots, |obj| visit(obj.header(), obj.words().as_ptr()))
    }
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
///
/// The from-set must already be installed in `scratch` ([`Scratch::begin`] /
/// [`Scratch::begin_with`]), which is what lets a collector walk the same ranges twice without
/// rebuilding them; the visited state is cleared per call, so repeated walks cannot contaminate
/// one another.
pub(crate) fn for_each_reachable_view<F>(
    scratch: &mut Scratch,
    roots: &[Eterm],
    visit: F,
) -> Result<()>
where
    F: FnMut(ReachableObject<'_>),
{
    scratch.for_each_view(roots, visit)
}
/// Visits every **boxed object** reachable from `roots` out of the ranges of a from-set, each
/// exactly once, passing a safe read-only [`ReachableObject`] view of it — the **public** form of
/// [`for_each_reachable_view`], for a host whose region is not a process heap.
///
/// `from` may be built from raw word ranges
/// ([`FromSet::from_ranges`](crate::FromSet::from_ranges)), so inspecting a foreign arena (a
/// `beam_alloc::ReservedHeap`, a pre-reserved `Vec<Eterm>`, the engine's term arena) needs no
/// collector and no `unsafe` of its own. It is the walk the collectors use, so it is **read-only**
/// (it writes no forwarding marker, so it can run immediately before a
/// [relocation](crate::Relocator) over the same ranges), **iterative** (a deep list cannot
/// overflow the stack) and **deduplicated by object address** (a shared subterm or a cycle is
/// reported once). A cons cell is *traversed* — the boxed objects behind it are reached — but is
/// **not** reported: it carries no header word.
///
/// The visitor is `FnMut(ReachableObject<'_>)` (higher-ranked over the borrow), so it cannot
/// retain the view past the callback.
///
/// This is a one-shot entry point: it owns a fresh [`Scratch`] for the call. A caller that walks
/// repeatedly (a collector at every collection) holds a scratch and calls
/// [`Scratch::for_each_view`] instead, which reuses the bitmap, the worklist and the range vector.
pub fn for_each_reachable_in<F>(from: &FromSet, roots: &[Eterm], visit: F) -> Result<()>
where
    F: FnMut(ReachableObject<'_>),
{
    let mut scratch = Scratch::new();
    scratch.begin_with(from);
    for_each_reachable_view(&mut scratch, roots, visit)
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
/// For a visitor that need not touch raw pointers, prefer
/// [`for_each_reachable_in`], which reports the same objects as a safe [`ReachableObject`]. Like
/// its safe sibling it walks the from-set already installed in `scratch`.
pub(crate) fn for_each_reachable<F>(
    scratch: &mut Scratch,
    roots: &[Eterm],
    visit: F,
) -> Result<()>
where
    F: FnMut(Eterm, *const Eterm),
{
    scratch.for_each_boxed(roots, visit)
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
///
/// A [`Scratch`] reuses one across walks, so [`reset`](Visited::reset) must clear **every** bit
/// the previous walk may have set: the bits are keyed on addresses, and the next from-set may
/// cover the very same addresses with entirely different objects.
#[derive(Default)]
struct Visited {
    /// `first[k]` is the bit index range `from.ranges()[k]` starts at — the ranges (in
    /// [`FromSet::ranges`]'s order) are packed into one bit vector, so a range's `lo`/`hi` are
    /// read from the from-set itself and never copied here.
    first: Vec<usize>,
    bits: Vec<u64>,
}

impl Visited {
    /// Re-indexes the ranges of `from` (`from.ranges()` is byte-addressed; a range added from a
    /// space's live region spans a whole number of term words) and **clears the whole bitmap**,
    /// then truncates or grows it to the size those ranges need.
    ///
    /// The `fill(0)` is what makes reuse sound: it zeroes every bit of the *used* prefix,
    /// including the tail bits of the last word, so a walk that reuses the vector can never see a
    /// bit an earlier walk set. `resize` then drops the bits of a from-set that shrank and
    /// zero-fills the ones a larger from-set added.
    fn reset(&mut self, from: &FromSet) {
        let word = size_of::<Eterm>();
        self.first.clear();
        let mut total = 0usize;
        for &(lo, hi) in from.ranges() {
            self.first.push(total);
            total += (hi - lo) / word;
        }
        self.bits.fill(0);
        self.bits.resize(total.div_ceil(64), 0);
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
    //!
    //! The last test is the **reuse soundness** half: one [`Scratch`] walked repeatedly over one
    //! from-set with different root sets reports exactly what a fresh scratch reports, which is
    //! what makes the collector-held scratch a pure allocation saving rather than a behaviour
    //! change. A stale set bit would make the second walk skip an object it has never visited —
    //! an under-count, and through the collector's room figures a stranded live term.

    use super::*;
    use beam_term::prelude::make_small;

    use crate::space::HeapSpace;
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

        let visited = {
            let mut visited = Visited::default();
            visited.reset(&from);
            visited
        };
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
        let mut visited = Visited::default();
        visited.reset(&from);

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

    /// Counts `(words, objects)` reachable from `roots` out of the scratch's installed from-set —
    /// the raw [`Scratch::walk`], which is where the visited set is consumed.
    fn count(scratch: &mut Scratch, roots: &[Eterm]) -> (usize, usize) {
        let (mut words, mut objects) = (0usize, 0usize);
        scratch
            .walk(roots, |_first, _ptr, total| {
                words += total;
                objects += 1;
            })
            .expect("the counting closure never fails");
        (words, objects)
    }

    /// **Reuse soundness**: one [`Scratch`] walked several times over the **same** from-set with
    /// **different** root sets reports exactly what a fresh scratch reports each time.
    ///
    /// The from-set is a single nursery-shaped range, so every round walks the very same
    /// addresses: a bit left over from the previous walk would make the next one skip an object at
    /// an address it has already seen — an under-count. The graph is built so that a stale bit is
    /// *unmistakable*: the subterm `S` is shared by both roots, so a scratch that failed to clear
    /// would report `(3, 1)` (only `B`) on the second walk instead of `(6, 2)`, and `(6, 2)`
    /// instead of `(9, 3)` on the joint one.
    #[test]
    fn a_reused_scratch_reports_what_a_fresh_one_does() {
        let mut space = HeapSpace::new(64).expect("nursery");
        // `S = {7,8}` (3 words), shared by `A = {S,1}` and `B = {S,2}` (3 words each), plus a
        // garbage tuple nothing reaches.
        let shared = space
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("shared");
        let a = space
            .heap_mut()
            .tuple(&[shared, make_small(1)])
            .expect("a");
        let b = space
            .heap_mut()
            .tuple(&[shared, make_small(2)])
            .expect("b");
        let _garbage = space
            .heap_mut()
            .tuple(&[make_small(3), make_small(4)])
            .expect("garbage");

        let mut from = FromSet::new();
        from.add(&space);

        // The three rounds, each its own root set: `A` alone (S is reached through it), `B` alone
        // (S is reached again — a *separate* walk counts it a separate time), and both together (S
        // once, because one walk deduplicates by address).
        let rounds: [(&[Eterm], (usize, usize)); 3] = [
            (&[a], (6, 2)),
            (&[b], (6, 2)),
            (&[a, b], (9, 3)),
        ];

        // One scratch, walked three times.
        let mut reused = Scratch::new();
        reused.begin_with(&from);
        for (roots, expected) in rounds {
            assert_eq!(
                count(&mut reused, roots),
                expected,
                "the reused scratch must report the round's own live set"
            );
        }

        // A fresh one-shot scratch per round: the reference figures.
        for (roots, expected) in rounds {
            let mut fresh = Scratch::new();
            fresh.begin_with(&from);
            assert_eq!(
                count(&mut fresh, roots),
                expected,
                "the one-shot reference walk"
            );
        }

        // And the reused scratch's answers equal the fresh ones round for round, whichever order
        // the rounds run in: re-running the first round *after* the others must still count S.
        reused.begin_with(&from); // re-installing the same ranges must not disturb anything
        assert_eq!(count(&mut reused, &[a]), (6, 2));
        assert_eq!(count(&mut reused, &[b]), (6, 2));
    }

    /// A reused scratch tolerates a **different** from-set between walks: `begin` replaces the
    /// ranges (never merges them), and the next walk's bits belong to the new ranges only.
    ///
    /// This is the collector-level sequence in miniature — a collection installs its from-set,
    /// walks, and the *next* collection installs a different one (a collapsed nursery at a new
    /// base, a relocated mature space).
    #[test]
    fn a_reused_scratch_reinstalls_its_from_set() {
        let mut space = HeapSpace::new(64).expect("nursery");
        let a = space
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("a");
        let mut from = FromSet::new();
        from.add(&space);

        let mut scratch = Scratch::new();
        scratch.begin_with(&from);
        assert_eq!(count(&mut scratch, &[a]), (3, 1));
        assert_eq!(scratch.from().ranges().len(), 1, "one range installed");

        // A second, distinct region: the reused scratch must forget the first entirely.
        let mut other = HeapSpace::new(64).expect("second region");
        let c = other
            .heap_mut()
            .tuple(&[make_small(5), make_small(6), make_small(7)])
            .expect("c");
        scratch.begin(|from| from.add(&other));
        assert_eq!(scratch.from().ranges().len(), 1, "replaced, never merged");
        assert_eq!(count(&mut scratch, &[c]), (4, 1));
        // The first region's term is now *outside* the installed from-set: skipped whole.
        assert_eq!(count(&mut scratch, &[a]), (0, 0));
    }
}
