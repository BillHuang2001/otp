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
//! The **NIF-resource half** of [`super`]'s reclamation: the two **non-park** cadence points at
//! which a live `enif_make_resource` term whose last reference died may drop the reference it took.
//!
//! # Ground truth, and the site that already exists
//!
//! `erts/emulator/beam/erl_nif.c`: `enif_make_resource` (`:3197`) mints an `ERTS_MAGIC_REF` whose
//! block owns **one reference** on the resource; the term GC drops that reference when the term
//! becomes unreachable, and the release that takes the count to zero runs the type's `dtor`
//! (`nif_resource_dtor_prologue` → `run_resource_dtor`, `:2961-3030`). `beam_abi` mints the term and
//! takes the reference, but it has **no GC** — which terms are still reachable is only knowable by
//! the host, so the seam is a caller-supplied predicate
//! ([`beam_abi::nif::enif::resources::release_resource_terms`]).
//!
//! The process route's caller is [`crate::context::gc`]'s `resources::reclaim`, driven from the
//! runner's `Yield` **park** arm, whose module docs are that site's design record. This module adds
//! the two routes that site structurally cannot serve:
//!
//! * **the `-eval` route's statement boundary** ([`prune_at_statement_boundary`], called between
//!   the top-level statements of a `-eval` expression). The `-eval` **action process is never
//!   enqueued**, so it never parks: without this site an `-eval`-only run retains every resource
//!   term it ever minted for the runtime's life;
//! * **a nested run's epilogue** ([`prune_beyond`], called from
//!   [`RuntimeContext::adopt_nested_payloads`](crate::context::RuntimeContext)). A cross-module
//!   call (a `lists:map/2` body, `base64:encode/1`) runs on a **fresh `Machine` that is not a
//!   scheduled process**, so it never parks either — and a nested run is where a per-call resource
//!   leak costs the most, because it repeats once per element.
//!
//! # The frontier, and why only *fresh* candidates may ever be released
//!
//! A prune may only free what it can **prove** dead, and the proof is always "nothing in *my*
//! complete root enumeration names it". Neither site has a complete enumeration of its process:
//!
//! * the statement boundary can name the evaluator's environment and the reduced value (the
//!   caller's `extra`) plus everything [`Runtime::gather_off_heap_roots`] hands over, but a resource
//!   minted **before** this expression started may be held by a store outside that enumeration;
//! * a nested site can name exactly **one machine**'s roots, while the *enclosing* machines of the
//!   same process (a suspended `lists:map/2`, the slice's own machine) are alive and hold terms this
//!   crate cannot see.
//!
//! So each site releases only candidates **minted after a known earlier point** — its
//! [`Frontier`]. A candidate the frontier already reports existed before the site began is retained
//! unconditionally, which is the soundness device (and the mechanism the sibling refc prune uses in
//! its scoped form, whose measurement — 190/200 corrupted elements with an unscoped prune at a
//! nested site — is the reason this rule is not negotiable). A term minted *during* the site's own
//! window can only have escaped into an unseen store by being **returned**, and the returned term
//! rides the site's own roots, so it is accounted for.
//!
//! The consequence is a **bounded one-window leak** per site, documented with each:
//!
//! * the statement boundary's very first boundary of a runtime has no previous observation, so it
//!   records the frontier and releases **nothing**;
//! * the nested site's frontier is taken at **run entry**, so a run that mints a term and drops it
//!   before returning frees it at its own epilogue — the leak is only what an enclosing store
//!   un-nameably holds.
//!
//! # No region word scan — and the proof that none is possible
//!
//! The sibling refc prune needs a region scan: a `[BinRef][ErlSubBits]` pair standing in one of the
//! runtime's term regions ([`Runtime::for_each_off_heap_region`]) is found by its own **header**
//! word, which is what keeps a pair inside a container this module cannot read from being missed.
//! **No such scan exists for resources, and it cannot**: a resource's identity (`node`, `serial`,
//! `id`) lives **inside** its 4-word `[header][node][serial][id]` block, and the block that was
//! minted stands in a runtime region itself (the pooled BIF heap a NIF result is built on). A scan
//! that "found" a resource identity in a region would therefore find the **mint block** — the very
//! candidate it is asking about — and would match every candidate at every site, every time, making
//! the prune a permanent no-op. A word-pattern scan cannot distinguish a live mint block from a dead
//! copy of it, whereas a `[BinRef]`'s payload pointer is a *value* written next to its header.
//!
//! Omitting it is nevertheless **sound** here, for the reason this descent is region-agnostic: it
//! never consults the collector's scanned bounds, so it dereferences and expands containers in
//! **every** region the roots reach (unlike the process-site walk, whose geometry forces the
//! `reaches_an_unscanned_word` valve). A *live* copy of a resource term is therefore reached from
//! the named roots wherever it stands; a copy standing in a region that nothing names is dead data
//! — and the regions are never reset, so treating their words as roots would retain forever.
//!
//! # The gates (every one is a decline, and a decline is a leak)
//!
//! 1. **The kill switch** ([`Runtime::set_resource_prune_enabled`]) — off by the caller's choice;
//! 2. **the frontier is unchanged** — `O(1)`: with no live resources both snapshots are empty and
//!    the compare is one length check (the overwhelmingly common case: no NIF ever minted one);
//! 3. **the fresh set is empty** — candidates exist, but none was minted inside this site's window;
//! 4. **the walk declined** — the visit budget was spent or an unknown boxed header was met (see
//!    [`super`]'s decline rule). Nothing is released, and the *observed* frontier still advances, so
//!    a once-declined candidate is retained for the runtime's life rather than re-walked on every
//!    subsequent statement (`doubt ⇒ retain`, and a persistent decline must not cost a full walk per
//!    statement).
//!
//! # The residual class (shared with the park-arm site; each a leak, never a dangle)
//!
//! * a resource term held **only** by a store outside the walk's root families — another process's
//!   machine heap, an ETS table, a `persistent_term` value, a `seq_trace` token this runtime's
//!   [`Runtime::gather_off_heap_roots`] does not enumerate — is answered unreachable, so its
//!   destructor never runs. This is exactly the park-arm prune's residual, and closing it needs the
//!   same host root-source seam (`Runtime::root_terms`);
//! * a candidate whose **reference identity cannot be read** (its minted block no longer holds a
//!   reference header) is recorded as reached rather than released: "cannot identify it" must never
//!   become "it is dead";
//! * the nested site's **error path** prunes nothing: a raise's reason term is not part of the
//!   frozen root set, so there is no complete root list to hand over (the refc half takes the same
//!   decision, for the same reason);
//! * **pointer reuse**: the pooled BIF heap is rewound between calls, so a later mint can reuse a
//!   freed address. A fresh mint that lands on a word the frontier already lists looks *pre-existing*
//!   and is therefore retained — a leak, never a dangle. (The reverse cannot happen: a word in the
//!   frontier that no longer exists is simply absent from the snapshot.)
//!
//! # Steady state
//!
//! Per site, with no live resources: one [`Frontier::snapshot`] (an empty `Vec`, no allocation) and
//! one `Vec` compare. Per site, with live resources but none fresh: the same snapshot plus the
//! sorted difference, and **no walk**. A walk (and therefore anything that dereferences a term) only
//! runs when a candidate was minted inside the site's own window.
// The narrow `unsafe` override `super` documents, extended to this half's single raw read: a
// **candidate** word is the head of a 4-word reference block `beam_abi` minted on the
// pointer-stable pooled BIF heap (so the block stays readable for the runtime's lifetime), and a
// **reached** word is admitted exactly as the descent admits one — `internal_ref_id` validates the
// header, the arity and the id word before it reads anything past the header, so a word that is not
// a reference yields `None` without an out-of-bounds read.
#![allow(unsafe_code)]

use std::mem::align_of;

use beam_term::Eterm;

use beam_abi::nif::enif::resources::{release_resource_terms, resource_terms};

use super::{walk, Prune, Reach};
use crate::runtime::Runtime;

/// The candidate set as of one point in time: the raw words of every live resource term
/// ([`resource_terms`], already sorted and unique — one entry per mint).
///
/// A `Frontier` is the *evidence* a site prunes on: candidates **not** in it may be released (they
/// were minted inside the site's window), candidates in it are retained unconditionally (they
/// predate the site, so a store the site cannot see may hold them). See the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Frontier {
    /// The minted words, sorted ascending and unique (the registry's own order).
    words: Vec<usize>,
}

impl Frontier {
    /// Observes the live resource terms right now.
    ///
    /// `O(live)` with one registry lock and one small `Vec`; an empty set (no NIF ever minted a
    /// resource) allocates nothing.
    pub(crate) fn snapshot() -> Frontier {
        Frontier {
            words: resource_terms().iter().map(|term| term.raw()).collect(),
        }
    }

    /// The words `now` holds that `self` did not — sorted, unique, i.e. **the candidates minted
    /// after this frontier was taken**. Only these may be released.
    fn freshness(&self, now: &Frontier) -> Vec<usize> {
        // Both sides are sorted and unique, so a binary search per word is enough and the result
        // keeps `now`'s order.
        let mut fresh = Vec::new();
        for &word in &now.words {
            if self.words.binary_search(&word).is_err() {
                fresh.push(word);
            }
        }
        fresh
    }
}

/// The two sites' bookkeeping: the `-eval` statement boundary's frontier slot, the counters, and
/// the diagnostic kill switch.
///
/// Only [`prune_at_statement_boundary`] touches `frontier`: a nested site's frontier is a per-run
/// local, because nested runs **recurse** and a runtime-global slot would be clobbered by an inner
/// run.
#[derive(Debug, Clone)]
pub(crate) struct ResourcePruneState {
    /// Whether the resource prune may run at all — see
    /// [`Runtime::set_resource_prune_enabled`].
    pub(crate) enabled: bool,
    /// The candidate set as of the `-eval` route's previous statement boundary. `None` until the
    /// first boundary of a runtime has observed one (which is why that boundary releases nothing).
    pub(crate) frontier: Option<Frontier>,
    /// How many resource terms were live at the most recent observation.
    pub(crate) seen_terms: usize,
    /// How many times a prune actually ran a walk.
    pub(crate) prunes: u64,
    /// How many times a prune declined because soundness could not be shown.
    pub(crate) declines: u64,
    /// The reason of the most recent decline.
    pub(crate) last_decline: Option<&'static str>,
    /// How many resource terms have been released in total (each is one dropped term reference).
    pub(crate) released: u64,
}

impl Default for ResourcePruneState {
    fn default() -> Self {
        ResourcePruneState {
            // On by default: this is a memory reclaim, and a user-facing off switch for one would
            // be a footgun — the switch exists only so a measurement can hold the mechanism out of
            // the picture exactly as `set_eval_prune_enabled` does.
            enabled: true,
            frontier: None,
            seen_terms: 0,
            prunes: 0,
            declines: 0,
            last_decline: None,
            released: 0,
        }
    }
}

/// The resource question's [`Reach`] sink: which of the **fresh** candidates a reachable word names
/// — by its own word (tier 1) or through the reference identity a **copy** of it carries (tier 2).
///
/// The two tiers mirror the process site's walk
/// ([`crate::context::gc_walk::reachable_resource_words`]): `copy_struct` (the send path, an ETS
/// insert, the `-eval` heap copy-out) builds a **different word** for the same object, so a word
/// match alone would call a live resource dead. The candidate recorded in each case is the
/// candidate's **own minted word**, which is what [`release_resource_terms`]' predicate compares.
struct ResourceSink {
    /// The fresh candidates, sorted (the only words that may be answered reachable *or* released).
    candidates: Vec<usize>,
    /// Candidate reference identity → candidate word, sorted by identity (tier 2's index).
    ids: Vec<(u64, usize)>,
    /// Candidate words answered reachable, in visit order (deduplicated when the walk ends).
    reached: Vec<usize>,
}

impl ResourceSink {
    /// Indexes `candidates`; a candidate whose identity cannot be read is put straight into
    /// `reached` — "cannot identify it" must never become "it is dead".
    fn new(candidates: Vec<usize>) -> ResourceSink {
        let mut ids = Vec::with_capacity(candidates.len());
        let mut reached = Vec::new();
        for &word in &candidates {
            match resource_identity(Eterm::from_raw(word)) {
                Some(id) => ids.push((id, word)),
                None => reached.push(word),
            }
        }
        ids.sort_unstable();
        ResourceSink {
            candidates,
            ids,
            reached,
        }
    }

    /// The candidate words a **copy** of which `word` is, if any.
    fn identity_match(&self, word: Eterm) -> Option<usize> {
        let id = resource_identity(word)?;
        let index = self.ids.binary_search_by_key(&id, |&(id, _)| id).ok()?;
        Some(self.ids[index].1)
    }

    /// The sink's two answers, once the walk is over: the candidate list and the reachable subset.
    fn into_parts(mut self) -> (Vec<usize>, Vec<usize>) {
        // The descent has no visited set, so one candidate can be reached many times.
        self.reached.sort_unstable();
        self.reached.dedup();
        (self.candidates, self.reached)
    }
}

impl Reach for ResourceSink {
    fn word(&mut self, term: Eterm) {
        // Tier 1: the candidate's own word. Safe on **any** word (a raw compare), which is why it
        // runs before the descent's skips.
        let raw = term.raw();
        if self.candidates.binary_search(&raw).is_ok() {
            self.reached.push(raw);
            return;
        }
        // Tier 2 reads the word's identity, so it must apply the descent's own guards first: this
        // hook runs *before* them, so an unaligned or literal "pointer" the descent is about to
        // skip would otherwise be dereferenced here.
        if self.ids.is_empty() {
            return;
        }
        if let Some(candidate) = self.identity_match(term) {
            self.reached.push(candidate);
        }
    }
}

/// The **reference identity** of `word` — its id — or `None` when `word` is not a readable
/// internal-reference block.
///
/// The guards are the descent's own, applied *here* because [`Reach::word`] runs before them; a
/// literal-area pointer and an unaligned or null pointer are therefore rejected without a read.
/// [`beam_term::boxed::internal_ref_id`] then validates the tag, the header arity and the id word,
/// so a block of another shape (a C-layout three-word reference, a tuple) is rejected too. A minted
/// block that has been recycled in place can only yield a **different** id — which matches nothing
/// and leaves the candidate's fate to the word match, never to an invented identity.
fn resource_identity(word: Eterm) -> Option<u64> {
    if beam_term::literal::is_literal(word) || !beam_term::boxed::is_boxed_term(word) {
        return None;
    }
    let raw = beam_term::list::ptr_val(word) as usize;
    if raw == 0 || raw % align_of::<Eterm>() != 0 {
        return None;
    }
    // SAFETY: the word is a boxed, non-literal term whose pointer is non-null and word-aligned —
    // the same guard the descent applies before it reads a header. `internal_ref_id` validates the
    // header (tag, arity) before it reads the id word, so a non-reference block is refused rather
    // than read past its end.
    unsafe { beam_term::boxed::internal_ref_id(word) }.ok()
}

/// The candidates `frontier` does not already list — those minted since it was taken.
///
/// An empty answer means there is nothing this site may release, so a caller that must first
/// collect roots can skip that work entirely (that is why this is a separate entry point: a nested
/// site's root collection copies a whole machine register file).
pub(crate) fn fresh_candidates(runtime: &Runtime, frontier: &Frontier) -> Vec<usize> {
    if !runtime.resource_prune.enabled {
        return Vec::new();
    }
    frontier.freshness(&Frontier::snapshot())
}

/// Whether [`prune_beyond`] has anything to consider at this site — the cheap gate a caller that
/// must first collect roots checks.
pub(crate) fn resources_pending(runtime: &Runtime, frontier: &Frontier) -> bool {
    !fresh_candidates(runtime, frontier).is_empty()
}

/// Releases the reference of every live resource term **minted after `frontier`** that nothing
/// reachable from `extra` plus [`Runtime::gather_off_heap_roots`] names.
///
/// `extra` is the caller's own live set (the terms it holds that no runtime field names), and
/// passing it is what makes the root enumeration complete: a caller that cannot name its live terms
/// must not call this.
///
/// # The walk's completeness depends on the root enumeration
///
/// Every family [`Runtime::gather_off_heap_roots`] hands over is assumed complete for the *stores it
/// claims*. The one shape that makes this a live requirement is the exactly-mint-and-schedule case —
/// `timer:apply_after(T, M, F, [enif_make_resource(...)])`, one statement, the resource named only
/// by a pending timer message — because such a term is named by no heap region and no machine: it is
/// the pending-timer store's, so that store must be in the enumeration (a sibling change to that
/// function; this site inherits whichever set it publishes).
pub(crate) fn prune_beyond(runtime: &mut Runtime, extra: &[Eterm], frontier: &Frontier) -> Prune {
    prune_beyond_observed(runtime, extra, frontier, Frontier::snapshot())
}

/// [`prune_beyond`] with the "now" observation already taken, so a caller that had to snapshot
/// anyway (the statement boundary, which stores it as the next frontier) pays for exactly one.
fn prune_beyond_observed(
    runtime: &mut Runtime,
    extra: &[Eterm],
    frontier: &Frontier,
    now: Frontier,
) -> Prune {
    // Gate 1: the diagnostic kill switch, checked before anything is observed so turning it back on
    // starts from a clean slate.
    if !runtime.resource_prune.enabled {
        return Prune::Deferred;
    }
    // Gate 2: the `O(1)` steady state. With no live resources both sides are empty and this is one
    // length check.
    runtime.resource_prune.seen_terms = now.words.len();
    if now.words == frontier.words {
        return Prune::Deferred;
    }
    // Gate 3: live resources exist, but none was minted inside this site's window — the only
    // candidates that may be released. No walk runs.
    let fresh = frontier.freshness(&now);
    if fresh.is_empty() {
        return Prune::Deferred;
    }
    // The root set: the caller's live terms, then every family the runtime enumerates.
    let mut roots = Vec::with_capacity(extra.len() + 32);
    roots.extend_from_slice(extra);
    runtime.gather_off_heap_roots(&mut roots);
    let mut sink = ResourceSink::new(fresh);
    // Gate 4: a walk that could not finish releases nothing (see the module docs: the observed
    // frontier still advances, so a persistent decline costs one walk rather than one per call).
    let verdict = walk(&roots, &mut sink);
    if !verdict.is_complete() {
        let reason = verdict.reason();
        runtime.resource_prune.declines += 1;
        runtime.resource_prune.last_decline = Some(reason);
        crate::debug_trace::trace(format_args!(
            "resource prune declined: {reason} ({} live term(s))",
            now.words.len()
        ));
        return Prune::Declined(reason);
    }
    let (candidates, reached) = sink.into_parts();
    // A candidate outside the fresh set is **retained** (it predates the site: a store this walk
    // cannot see may hold it); a fresh candidate is released only when nothing reached names it.
    let released = release_resource_terms(|term| {
        let word = term.raw();
        candidates.binary_search(&word).is_err() || reached.binary_search(&word).is_ok()
    });
    runtime.resource_prune.prunes += 1;
    runtime.resource_prune.released += released as u64;
    runtime.resource_prune.last_decline = None;
    runtime.resource_prune.seen_terms = now.words.len().saturating_sub(released);
    if released > 0 {
        crate::debug_trace::trace(format_args!(
            "resource prune released {released} of {} live resource term(s)",
            now.words.len()
        ));
    }
    Prune::Pruned {
        released,
        retained: candidates.len().saturating_sub(released),
    }
}

/// The **`-eval` statement boundary**'s resource prune: prune against the frontier the previous
/// boundary observed, then record this boundary's observation as the next frontier.
///
/// * the **first** boundary of a runtime (no frontier recorded yet) observes and releases
///   **nothing**: a resource minted before this expression began may be held by a host store the
///   walk cannot see, so there is no evidence yet (a bounded one-statement leak, documented);
/// * the frontier advances on **every** boundary, including a declined prune — so a candidate a
///   permanent decline keeps alive is retained for the runtime's life instead of costing a full walk
///   on every subsequent statement;
/// * the kill switch is checked first, so while it is off nothing is observed and turning it back on
///   starts from a clean slate.
pub(crate) fn prune_at_statement_boundary(runtime: &mut Runtime, extra: &[Eterm]) -> Prune {
    if !runtime.resource_prune.enabled {
        return Prune::Deferred;
    }
    let now = Frontier::snapshot();
    let Some(prior) = runtime.resource_prune.frontier.replace(now.clone()) else {
        runtime.resource_prune.seen_terms = now.words.len();
        return Prune::Deferred;
    };
    prune_beyond_observed(runtime, extra, &prior, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::prelude::make_small;
    // Only the refusal tests name the verdict type: the descent itself answers through
    // `Walk::is_complete`, so the production half imports nothing extra.
    use crate::eval::reclaim::Walk;

    /// A frontier over the given words (a helper so the diff's semantics can be tested without a
    /// registry: the words are opaque to [`Frontier`], which only orders and compares them).
    fn frontier(words: &[usize]) -> Frontier {
        let mut words = words.to_vec();
        words.sort_unstable();
        words.dedup();
        Frontier { words }
    }

    /// An `[hdr][node][serial][id]` internal-reference block on `heap`, the shape `beam_abi`'s
    /// resource mint builds.
    fn internal_ref(heap: &mut beam_term::heap::Heap, id: u64) -> Eterm {
        // SAFETY: `alloc_words` reserves the block's four words, which `write_internal_ref` fully
        // writes, and the heap outlives the returned term here.
        unsafe {
            let hp = heap
                .alloc_words(beam_term::boxed::INTERNAL_REF_WORDS)
                .expect("the test heap has room for one reference block");
            beam_term::boxed::write_internal_ref(hp, id).expect("the id fits the block's slot")
        }
    }

    /// The freshness diff is a sorted difference: only words the frontier does not list, and an
    /// unchanged or shrinking set yields nothing at all (the `O(1)` steady state).
    #[test]
    fn freshness_is_the_sorted_difference() {
        let before = frontier(&[10, 30]);
        assert_eq!(before.freshness(&frontier(&[10, 30])), Vec::<usize>::new());
        assert_eq!(
            before.freshness(&frontier(&[10])),
            Vec::<usize>::new(),
            "a shrinking set yields nothing: no word it holds is new"
        );
        assert_eq!(
            before.freshness(&frontier(&[10, 20])),
            vec![20],
            "a word the frontier did not list is fresh even when another candidate vanished"
        );
        assert_eq!(before.freshness(&frontier(&[10, 20, 30, 40])), vec![20, 40]);
        assert_eq!(
            Frontier::default().freshness(&frontier(&[7, 5])),
            vec![5, 7],
            "an empty frontier makes every candidate fresh"
        );
    }

    /// A snapshot keeps the registry's sorted, unique order, and a default frontier lists nothing.
    #[test]
    fn a_snapshot_is_sorted_and_unique() {
        let snapshot = Frontier::snapshot();
        assert!(
            snapshot.words.windows(2).all(|pair| pair[0] < pair[1]),
            "the snapshot keeps the registry's sorted, unique order"
        );
        assert!(
            Frontier::default().words.is_empty(),
            "a default frontier lists nothing"
        );
    }

    /// Tier 1: a candidate's **own** word is recorded reachable, and an unrelated word is not.
    #[test]
    fn the_sink_matches_a_candidate_by_its_own_word() {
        let mut heap = beam_term::heap::Heap::new(32);
        let candidate = internal_ref(&mut heap, 7);
        let other = internal_ref(&mut heap, 8);
        let mut sink = ResourceSink::new(vec![candidate.raw()]);
        sink.word(candidate);
        let (candidates, reached) = sink.into_parts();
        assert_eq!(candidates, vec![candidate.raw()]);
        assert_eq!(reached, vec![candidate.raw()]);
        assert!(
            !reached.contains(&other.raw()),
            "a word that is not a candidate is never recorded"
        );
    }

    /// Tier 2: a **copy** of a candidate is a different word with the same identity, and it keeps
    /// the candidate alive (the send path / ETS / copy-out shape).
    #[test]
    fn the_sink_matches_a_copy_of_a_candidate_by_identity() {
        let mut heap = beam_term::heap::Heap::new(32);
        let minted = internal_ref(&mut heap, 42);
        let copy = internal_ref(&mut heap, 42);
        assert_ne!(minted.raw(), copy.raw(), "the copy is a distinct block");
        let mut sink = ResourceSink::new(vec![minted.raw()]);
        sink.word(copy);
        let (_, reached) = sink.into_parts();
        assert_eq!(
            reached,
            vec![minted.raw()],
            "the *candidate* is recorded, not the copy's word"
        );
        // A reached copy of a *different* identity must not be folded onto this candidate.
        let unrelated = internal_ref(&mut heap, 43);
        let mut sink = ResourceSink::new(vec![minted.raw()]);
        sink.word(unrelated);
        let (_, reached) = sink.into_parts();
        assert!(reached.is_empty(), "a different identity matches nothing");
    }

    /// A candidate whose identity cannot be read is treated as **reached** — never free what cannot
    /// be identified.
    #[test]
    fn an_unreadable_candidate_is_treated_as_reached() {
        let mut heap = beam_term::heap::Heap::new(32);
        // A block of another shape (a tuple) stands in for a recycled minted block: it is not an
        // internal reference, so its identity cannot be read.
        let not_a_ref = heap.tuple(&[make_small(1)]).expect("the tuple");
        let sink = ResourceSink::new(vec![not_a_ref.raw()]);
        let (candidates, reached) = sink.into_parts();
        assert_eq!(candidates, vec![not_a_ref.raw()]);
        assert_eq!(
            reached,
            vec![not_a_ref.raw()],
            "an unreadable identity answers reached, so the candidate is retained"
        );
    }

    /// The sink's `word` hook runs before the descent's guards, so it must not dereference a word
    /// those guards would skip: an immediate is inert, and so is a plain word.
    #[test]
    fn the_sink_ignores_an_immediate() {
        let mut sink = ResourceSink::new(Vec::new());
        sink.word(make_small(0));
        let (candidates, reached) = sink.into_parts();
        assert!(candidates.is_empty() && reached.is_empty());
        assert_eq!(resource_identity(make_small(0)), None);
    }

    /// A decline is reported by the shared descent, and the resource sink does not change the
    /// descent's unknown-header rule: a header word no constructor writes declines the walk (so
    /// nothing is released).
    #[test]
    fn a_walk_meeting_an_unknown_boxed_header_declines() {
        let mut heap = beam_term::heap::Heap::new(32);
        // SAFETY: `alloc_words` reserves a four-word block; only its header word is written, which
        // is the only word the descent reads before deciding it does not know the shape.
        let bogus = unsafe {
            let hp = heap.alloc_words(4).expect("the test heap has room");
            *hp = beam_term::boxed::make_header(1, 0x3C);
            beam_term::boxed::make_boxed(hp)
        };
        let mut sink = ResourceSink::new(Vec::new());
        let verdict = walk(&[bogus], &mut sink);
        assert!(
            matches!(verdict, Walk::UnknownHeader { .. }),
            "an unknown boxed header declines the walk"
        );
        assert_eq!(
            verdict.header(),
            Some(beam_term::boxed::make_header(1, 0x3C).raw()),
            "and the verdict names the header word it refused on"
        );
        assert_eq!(
            verdict.reason(),
            "the reachability walk met an unknown boxed header",
            "the recorded reason string is unchanged"
        );
    }

    /// The `O(1)` steady state and the kill switch: with no live resource terms a prune is deferred
    /// without a walk, and switching the mechanism off defers before anything is observed.
    #[test]
    fn prune_beyond_defers_with_nothing_fresh() {
        let mut runtime =
            crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
                .expect("the runtime");
        let empty = Frontier::default();
        assert_eq!(
            prune_beyond(&mut runtime, &[], &empty),
            Prune::Deferred,
            "two empty snapshots are the steady state — one length check, no walk"
        );
        // No NIF mints a resource in this test, so the fresh set is empty and the root collection a
        // nested site would pay for is skipped outright.
        assert!(fresh_candidates(&runtime, &empty).is_empty());
        assert!(!resources_pending(&runtime, &empty));
        runtime.set_resource_prune_enabled(false);
        assert_eq!(
            prune_at_statement_boundary(&mut runtime, &[]),
            Prune::Deferred,
            "the kill switch defers, and records no frontier while it is off"
        );
        assert_eq!(
            prune_beyond(&mut runtime, &[], &empty),
            Prune::Deferred,
            "the kill switch defers before anything is observed"
        );
        let stats = runtime.resource_prune_stats();
        assert_eq!((stats.prunes, stats.declines, stats.released), (0, 0, 0));
    }
}