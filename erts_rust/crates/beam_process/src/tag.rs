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

//! The `erlang:monitor/3` `{tag, Tag}` **user tag's ownership model**.
//!
//! A tagged monitor's notification is `{Tag, Ref, Kind, Target, Reason}` instead of the ordinary
//! `{'DOWN', …}`, so the tag must stay readable from `erlang:monitor/3` until the DOWN (or the
//! `erlang:demonitor/1`) that consumes it — an unbounded interval in which the *watcher's* heap may
//! be collected any number of times. The legacy C keeps an **immediate** tag inline in the monitor
//! allocation and copies a **non-immediate** one into its own `ErlHeapFragment`
//! (`mk_heap_frag_eterm`, `erts/emulator/beam/erl_monitor_link.c:833-900`) reached through a
//! continuation pointer and freed with the monitor data. [`TagSlot`] is that record in Rust form:
//! `fragment: None` stores the word verbatim (an immediate, or a term in a **literal area** —
//! shared, frozen and never relocated), and `fragment: Some(..)` owns an independent copy.
//!
//! # Why the tag is on no process heap
//!
//! A monitor record is a plain value the collector never scans, so a tag word resident in a
//! per-process heap could not be relocated and a later DOWN would carry a dangling tag. A
//! [`TagSlot`]'s copy lives in a [`HeapFragment`] the *slot* owns, so
//!
//! * the collector neither scans nor relocates it — it is not part of any heap a collection moves;
//! * its off-heap references (a refc binary's payload, an MSO container) are held by the fragment's
//!   own off-heap table, so they outlive every collection of the watcher's heap; and
//! * dropping the slot releases the fragment **and** refcounts those off-heap references down
//!   ([`HeapFragment`]'s own drop), which is what makes `erlang:demonitor/1` a real release.
//!
//! # Where the tag word may travel
//!
//! A fragment-resident word is only valid for as long as the slot lives **and** only against the
//! slot's own off-heap table, so it is never handed to a mailbox/signal queue directly:
//! [`TagSlot::copy_into_heap`] materialises it on the destination process's own heap (where it
//! becomes an ordinary in-heap word — what the queued `PendingSignal::Down`'s `tag` slot, an
//! in-heap sub-rootset, requires), and [`stage_sources`] stages it (together with the DOWN's
//! reason) on one scratch arena with **one merged** off-heap table, so a payload that mixes a tag
//! carrying a refc binary with a reason from another heap can be moved in a single
//! [`beam_message::copy_struct`] step — that copier resolves off-heap references against exactly
//! one source table per call.

use beam_alloc::Heap;
use beam_common::{AllocFailure, AllocKind, BeamError, Result};
use beam_message::{copy_struct, CopyContext, HeapFragment, OffHeapTable};
use beam_term::prelude::{is_boxed_term, is_immediate, is_list, is_literal, Eterm};
use beam_term::size_object;
use crate::process::Process;

/// The words added on top of a [`size_object`] figure when reserving a tag's fragment, so the
/// common small tag costs one copy attempt. An optimisation only: an under-sized reservation is
/// re-reserved from the copy's own refusal (see [`copy_into_fragment`]).
const TAG_SLACK_WORDS: usize = 8;

/// The floor for a tag fragment's reservation — a boxed tuple header plus a few elements.
const TAG_FRAGMENT_FLOOR_WORDS: usize = 16;

/// The floor for the scratch arena a tagged DOWN payload is staged on, before the sized part
/// ([`stage_sources`]) is added: it covers the payload's own envelope (the tuple, the boxed
/// reference and the target).
const STAGE_FLOOR_WORDS: usize = 64;

/// Whether `term` may be stored in a tag slot **without** an owned copy: an **immediate** (an atom,
/// a small integer, a local pid/port, `nil`), or a term resident in a **literal area**
/// (shared, frozen and never relocated).
///
/// [`is_literal`] is a plain `TAG_LITERAL_PTR` bit test, which its own doc restricts to
/// BOXED/LIST words (an immediate's payload bits are unrelated to it), so it is guarded by the
/// pointer-tag test — the `is_boxed(x) || is_list(x)` guard the C's `erts_set_literal_tag` rule
/// uses.
pub(crate) fn is_heap_independent(term: Eterm) -> bool {
    is_immediate(term) || ((is_boxed_term(term) || is_list(term)) && is_literal(term))
}

/// The word count a heap refusal reports, when its byte figure is a whole (nonzero) number of term
/// words — `None` for any other figure.
///
/// The `AllocKind::Message` figure is the copier's **exact destination price** in bytes: the copy
/// preflights the whole graph in one walk and refuses with that count
/// (`beam_message::error::heap_exhausted`). The conversion is the same one `beam_message`'s private
/// `refused_words` makes for its own fragmenting send path.
pub(crate) fn refused_words(requested_bytes: usize) -> Option<usize> {
    let word = core::mem::size_of::<Eterm>();
    let words = requested_bytes / word;
    (words > 0 && requested_bytes % word == 0).then_some(words)
}

/// An owned copy of one `erlang:monitor/3` `{tag, Tag}` user tag: the monitor's proprietary record
/// of it, released exactly when the monitor is.
///
/// An **immediate** or **literal-area** tag is stored verbatim (`fragment: None`) and allocates
/// nothing, so the `{tag, mytag}`-shaped monitor — and every monitor with no tag at all — pays
/// nothing for this model. Any other tag is copied at monitor-creation time into the slot's own
/// [`HeapFragment`] (`fragment: Some(..)`, the C's `mk_heap_frag_eterm`), so every term shape the
/// oracle accepts is accepted here.
pub(crate) struct TagSlot {
    /// The tag's independent copy, when one was needed; `None` for a heap-independent word.
    fragment: Option<HeapFragment>,
    /// The tag word: the caller's own word for a heap-independent tag, the fragment-resident copy
    /// otherwise. Never a word on a process heap.
    root: Eterm,
}

impl TagSlot {
    /// Takes ownership of the user tag `term`, whose off-heap references are described by `src` —
    /// the **watcher's** table, because a heap-resident tag lives in the watcher's young heap and
    /// its refc binaries are registered there (they are resolved and re-registered in the slot's
    /// own table, so the copy survives every collection of that heap).
    ///
    /// Allocates nothing for a heap-independent tag; otherwise reserves from [`size_object`] and
    /// re-reserves **monotonically** from the copy's own refusal, so an under-sized first attempt
    /// cannot fail a monitor.
    ///
    /// # Errors
    ///
    /// Propagates the fragment's `BeamError::Alloc` when the copy cannot be priced (a cyclic graph,
    /// or an exhausted allocator).
    pub(crate) fn new(term: Eterm, src: &OffHeapTable) -> Result<TagSlot> {
        if is_heap_independent(term) {
            return Ok(TagSlot {
                fragment: None,
                root: term,
            });
        }
        let floor = size_object(term)
            .saturating_add(TAG_SLACK_WORDS)
            .max(TAG_FRAGMENT_FLOOR_WORDS);
        let (fragment, root) = copy_into_fragment(term, src, Some(floor))?;
        Ok(TagSlot {
            fragment: Some(fragment),
            root,
        })
    }

    /// The tag word this slot stands for — the caller's own word for a heap-independent tag, the
    /// fragment-resident copy otherwise.
    ///
    /// The fragment-resident form is **only** valid against this slot's off-heap table (see
    /// [`TagSlot::off_heap`]) and only while the slot lives; a consumer that needs it on a process
    /// heap must go through [`TagSlot::copy_into_heap`].
    pub(crate) fn root(&self) -> Eterm {
        self.root
    }

    /// The off-heap table the tag word's references resolve against: the slot's own fragment table
    /// for an owned copy, `None` for a heap-independent word (which references nothing off-heap by
    /// construction).
    pub(crate) fn off_heap(&self) -> Option<&OffHeapTable> {
        self.fragment.as_ref().map(HeapFragment::off_heap)
    }

    /// Whether this slot owns a fragment — i.e. whether the tag must be copied before it can be
    /// embedded in a term on some other heap.
    pub(crate) fn is_fragment_backed(&self) -> bool {
        self.fragment.is_some()
    }

    /// The heap words the slot's own copy occupies (`0` for a heap-independent tag).
    pub(crate) fn fragment_words(&self) -> usize {
        self.fragment.as_ref().map_or(0, HeapFragment::used)
    }

    /// Copies the tag onto `process`'s own young heap and returns the copied word, registering every
    /// off-heap object it references in **that** process's table.
    ///
    /// This is the transfer into the destination's ownership: once copied, the word is an ordinary
    /// in-heap term (the form the queued DOWN's `tag` slot requires, since that slot is an in-heap
    /// sub-rootset the collector gathers and scatters). A heap-independent tag is returned
    /// unchanged.
    ///
    /// Deliberately applies **no** exhaustion policy: the caller owns it, because a
    /// collect-and-grow here could relocate a *previously* materialised receiver-heap word the
    /// caller is still holding (see `ptab`'s tag materialisation).
    ///
    /// # Errors
    ///
    /// Propagates the copy engine's `BeamError::Alloc` when the destination heap cannot take the
    /// copy.
    pub(crate) fn copy_into_heap(&self, process: &mut Process) -> Result<Eterm> {
        match &self.fragment {
            None => Ok(self.root),
            Some(fragment) => process.copy_into_heap(self.root, fragment.off_heap()),
        }
    }

    /// Copies the tag onto the **caller's** heap, registering its off-heap references in the
    /// caller's table `table` — the staging step of a payload that must carry the tag together with
    /// another heap's term (see [`stage_sources`]).
    ///
    /// A heap-independent tag is returned verbatim (it references nothing off-heap).
    ///
    /// # Errors
    ///
    /// Propagates the copy engine's `BeamError::Alloc` when `heap` cannot take the copy.
    pub(crate) fn stage_into(&self, heap: &mut Heap, table: &mut OffHeapTable) -> Result<Eterm> {
        match &self.fragment {
            None => Ok(self.root),
            Some(fragment) => {
                let mut ctx = CopyContext::new(fragment.off_heap(), table);
                copy_struct(self.root, heap, &mut ctx)
            }
        }
    }
}

/// Copies `term`'s graph — whose off-heap references are described by `src` — into a **fresh**
/// [`HeapFragment`] the caller owns, and returns the fragment together with its root.
///
/// `start_words` is the reservation's starting figure (`None` uses
/// [`TAG_FRAGMENT_FLOOR_WORDS`], plus [`TAG_SLACK_WORDS`]). A reservation can be too small — it is
/// a lower bound, and a copy of a refc binary the copier has to flatten exceeds
/// [`size_object`]'s source occupancy — so the copy's own refusal re-reserves the fragment
/// **monotonically** to its exact figure and the copy is retried. A refused copy writes nothing
/// (the preflight refuses before the first word), so the abandoned region is only garbage, and the
/// loop therefore terminates at the true price instead of failing the caller.
///
/// # Errors
///
/// Propagates the fragment's / the copier's `BeamError::Alloc` (or any other typed copy failure,
/// e.g. `BadArg` for a malformed graph) once no strictly larger price is available.
pub(crate) fn copy_into_fragment(
    term: Eterm,
    src: &OffHeapTable,
    start_words: Option<usize>,
) -> Result<(HeapFragment, Eterm)> {
    let mut capacity = start_words.map_or(TAG_FRAGMENT_FLOOR_WORDS, |words| {
        words.saturating_add(TAG_SLACK_WORDS)
    });
    loop {
        let mut candidate = HeapFragment::new(capacity)?;
        match candidate.copy_root(term, src) {
            Ok(root) => return Ok((candidate, root)),
            Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => {
                match refused_words(failure.requested) {
                    // The copy priced the graph exactly: reserve that and retry (strictly larger,
                    // so the loop terminates at the true price).
                    Some(words) if words > capacity => capacity = words,
                    _ => return Err(BeamError::Alloc(failure)),
                }
            }
            Err(other) => return Err(other),
        }
    }
}

/// A DOWN payload's two heap-resident sources — the `{tag, Tag}` user tag and the exit `reason` —
/// staged on **one** scratch arena with **one merged** off-heap table.
///
/// The merge is the whole point: a tag may carry a refc binary (whose payload is named by the
/// slot's own table) while the reason lives in a *different* process's heap (named by that
/// process's table), and [`beam_message::copy_struct`] resolves off-heap references against
/// exactly **one** source table per call. Staging both here first makes the payload
/// self-contained, so it can be moved onto a receiver's heap (or into a message heap fragment) in
/// a single copy against [`StagedSources::table`] — never mixing two tables' slot numbering.
///
/// `tag` is `None` when the monitor is untagged (or its tag is already a receiver-heap word);
/// `reason_src` is the table the reason's off-heap references live in, and an immediate reason is
/// embedded verbatim (nothing to stage).
///
/// The scratch arena is sized from the two [`size_object`] figures (plus `extra_words` for the
/// payload's own envelope) and re-sized **monotonically** from the staging copy's own refusal, for
/// the same reason [`copy_into_fragment`] does it.
///
/// # Errors
///
/// Propagates the copier's `BeamError::Alloc` when the staging arena cannot be sized up to the
/// true price, and any other typed copy failure unchanged.
pub(crate) fn stage_sources(
    tag: Option<&TagSlot>,
    reason: Eterm,
    reason_src: &OffHeapTable,
    extra_words: usize,
) -> Result<StagedSources> {
    let mut capacity = STAGE_FLOOR_WORDS.saturating_add(extra_words);
    if let Some(slot) = tag {
        if slot.is_fragment_backed() {
            capacity = capacity.saturating_add(size_object(slot.root()));
        }
    }
    if !is_immediate(reason) {
        capacity = capacity.saturating_add(size_object(reason));
    }
    loop {
        let mut scratch = Heap::new(capacity)?;
        let mut table = OffHeapTable::new();
        match stage_into(&mut scratch, &mut table, tag, reason, reason_src) {
            Ok((staged_tag, staged_reason)) => {
                return Ok(StagedSources {
                    scratch,
                    table,
                    tag: staged_tag,
                    reason: staged_reason,
                })
            }
            Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => {
                match refused_words(failure.requested) {
                    Some(words) if words > capacity => capacity = words,
                    _ => return Err(BeamError::Alloc(failure)),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

/// A DOWN payload's staged sources, plus the arena they (and the payload built around them) live
/// on — see [`stage_sources`].
pub(crate) struct StagedSources {
    /// The scratch arena the staged words and the payload built around them live on. It must stay
    /// alive until the payload has been copied somewhere durable.
    pub(crate) scratch: Heap,
    /// The merged off-heap table the staged words' references resolve against — the **one** source
    /// table every later copy of the payload must use.
    pub(crate) table: OffHeapTable,
    /// The staged tag word (`None` for an untagged monitor).
    pub(crate) tag: Option<Eterm>,
    /// The staged reason word.
    pub(crate) reason: Eterm,
}

/// The staging body of [`stage_sources`]: copies the tag and the reason into `scratch`, merging
/// their off-heap references into `table`.
fn stage_into(
    scratch: &mut Heap,
    table: &mut OffHeapTable,
    tag: Option<&TagSlot>,
    reason: Eterm,
    reason_src: &OffHeapTable,
) -> Result<(Option<Eterm>, Eterm)> {
    let staged_tag = match tag {
        Some(slot) => Some(slot.stage_into(scratch, table)?),
        None => None,
    };
    let staged_reason = if is_immediate(reason) {
        reason
    } else {
        let mut ctx = CopyContext::new(reason_src, table);
        copy_struct(reason, scratch, &mut ctx)?
    };
    Ok((staged_tag, staged_reason))
}

/// The destination price, in words, of the payload a refusal refused — or `None` when the refusal
/// carries no such price.
///
/// Only an [`AllocKind::Message`] refusal is the copier's preflight figure. A [`AllocKind::Heap`]
/// one is some *other* allocation (an off-heap slot table growing, say) whose byte count says
/// nothing about the payload, and is never used as a reservation.
pub(crate) fn refused_payload_words(failure: AllocFailure) -> Option<usize> {
    match failure.kind {
        AllocKind::Message => refused_words(failure.requested),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the tag's ownership model: the immediate/literal no-allocation rule, the
    //! independent copy of a heap-resident tag, the refc-binary re-registration of a transfer, the
    //! release on drop, and the two-source staging that lets a tag travel with another heap's
    //! reason.
    use beam_alloc::RefcBinary;
    use beam_term::compare;
    use beam_term::prelude::{
        is_list, is_literal, make_arityval, make_small, make_tuple, set_literal_tag, tuple_element,
        tuple_size, LiteralArea, ERL_REFC_BITS_SIZE,
    };
    use super::*;
    use crate::pid::ProcessId;

    /// A process with a default heap (the destination of a transfer).
    fn process(pid: u64) -> Process {
        Process::new(ProcessId::new(pid, 0)).expect("the default heap allocates")
    }

    /// A 200-byte off-heap payload (`> ERL_ONHEAP_BINARY_LIMIT`, so its bytes really live in a
    /// counted off-heap object).
    fn payload_binary() -> RefcBinary {
        let bytes: Vec<u8> = (0..200usize).map(|i| ((i * 7) % 251) as u8).collect();
        RefcBinary::new(&bytes).expect("the payload allocates")
    }

    /// Writes the on-heap `[BinRef][ErlSubBits]` pair naming `binary`'s payload on `heap`.
    ///
    /// # Safety
    ///
    /// `heap` must have [`ERL_REFC_BITS_SIZE`] spare words, and the caller must keep `binary` (or
    /// another handle to its payload) alive as long as the returned term is used.
    unsafe fn bin_ref_pair(heap: &mut Heap, binary: &RefcBinary) -> Eterm {
        // SAFETY: the caller reserves the words and keeps the payload alive; the payload pointer is
        // word-aligned (`beam_alloc` hands out `base + HEADER_SIZE`, a whole unit).
        unsafe {
            let hp = heap
                .alloc_words(ERL_REFC_BITS_SIZE)
                .expect("the refc pair fits the test heap");
            beam_term::prelude::write_refc_binary(
                hp,
                binary.data_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                binary.data_ptr(),
                0,
                binary.as_bytes().len() * 8,
            )
        }
    }

    #[test]
    fn an_immediate_tag_is_stored_verbatim_and_allocates_nothing() {
        let empty = OffHeapTable::new();
        let tag = make_small(17);
        let slot = TagSlot::new(tag, &empty).expect("an immediate needs no allocation");
        assert!(!slot.is_fragment_backed());
        assert_eq!(slot.root(), tag, "the caller's own word is stored");
        assert_eq!(slot.fragment_words(), 0, "no fragment was reserved");
        assert!(slot.off_heap().is_none(), "an immediate references nothing off-heap");
        // The transfer is the identity — no copy, so a receiver keeps the same word.
        let mut process = process(1);
        assert_eq!(
            slot.copy_into_heap(&mut process).expect("an immediate copies"),
            tag
        );
    }

    #[test]
    fn a_literal_area_tag_is_stored_verbatim_and_allocates_nothing() {
        let mut area = LiteralArea::new(8);
        // SAFETY: two words are requested and both written before anything is read.
        let words = unsafe { area.alloc_words(2) }.expect("the literal area has room");
        // SAFETY: the two words were just handed out exclusively and both are written here.
        unsafe {
            words.write(make_arityval(1));
            words.add(1).write(make_small(9));
        }
        let mut tag = make_tuple(words);
        // SAFETY: `words` addresses the two live initialised words inside `area`.
        unsafe { set_literal_tag(&mut tag, words, 2) };
        assert!(is_literal(tag) && !is_immediate(tag));

        let empty = OffHeapTable::new();
        let slot = TagSlot::new(tag, &empty).expect("a literal-area tag needs no allocation");
        assert!(!slot.is_fragment_backed());
        assert_eq!(slot.root(), tag, "the literal word is shared, so it is stored as-is");
        assert_eq!(slot.fragment_words(), 0);
    }

    #[test]
    fn a_heap_resident_tag_owns_an_independent_copy() {
        let empty = OffHeapTable::new();
        let mut source = Heap::new(16).expect("a test heap");
        let tag = source
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits");
        let slot = TagSlot::new(tag, &empty).expect("a heap tag is copied");
        assert!(slot.is_fragment_backed());
        assert_ne!(slot.root(), tag, "the copy lives in the slot's own fragment");
        assert!(slot.fragment_words() >= 3, "the tuple's three words are copied");

        // Overwrite every word the source heap handed out: the slot's copy is untouched, which is
        // exactly the property a per-process-heap word could not offer.
        for word in source.used_words_mut() {
            *word = make_small(0);
        }
        let expected = source
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple is rebuilt");
        assert!(compare::eq(slot.root(), expected, &beam_term::prelude::AtomTable::new()));
    }

    #[test]
    fn dropping_the_slot_releases_the_fragment_and_its_refcounted_payload() {
        let binary = payload_binary();
        let mut table = OffHeapTable::new();
        table.push_binary(binary.clone());
        let mut source = Heap::new(ERL_REFC_BITS_SIZE + 4).expect("a test heap");
        // SAFETY: the heap has the pair's words and `binary` keeps the payload alive.
        let tag = unsafe { bin_ref_pair(&mut source, &binary) };
        let baseline = binary.refcount();
        let slot = TagSlot::new(tag, &table).expect("the binary-carrying tag is copied");
        assert_eq!(
            binary.refcount(),
            baseline + 1,
            "the slot's fragment holds its own handle to the payload"
        );
        drop(slot);
        assert_eq!(
            binary.refcount(),
            baseline,
            "dropping the slot releases the handle with the fragment"
        );
    }

    #[test]
    fn a_transfer_re_registers_the_refc_binary_in_the_destination_table() {
        let binary = payload_binary();
        let mut table = OffHeapTable::new();
        table.push_binary(binary.clone());
        let mut source = Heap::new(ERL_REFC_BITS_SIZE + 4).expect("a test heap");
        // SAFETY: the heap has the pair's words and `binary` keeps the payload alive.
        let tag = unsafe { bin_ref_pair(&mut source, &binary) };
        let slot = TagSlot::new(tag, &table).expect("the binary-carrying tag is copied");
        assert!(
            slot.off_heap()
                .expect("the slot owns a table")
                .binary_by_data_ptr(binary.data_ptr())
                .is_some(),
            "the slot's own table holds the payload"
        );

        let mut process = process(2);
        let copied = slot.copy_into_heap(&mut process).expect("the copy fits");
        assert!(
            process
                .off_heap()
                .binary_by_data_ptr(binary.data_ptr())
                .is_some(),
            "the destination table re-registered the payload"
        );
        // The copied word is the same shape as the source (`=:=` over a refc binary compares the
        // slice), so a `{Tag, …}` built around it is the oracle's.
        assert!(compare::eq(copied, tag, &beam_term::prelude::AtomTable::new()));
    }

    #[test]
    fn staging_merges_two_off_heap_tables_into_one() {
        // A tag carrying a refc binary (named by the slot's table) and a reason that is a *heap*
        // tuple carrying a different refc binary (named by the reason's own table): the merge is
        // what lets one copy reproduce both.
        let tag_binary = payload_binary();
        let mut tag_table = OffHeapTable::new();
        tag_table.push_binary(tag_binary.clone());
        let mut tag_heap = Heap::new(ERL_REFC_BITS_SIZE + 4).expect("a test heap");
        // SAFETY: the heap has the pair's words and `tag_binary` keeps the payload alive.
        let tag_term = unsafe { bin_ref_pair(&mut tag_heap, &tag_binary) };
        let slot = TagSlot::new(tag_term, &tag_table).expect("the tag is copied");

        let reason_binary = RefcBinary::new(&[7u8; 200]).expect("the reason payload allocates");
        let mut reason_table = OffHeapTable::new();
        reason_table.push_binary(reason_binary.clone());
        let mut reason_heap = Heap::new(ERL_REFC_BITS_SIZE + 8).expect("a test heap");
        // SAFETY: the heap has the pair's words and `reason_binary` keeps the payload alive.
        let reason_pair = unsafe { bin_ref_pair(&mut reason_heap, &reason_binary) };
        let reason = reason_heap
            .tuple(&[make_small(5), reason_pair])
            .expect("the reason tuple fits");

        let StagedSources {
            scratch,
            table,
            tag,
            reason,
        } = stage_sources(Some(&slot), reason, &reason_table, 16).expect("the staging fits");
        assert!(scratch.used() > 0);
        assert!(
            table.binary_by_data_ptr(tag_binary.data_ptr()).is_some(),
            "the tag's payload is merged in"
        );
        assert!(
            table.binary_by_data_ptr(reason_binary.data_ptr()).is_some(),
            "the reason's payload is merged in"
        );
        let staged_tag = tag.expect("the tag was staged");
        assert!(compare::eq(staged_tag, tag_term, &beam_term::prelude::AtomTable::new()));
        // SAFETY: the staged reason really is the 2-tuple built above.
        unsafe {
            assert_eq!(tuple_size(reason), 2);
            assert_eq!(tuple_element(reason, 0), make_small(5));
        }
        assert!(
            compare::eq(
                // SAFETY: the staged reason really is the 2-tuple built above.
                unsafe { tuple_element(reason, 1) },
                reason_pair,
                &beam_term::prelude::AtomTable::new()
            ),
            "the reason's refc sub-term is copied into the scratch arena as the same slice"
        );
        assert!(!is_list(staged_tag));
    }
}
