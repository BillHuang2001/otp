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

//! The conservative **reachability walk** behind the unscanned-root valve.
//!
//! [`super::gc::root_is_unscanned`] classifies a *single* word: it answers `true` when that
//! word names a container in a region the process's collector does not scan. The valve that
//! guards every collection site must ask the stronger question a **nested** graph asks — is an
//! unscanned container reachable from a *scanned* one? The shape that defeats the per-word test
//! is exactly one level of indirection deep: a young-heap list whose element is a container
//! built on the runtime's pooled BIF heap (`Runtime::take_bif_heap`), whose element is a
//! pointer into the process's nursery. The root's own top-level address is scanned, so the
//! per-word test calls it safe — yet relocating the nursery leaves the BIF-heap container's
//! child pointing into the evacuated from-space, and the next dereference of it reads a dead
//! word.
//!
//! # The walk
//!
//! [`any_root_is_unscanned`] answers that question in two phases:
//!
//! 1. **The cheap, `O(roots)` classification** (`Walk::seed`, the old single-level valve): every
//!    word the collection is about to publish is tested once with [`root_is_unscanned`]. A root
//!    that is *itself* an unscanned container declines the collection here, without any descent —
//!    the common direct case, at the cost the valve always had.
//! 2. **The descent** — an **iterative worklist** walk, run **only** when a caller-supplied
//!    `descend` gate allows it (see [`any_root_is_unscanned`]):
//!
//!    * it seeds from the machine roots the caller is about to publish (the engine's x registers,
//!      stack and FR file) plus every container `Process::collect` gathers — the registers, the
//!      mailbox, the dictionary, the saved arguments and the pending-signal queue;
//!    * it descends **only** into objects that live in the scanned region (a term the per-word
//!      test calls safe, plus the existing "the object is a container" test), so a word no copier
//!      would follow is never dereferenced as an object header;
//!    * it reports a hit as soon as any reachable child word is a container in a region the
//!      collector does not scan, and otherwise runs to exhaustion.
//!
//! A `visited` set of object addresses makes a shared or cyclic graph terminate: each object
//! is expanded at most once, and the walk is therefore `O(distinct live objects)` — the same
//! order as the collection it is deciding, which is the only time it runs.
//!
//! # The descent gate (why the walk usually does not run at all)
//!
//! The descent is `O(live objects)` and ran on *every* candidate collection, which made a
//! long-lived allocating loop pay a full graph traversal per collection (see the
//! [`any_root_is_unscanned`] caller). It is therefore gated: the caller passes `descend`, which it
//! computes from the process's **foreign-reachability flag**
//! ([`crate::runtime::gc::GcState`]). The flag is a **sticky, add-only** per-process bit, set by
//! every event that can put a container from an unscanned region into the process's reachable
//! graph (a registered-BIF result built outside the scanned region, the arena fallback of the
//! allocation seams, the escape-store stabilisation, a delivered message / retained mailbox
//! fragment, and a spawn's initial arguments or entry fun).
//!
//! Soundness of the gate (this is the whole argument, and it is a *one-way* argument — a wrong
//! `false` would resurrect the corruption the walk exists to prevent):
//!
//! * when `descend` is `false`, **phase 1 still runs**. A foreign container reachable at depth 1
//!   is a root word, so phase 1 catches it exactly as it always did — the gate removes only the
//!   *descent*, never the per-word classification.
//! * for the descent to matter, a foreign container must be reachable at depth ≥ 2, i.e. it must
//!   be a **child of a scanned container**. A pointer to a foreign container can only be written
//!   into a scanned container by code that *holds* that foreign container, and the only ways to
//!   hold one are the events listed above — each of which sets the flag. A foreign container can
//!   likewise only be a *root word* after one of those events put it there.
//! * the flag is only ever **set**, never cleared while the process lives (and the per-process
//!   entry is pruned only when the process itself is gone), so a flag observed `true` at one
//!   collection stays `true` at every later one.
//!
//! # Why there is no "close the latch again" fast path (analysed, **not** implemented)
//!
//! The descent's cost is its **invocation count**: the baseline JIT's `jit_test_heap` reaches the
//! valve at roughly every heap test, so a real compile pays a walk per heap-admission point
//! (hundreds of thousands of them — see [`seed_counters`]' measurement test). The obvious next
//! step is therefore to *re-close* the per-pid latch after an exhaustive descent that found no
//! foreign container, so a loop that settles into a foreign-free steady state pays the descent once
//! instead of once per admission point.
//!
//! **That is unsound as posed.** The refutation is one sentence: *a descent is exhaustive only over
//! the roots it was handed, and one of the two callers does not have all of them.* The runner's park
//! arm ([`process_roots_are_unscanned`]) and the collection gate ([`RuntimeCollectionGate`]) run for
//! a process that is **not** the running one, whose `x`/stack/FR words are unpublished — and an
//! *exempt* park ([`Process::has_unpublished_roots`], the `Wait` arm and a `Yield` whose slice ended
//! with a live FR prefix) publishes **nothing at all** (`src/runner/run.rs::park_machine` returns
//! early). Concrete counterexample:
//!
//! 1. the process holds a container from an unscanned region (the bit opens) and stores it into a
//!    scanned young tuple `T` held **only** in `x[1]`; no admission point has run since;
//! 2. it parks on `receive` — an exempt park, so `x[1]` is not in the gathered roots;
//! 3. a signal site (or the parked process's own `Process::deliver`) runs
//!    [`RuntimeCollectionGate::declines_relocation`]: the descent sees the gathered roots only,
//!    reaches nothing foreign, and (hypothetically) closes the bit;
//! 4. the receiver resumes; `x[1] = T` is published at the next admission point, the bit is clear,
//!    the descent is skipped — and the per-word phase classifies `T` as a *scanned* container, so
//!    the collection proceeds and leaves the foreign container's child pointing into the evacuated
//!    from-space.
//!
//! What a sound version would need, and where each piece lives — **none of it in this file**:
//!
//! * a "the descent saw the *whole* root set" bit, set only by the admission-point walks, whose
//!   root set is the machine's published `RootSet` (`super::gc::ensure_heap*`) — a clear on any
//!   other walk (the gate's, the park arm's) is the bug above;
//! * the **clear site**: the latch itself is only an alias here ([`ForeignLatch`]); the real cell is
//!   `crate::runtime::gc::GcState::foreign`, written by `Runtime::note_foreign_reachable`;
//! * a **completeness proof for that writer**: every event that can put a foreign container into the
//!   reachable graph must open the bit. Today the bit is *add-only*, so a missed event costs only a
//!   descent; with a clear, a missed event is a memory-safety bug. The known suspects are the ones
//!   `M1_GAPS.md`'s *GC-relocation hazards* roster names as pinned outside the root set — a
//!   `beam_bifs` fast-path BIF's Rust-local accumulator, the BIF-result copy-out slot, the
//!   `beam_engine` `PendingException` reason — an audit across `beam_bifs`/`beam_engine`;
//! * an **ordering** guarantee: a set that happens while (or just before) the clear runs must not be
//!   lost, i.e. the clear must be a compare-and-swap against the state the walk started from.
//!
//! **Verdict: sound-but-cross-crate at best, and unsound as posed.** The pieces above are owned by
//! `super::gc`, `src/runtime/gc.rs`, `src/runner/` and `beam_process`'s signal sites; nothing in
//! this file can make the skip safe on its own, so it is deliberately **not** implemented here. What
//! this file does own — the per-walk allocation — is fixed instead (see [`WalkScratch`]).
//!
//! # The same valve installed as the `beam_process` collection gate
//!
//! The valve's host call sites — the three heap-admission points in [`super::gc`] and the
//! runner's park arm — all hold a [`Runtime`](crate::runtime::Runtime), so they can consult
//! [`any_root_is_unscanned`] directly and gate the descent on the per-pid latch. The
//! `beam_process` **signal** sites — a monitor `'DOWN'`, an exit propagation and an applied
//! trap reason — run inside [`ProcessTable`](beam_process::ProcessTable) with only
//! `&mut ProcessTable` available, and the rust-level `Process` they *can* reach exposes only the
//! crate's own [`Process::has_unpublished_roots`] test (relocation-exempt, or
//! [`Running`](beam_process::ProcessState::Running)). A relocating `ensure_capacity` on a
//! **waiting** receiver — the woken-but-not-yet-resumed shape the A.9 `selfhost/*` corruption
//! went through — is therefore neither exempt nor `Running`, and moved a heap this valve would
//! have refused.
//!
//! The crate's [`beam_process::CollectionGate`] seam is the host's half of that decision:
//! [`RuntimeCollectionGate`] implements it by running **this very walk** over the receiver's
//! gathered roots, and [`Runtime::new`](crate::runtime::Runtime::new) installs one on the
//! runtime's process table. The gate keeps its **own** scratch — never the runtime's
//! admission-point one, because a signal site may run while a machine is mid-collection — and
//! it is built over the host's **one** per-pid latch cell ([`ForeignLatch`]), so a signal site
//! decides exactly as an admission point does and costs `O(1)` while the pid's bit is clear.
//! Because the gate is a *table* field, it travels with the table into
//! every scheduler pass ([`Runtime::take_processes`](crate::runtime::Runtime)) and covers every
//! process the table holds — including one spawned from inside a pass — so all signal sites
//! (`ptab.rs`'s `declines_signal_collection`) and
//! [`ProcessTable::deliver`](beam_process::ProcessTable) decide with the *same* predicate the
//! admission points use. One valve, never two that could drift.
//!
//! # Why no boxed kind can be missed
//!
//! The per-object child enumeration is the canonical
//! [`beam_term::layout::for_each_child_slot`] — the same model the workspace's own copier uses
//! (`beam_message::copy`) — so the walk can never disagree with the collector about which
//! payload words of an object are references. Two shapes need no descent at all and are
//! deliberately skipped:
//!
//! * an **immediate** (a small integer, an atom, `NIL`, an internal pid/port id) has no
//!   children;
//! * a **literal-area pointer** ([`beam_term::literal::is_literal`]) is a frozen code constant
//!   whose words reference the literal area alone, never a runtime heap region, so it can
//!   neither dangle itself nor lead to a word that does. Skipping it is also what keeps a
//!   *nested* pointer inside a literal (which carries no literal bit of its own) from being
//!   mis-read as an unscanned heap container.
//!
//! # Why this module is `unsafe`
//!
//! The walk must read an object's header word and its payload words at raw addresses; that is
//! the one operation `beam_term` does not expose through a safe API (its own safe entry point,
//! [`beam_term::size::size_object`], reports only a word *count*, never the child words a
//! classifier needs). The crate is `#![deny(unsafe_code)]`, so this module carries the
//! documentedly narrow `#![allow(unsafe_code)]` the workspace's `unsafe` policy sanctions for
//! the term-heap/GC category — `src/nif.rs` and the `gc.rs` unit tests take the same targeted
//! exception — and every raw access is confined to the single `expand` routine below, under a
//! `// SAFETY:` rationale naming the invariant that makes it sound: a word reaches the
//! worklist only through `seed`, which admits exactly the boxed/cons words addressed inside
//! the region the process's collector scans, i.e. live objects of the running process.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use beam_process::{Process, ProcessId};
use beam_term::layout::for_each_child_slot;
use beam_term::list::ptr_val;
use beam_term::literal::is_literal;
use beam_term::prelude::{is_boxed, is_list};
use beam_term::Eterm;

use super::gc::{forwards_scanned_word, root_is_unscanned_with, word_region, WordRegion};
use beam_gc::ScannedBounds;

// ============================ END TEMPORARY POLICY SWITCH ============================

/// The **reused** mutable state of the descent: its worklist, its visited set and the root buffer
/// the appending [`Process`] sub-rootsets are gathered into.
///
/// One lives in the runtime's GC state ([`crate::runtime::gc::GcState`]) and is handed to every
/// walk, so a collection **never allocates** the descent's structures (see [`Visited`] for the
/// `O(1)` per-walk reset that makes the reuse free). All three buffers only ever grow, so the
/// steady state of a running process is allocation-free after its first few walks.
///
/// The third buffer is load-bearing for that claim: three of the six root-set families
/// ([`Registers`](beam_process::Registers), [`SavedArgs`](beam_process::SavedArgs),
/// [`SignalQueue`](beam_process::SignalQueue)) *append into a caller buffer* — their
/// `gather_roots` API takes a `&mut Vec<Eterm>` and returns a start offset — so the seeding needs
/// a buffer for them. A fresh `Vec::new()` per call was the one allocation that broke the
/// module's "allocation-free after warm-up" property, and it was paid on **every** walk (the
/// walk runs once per heap-admission point, i.e. hundreds of thousands of times over a single
/// in-process stdlib compile). The buffer is reused instead: [`Walk::seed_process_roots`] takes
/// it with a 3-word [`std::mem::take`] and puts it back before returning, so a walk neither
/// allocates nor retains a borrow of the scratch.
///
/// The other three families are seeded **straight from their iterators**
/// ([`Queue::iter`](beam_message::Queue::iter) for the mailbox envelope,
/// [`Dict::entries`](beam_process::Dict::entries) for the dictionary), so they need no buffer at
/// all and never touch this one.
#[derive(Default)]
pub(crate) struct WalkScratch {
    /// The objects still to expand (the worklist).
    pending: Vec<Eterm>,
    /// The objects already entered.
    visited: Visited,
    /// The root buffer the appending sub-rootsets are gathered into — taken and returned by
    /// [`Walk::seed_process_roots`], so it never re-allocates once warmed up.
    roots: Vec<Eterm>,
}

/// What one valve call over one root set answered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ValveVerdict {
    /// A container in a region the collector does not scan, **forwards a word into one it does**,
    /// is reachable from the roots — so a collection would leave that word pointing into an
    /// evacuated from-space and must be declined.
    pub(crate) declined: bool,
    /// The descent ran (the caller's gate was open) and reached **no** container in a region the
    /// collector does not scan at all.
    ///
    /// This is deliberately the *stronger* of the two clean answers: "no foreign container is
    /// reachable" makes a hazard impossible without appealing to the forwarding test's precision,
    /// so a caller that handed over its **whole** root set may close the process's descent latch
    /// on it ([`crate::runtime::gc::GcState::foreign`]). A caller that handed over less (the
    /// narrow admission door, the park arm, the collection gate) must ignore it.
    pub(crate) foreign_free: bool,
}

/// Whether a collection of `process` would be **unsound** — whether a container in a region
/// the collector does not scan, forwarding a word into one it does, is reachable from one it does.
///
/// `machine_roots` are the words the caller is about to publish (the engine's x registers, the
/// whole Erlang stack, and each register file at its own **pointer-bearing prefix** — the FR file
/// reaches this walk up to `fr_count`, not whole); the process's **own** root-set containers are
/// always included, because [`Process::collect`] gathers them on every collection. Passing an
/// over-approximation of the roots is always allowed: a spurious hit only **declines** a
/// collection that might have been sound (the allocation then falls back to the pointer-stable
/// arena), never allowing one that is not.
///
/// `descend` is the caller's **foreign-reachability gate** (this module's docs): when it is
/// `false` the descent is skipped and only the cheap `O(roots)` per-word phase runs. The gate can
/// only ever cost a collection, never allow a wrong one — and a gate that is *open* is closed
/// again by a [verdict](ValveVerdict) that reached no foreign container at all.
pub(crate) fn any_root_is_unscanned<'a, I>(
    scratch: &mut WalkScratch,
    process: &Process,
    machine_roots: I,
    descend: bool,
) -> bool
where
    I: IntoIterator<Item = &'a Eterm>,
{
    valve_verdict(scratch, process, machine_roots, descend).declined
}

/// [`any_root_is_unscanned`], reporting **both** halves of the valve's answer so an admission
/// point that handed over its whole root set can also close the process's descent latch.
pub(crate) fn valve_verdict<'a, I>(
    scratch: &mut WalkScratch,
    process: &Process,
    machine_roots: I,
    descend: bool,
) -> ValveVerdict
where
    I: IntoIterator<Item = &'a Eterm>,
{
    // The collector's aggregate geometry is read **once per call** and handed down to every
    // per-word classification: hygiene, not a claimed win (the whole valve is 0.40 % of samples
    // in a `beam_gc` peer's profile), but it keeps one decision on one geometry.
    let bounds = process.collector().scanned_bounds();
    let mut walk = Walk::begin(scratch, bounds);
    for &word in machine_roots {
        walk.seed(process, word);
        if walk.hit {
            return ValveVerdict {
                declined: true,
                foreign_free: false,
            };
        }
    }
    walk.seed_process_roots(process);
    if !walk.hit && descend {
        walk.run(process);
    }
    ValveVerdict {
        declined: walk.hit,
        // Only a descent may claim the graph is foreign-free: the per-word phase looked at the
        // roots alone, and a foreign container nested in a scanned one is exactly what it cannot
        // see.
        foreign_free: descend && !walk.foreign_reached,
    }
}

/// The [`Process`]-only form of [`any_root_is_unscanned`], for a collection site whose machine
/// roots are **already published** into the process's own root set — the runner's park arm,
/// where `publish_parked_roots` runs immediately before
/// [`Runtime::serve_gc_request`](crate::runtime::Runtime).
pub(crate) fn process_roots_are_unscanned(
    scratch: &mut WalkScratch,
    process: &Process,
    descend: bool,
) -> bool {
    any_root_is_unscanned(scratch, process, std::iter::empty::<&Eterm>(), descend)
}

/// The **one** per-runtime cell holding the pids whose descent latch is open.
///
/// The latch is the host's sticky, add-only per-process bit: it is set by every event that can
/// put a container from a region the process's collector does not scan into that process's
/// reachable term graph, and set again (never cleared) by the valve itself on a root hit. The
/// runtime owns the one cell and shares it by cloning the `Rc` at the collection gate's install
/// site, so a `beam_process` signal site decides exactly as an admission point does and never
/// pays the `O(live objects)` descent for a pid whose bit is clear. It is a **verdict-only** set
/// of internal pids, so it probes with the crate's own deterministic
/// [`FastBuildHasher`](crate::hasher::FastBuildHasher) instead of `RandomState`'s SipHash: a
/// membership answer is hasher-independent, and this cell is never iterated for an observable
/// ordered result (see the `hasher` module docs for the full soundness argument).
pub(crate) type ForeignLatch = Rc<RefCell<HashSet<ProcessId, crate::hasher::FastBuildHasher>>>;

/// The host side of the [`beam_process::CollectionGate`] seam: it answers a signal site's
/// "may this receiver's heap be relocated?" question with **this module's** reachability walk,
/// so a signal-driven relocation and an admission-point one decide identically.
///
/// [`Runtime::new`](crate::runtime::Runtime::new) installs one on the runtime's process table;
/// see the module docs' *the same valve installed as the `beam_process` collection gate* for
/// why the signal sites need it at all.
///
/// # Borrowing
///
/// The gate is owned by the [`ProcessTable`](beam_process::ProcessTable), and a signal site
/// holds nothing but `&mut ProcessTable`, so the gate must be self-sufficient: it **owns its own
/// [`WalkScratch`]** and drives it through `&mut self` in
/// [`declines_relocation`](beam_process::CollectionGate::declines_relocation). The scratch is a
/// plain struct of two `Vec`s that only ever grow, so after warm-up a decision allocates
/// nothing. The gate's **only** shared handle is the host's per-pid latch cell
/// ([`ForeignLatch`]) — an `Rc<RefCell<…>>` the gate holds a clone of, never a handle into the
/// runtime's own state — and no `unsafe` is used; the crate stays `#![deny(unsafe_code)]`
/// (this module's narrow, documented `#![allow(unsafe_code)]` covers only the raw object read
/// in [`Walk::expand`]).
///
/// The scratch is a **second, independent** instance — never the runtime's admission-point one
/// ([`GcState::walk`](crate::runtime::gc::GcState)): a signal site runs from a different borrow
/// than an admission point and may even run while a machine is mid-collection, so aliasing the
/// two would be a borrow violation. Two instances cost one extra `Vec` pair and no correctness.
///
/// # The descent gate on a signal site
///
/// A signal site has **no in-flight machine**: the receiver is not the running process, so there
/// is no machine root set to publish and the only roots the walk sees are the ones
/// [`Process::collect`] gathers. The gate therefore holds a clone of the runtime's **one** latch
/// cell and applies the **same one-way rule** the four admission doors and `send.rs`'s
/// `delivery_collection_is_unsound` apply:
///
/// * a **clear** bit means no container from an unscanned region has ever entered this process's
///   reachable graph, so the descent can find nothing: the gate answers `false` **without
///   descending** — `O(1)` per signal;
/// * an **open** bit descends, and a hit declines the relocation.
///
/// Being *conservative* is what makes the `false` answer sound, and the argument is one
/// paragraph: the walk's per-word phase always runs, so a foreign container reachable at depth 1
/// is still caught; for the descent to matter a foreign container must be a child of a *scanned*
/// container, and such a pointer can only be written by code that holds that foreign container —
/// every way of holding one (an arena fallback, a BIF/floor/NIF result outside the process heap,
/// a delivered or received fragment, a spawn argument, the escape store) opens the bit. So a
/// clear bit cannot hide one. And declining a relocation is never wrong: the notification is
/// still delivered, through the heap-fragment route the `Running`/relocation-exempt receiver
/// already takes (a byte-faithful copy), so over-declining can only route a delivery to a
/// fragment, never lose it or fabricate a value.
///
/// A gate built with [`RuntimeCollectionGate::new`] holds **no** latch — there is no host cell to
/// consult — so it always descends. That is the conservative form for a context with no host
/// latch (and it is what the unit suites exercise).
pub(crate) struct RuntimeCollectionGate {
    /// The descent scratch this gate owns; never shared with the runtime's admission-point
    /// scratch (see the type docs).
    scratch: WalkScratch,
    /// The host's shared per-pid descent latch, when this gate was built over one; `None` for a
    /// gate with no host latch to consult (see the type docs).
    latch: Option<ForeignLatch>,
}

#[cfg(test)]
impl RuntimeCollectionGate {
    /// A gate with an empty (allocated lazily, on the first decision) descent scratch and **no**
    /// host latch, so every decision descends unconditionally.
    pub(crate) fn new() -> RuntimeCollectionGate {
        RuntimeCollectionGate {
            scratch: WalkScratch::default(),
            latch: None,
        }
    }
}

#[cfg(test)]
impl Default for RuntimeCollectionGate {
    fn default() -> RuntimeCollectionGate {
        RuntimeCollectionGate::new()
    }
}

impl From<ForeignLatch> for RuntimeCollectionGate {
    /// A gate that reads the host's shared per-pid latch cell — the form the runtime's install
    /// site builds, so a signal site's decision is `O(1)` while the pid's bit is clear.
    fn from(latch: ForeignLatch) -> RuntimeCollectionGate {
        RuntimeCollectionGate {
            scratch: WalkScratch::default(),
            latch: Some(latch),
        }
    }
}

impl beam_process::CollectionGate for RuntimeCollectionGate {
    /// `true` when the receiver's gathered roots reach a container in a region its collector
    /// does not scan — [`process_roots_are_unscanned`], reached only for a pid whose descent latch
    /// is open (see the type docs).
    fn declines_relocation(&mut self, receiver: &Process) -> bool {
        // The pid's bit is the O(1) fast path: a CLEAR bit means no container from an unscanned
        // region has ever entered this process's reachable graph, so the descent can find nothing
        // and is skipped outright. The walk's always-run per-word phase still classifies every
        // root, so a foreign container reachable at depth 1 is caught exactly as before.
        let descend = match &self.latch {
            Some(latch) => latch.borrow().contains(&receiver.pid()),
            None => true,
        };
        if !descend {
            return false;
        }
        process_roots_are_unscanned(&mut self.scratch, receiver, true)
    }
}

#[cfg(test)]
impl RuntimeCollectionGate {
    /// How many descents this gate has run — a test-only witness (the reused scratch's walk
    /// generation), so a case can prove a closed latch skipped the descent rather than merely
    /// observing a `false` answer.
    pub(super) fn descents(&self) -> u32 {
        self.scratch.visited.generation
    }
}

/// The walk's mutable state: the reused scratch, the scanned geometry the whole walk classifies
/// against, and whether a hazard (a hit) or any foreign container at all has been reached.
struct Walk<'a> {
    scratch: &'a mut WalkScratch,
    /// The collector's aggregate scanned geometry, read once by the caller
    /// ([`valve_verdict`]) and reused for every word of this walk.
    bounds: ScannedBounds,
    /// A hazard was reached: a foreign container forwarding a scanned word — or a root that is
    /// one.
    hit: bool,
    /// A container in a region the collector does not scan was reached at all (hazardous or not).
    /// This is what an admission point that handed over its whole root set needs in order to
    /// **close** the descent latch again (see [`ValveVerdict::foreign_free`]).
    foreign_reached: bool,
}

/// The descent's **visited set**: the object addresses already expanded.
///
/// The descent runs over the process's whole live object graph (thousands of objects) on the
/// collections that reach it, so the set's per-key cost is the descent's dominant term. It is
/// therefore deliberately *not* a `HashSet`: it is an **open-addressing table keyed directly on
/// the object address** — a word-aligned term pointer, mixed by [`Visited::probe`] — and every
/// slot carries a **generation tag**, so a fresh walk "clears" the table in `O(1)` (one counter
/// increment) instead of touching one word per slot. That is what makes reusing the table across
/// walks free.
///
/// Each slot is **one 16-byte [`Slot`]** — a key plus its generation — rather than a key array
/// beside a generation array, so a probe touches one cache line instead of two arrays.
///
/// The table is never shed. Right-sizing it (shrinking it back once a deep walk's live set stops
/// being reached) was implemented and measured: it does halve the table's cache traffic, but a
/// smaller table means a higher load factor, and on the `timer.erl` compile workload that raised
/// the walk's mean probes per insert from 1.42 to 1.69, costing ~9 % more cycles end to end — the
/// table is memory-bound but with *low* load, so the probe count dominates the footprint.
///
/// # Cost
///
/// Per walk the table is **near-optimal**: ~1.25–1.45 probes per insert on the real compile
/// workloads, ~5 grows over a whole `pool.erl` compile, and the walk is **memory-bound** rather
/// than probe-bound (the compile it sits in runs at ~4.1–5.2 instructions per cycle). What
/// dominates that compile is instead how *often* the descent runs: `pool.erl` performs ~284 881
/// descents (≈256 M inserts — a few thousand objects each) against `timer.erl`'s ~2.87e9 inserts,
/// so on a small compile the set is **starved and cold** on most descents and building it costs
/// more than probing it. Memoising addresses **across** walks cannot replace the per-walk reset —
/// a heap address is reused as soon as its object is collected, so one walk's visit says nothing
/// about the next — and cutting the invocation count (with the caller's per-edge
/// `root_is_unscanned`/`for_each_child_slot` cost) is a `super::gc` / `beam_engine` policy
/// question, not this table's.
#[derive(Default)]
struct Visited {
    /// The table's slots, each a key and the generation that wrote it.
    slots: Vec<Slot>,
    /// The generation of the walk in progress.
    generation: u32,
    /// How many slots the current generation occupies.
    len: usize,
    /// The `grow` re-insert's key buffer, kept across grows so a rebuild allocates only the
    /// (doubled) slot array, never a scratch `Vec` beside it.
    keys: Vec<usize>,
    /// Test-only: the probes the walk in progress has made (the probe-quality test's witness).
    #[cfg(test)]
    probes: u64,
    /// Test-only: the keys the walk in progress has been asked to record.
    #[cfg(test)]
    inserts: u64,
    /// Test-only: how many times the table has grown (the growth test's witness).
    #[cfg(test)]
    grows: u64,
}

/// One slot of the visited set: the object address it holds, plus the generation that wrote it
/// (which is what marks a slot empty — a slot whose generation is not the walk's holds no key).
#[derive(Clone, Copy, Default)]
struct Slot {
    key: usize,
    generation: u32,
}

/// The table's initial slot count (a power of two) — the size a walk that only ever sees a handful
/// of objects stays at, and the floor every later table is a multiple of.
const INITIAL_SLOTS: usize = 1024;

/// The **odd** 64-bit multiplicative constant of the Fibonacci hash — the integer nearest
/// `2^64 / phi`, so consecutive multiples of it walk the whole word uniformly. Multiplying by an
/// odd constant is invertible modulo `2^64`, so no address bit is discarded by the multiply.
const FIBONACCI_HASH: u64 = 0x9E37_79B9_7F4A_7C15;

impl Visited {
    /// Starts a new walk: `O(1)`, without touching a single slot (the generation bump is the
    /// reset). The only path that touches every slot is the wrap of the `u32` generation
    /// counter, once every four billion walks.
    fn begin(&mut self) {
        if self.generation == u32::MAX {
            self.slots.clear();
            self.generation = 1;
        } else {
            self.generation += 1;
        }
        self.len = 0;
    }

    /// Rebuilds the slot array at `slots` (a power of two) with **every slot empty**. The arrays keep
    /// their capacity, so a reset of a grown table reuses the allocation it already has, and the
    /// (now untouched) tail of it costs nothing: no later probe reaches it again.
    fn resize_table(&mut self, slots: usize) {
        self.slots.clear();
        self.slots.resize(slots, Slot::default());
        self.len = 0;
    }

    /// Records `key`; answers whether it is **newly** recorded (`false` when the walk in
    /// progress already holds it — which is what makes a shared or cyclic graph terminate).
    fn insert(&mut self, key: usize) -> bool {
        if self.slots.is_empty() {
            self.slots.resize(INITIAL_SLOTS, Slot::default());
        } else if (self.len + 1) * 4 > self.slots.len() * 3 {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        #[cfg(test)]
        {
            self.inserts += 1;
        }
        let mut slot = Self::probe(key) & mask;
        loop {
            #[cfg(test)]
            {
                self.probes += 1;
            }
            let entry = self.slots[slot];
            if entry.generation != self.generation {
                self.slots[slot] = Slot {
                    key,
                    generation: self.generation,
                };
                self.len += 1;
                return true;
            }
            if entry.key == key {
                return false;
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Doubles the table and re-inserts the current generation's keys (the only allocation the
    /// descent ever performs, and it amortises away).
    fn grow(&mut self) {
        // The key buffer is taken out of `self` so the rebuild can borrow the table mutably;
        // it is returned at the end, so a warm table's grows allocate the slot array alone.
        let mut keys = std::mem::take(&mut self.keys);
        keys.clear();
        keys.extend(
            self.slots
                .iter()
                .filter(|slot| slot.generation == self.generation)
                .map(|slot| slot.key),
        );
        self.resize_table(self.slots.len() * 2);
        #[cfg(test)]
        {
            self.grows += 1;
        }
        // The re-insert cannot re-enter `grow`: the table is twice one that was at most
        // three-quarters full, so the resulting load is at most three-eighths.
        for &key in &keys {
            let _ = self.insert(key);
        }
        self.keys = keys;
    }

    /// The initial probe index of `key` — a **full-width multiplicative (Fibonacci) mix**, the
    /// **high** bits of the 64-bit product forming the index.
    ///
    /// The index must be a function of *every* address bit, because the addresses this table is
    /// keyed on are **allocation-shaped**, not random: an object address inside one allocation
    /// region is `base + step * i` for a small number of object sizes, and the immix mature
    /// generation lays its objects out on 128-byte lines inside 32 KiB blocks
    /// (`base + (B*4096 + L*16 + w)*8`). Every such pattern leaves the **low** address bits
    /// structured, so an index built from them alone (`key >> 3`, say) reaches only a fraction of
    /// the slots — the table then behaves as if it were a small fraction of its size, and its
    /// linear-probe runs grow without bound as it fills. Taking the *high* product bits instead
    /// folds every lower address bit into the index, so an aligned or fixed-stride allocation
    /// pattern lands uniformly and the probe runs stay near one. The mix is one `imul` — the
    /// multiply is the whole cost, since the caller already masks the result.
    fn probe(key: usize) -> usize {
        ((key as u64).wrapping_mul(FIBONACCI_HASH) >> 32) as usize
    }
}

impl<'a> Walk<'a> {
    /// Starts a walk over `scratch` against `bounds`, resetting the reused state in `O(1)`.
    fn begin(scratch: &'a mut WalkScratch, bounds: ScannedBounds) -> Walk<'a> {
        scratch.pending.clear();
        scratch.visited.begin();
        Walk {
            scratch,
            bounds,
            hit: false,
            foreign_reached: false,
        }
    }

    /// Classifies one reachable word: enqueues a container the collector scans, records a hazard
    /// for a container it does not scan that forwards a scanned word, and ignores everything no
    /// relocation touches.
    fn seed(&mut self, process: &Process, word: Eterm) {
        if self.hit {
            return;
        }
        // The per-word classifier, over the geometry this walk read once: `None` for an immediate
        // (a small integer, an atom, `NIL`, an internal pid/port id — no children, and neither
        // this walk nor the collector descends into it) and for a literal-area pointer (a frozen
        // code constant; see the module docs).
        match word_region(process, &self.bounds, word) {
            None => {}
            Some(WordRegion::Scanned) => {
                // A container the collector scans: descend into it (once — the visited set is what
                // makes a shared or cyclic graph terminate).
                if self.scratch.visited.insert(ptr_val(word) as usize) {
                    self.scratch.pending.push(word);
                }
            }
            Some(WordRegion::Foreign) => self.reach_foreign(process, word),
        }
    }

    /// Records a container in a region the collector does not scan and decides what it means: a
    /// hazard when it forwards a word into a region the collector **does** scan (a collection
    /// would relocate that word without updating the container), otherwise a container to expand
    /// so a foreign → foreign → scanned chain is still found.
    fn reach_foreign(&mut self, process: &Process, word: Eterm) {
        self.foreign_reached = true;
        if forwards_scanned_word(process, &self.bounds, word) {
            self.hit = true;
            return;
        }
        // An inert foreign container (nothing it forwards is relocated) cannot dangle anything
        // itself — but a foreign child of it can, so it joins the worklist like a scanned one.
        if self.scratch.visited.insert(ptr_val(word) as usize) {
            self.scratch.pending.push(word);
        }
    }

    /// Seeds from every container [`Process::collect`] gathers: the registers (`x[0..live)`,
    /// the whole stack and the host-published extra roots), then each queued message's
    /// `payload`/`from`/`token`, the process dictionary, the saved arguments and the pending
    /// signals — in that order, and **exactly** that set (nothing more, nothing less: the walk's
    /// answer may over-approximate the roots, but a *missing* root would be a memory-safety bug,
    /// so the set is pinned by the `a_foreign_container_in_every_seeded_root_family_is_detected`
    /// case).
    ///
    /// # No per-call allocation
    ///
    /// The mailbox and the dictionary expose iterators, so those two families are seeded
    /// **directly**, with no buffer. The other three append into a caller buffer, so they are
    /// gathered into the scratch's reusable [`WalkScratch::roots`] — taken with a 3-word
    /// [`std::mem::take`] (`self.scratch` is behind `&mut self`, so it cannot be borrowed for the
    /// gather *and* for the `seed` calls at once) and handed back at the end. A warmed-up walk
    /// therefore performs **no allocation at all** here.
    fn seed_process_roots(&mut self, process: &Process) {
        let mut buf = std::mem::take(&mut self.scratch.roots);
        #[cfg(test)]
        seed_counters::note_walk(buf.capacity() == 0);
        buf.clear();

        // 1. The registers: `x[0..live)`, the whole Erlang stack, the extra roots.
        process.registers().gather_roots(&mut buf);
        #[cfg(test)]
        {
            seed_counters::note_register_words(buf.len());
            seed_counters::note_root_words(buf.len());
        }
        self.seed_all(process, &buf);
        // 2. The mailbox envelope — three words per message, straight from the queue.
        if !self.hit {
            #[cfg(test)]
            seed_counters::note_root_words(3 * process.mailbox().len());
            for message in process.mailbox().iter() {
                self.seed(process, message.payload());
                self.seed(process, message.from());
                self.seed(process, message.token());
                if self.hit {
                    break;
                }
            }
        }
        // 3. The dictionary, straight from its entry iterator.
        if !self.hit {
            #[cfg(test)]
            seed_counters::note_root_words(2 * process.dict().len());
            for (key, value) in process.dict().entries() {
                self.seed(process, key);
                self.seed(process, value);
                if self.hit {
                    break;
                }
            }
        }
        // 4. The saved BIF/NIF arguments.
        if !self.hit {
            buf.clear();
            process.saved_args().gather_roots(&mut buf);
            #[cfg(test)]
            seed_counters::note_root_words(buf.len());
            self.seed_all(process, &buf);
        }
        // 5. The pending-signal queue.
        if !self.hit {
            buf.clear();
            process.signals().gather_roots(&mut buf);
            #[cfg(test)]
            seed_counters::note_root_words(buf.len());
            self.seed_all(process, &buf);
        }

        // Hand the buffer back, so the next walk reuses this allocation.
        self.scratch.roots = buf;
    }

    /// Classifies every word of `words` in order, stopping at the first hit (`seed` itself
    /// short-circuits, but the loop must not keep walking a long stack after one).
    fn seed_all(&mut self, process: &Process, words: &[Eterm]) {
        for &word in words {
            if self.hit {
                return;
            }
            self.seed(process, word);
        }
    }

    /// Expands every enqueued object, answering whether a hit was reached.
    fn run(&mut self, process: &Process) -> bool {
        while let Some(word) = self.scratch.pending.pop() {
            self.expand(process, word);
            if self.hit {
                return true;
            }
        }
        false
    }

    /// Classifies every child of the object `word` names.
    fn expand(&mut self, process: &Process, word: Eterm) {
        let address: *const Eterm = ptr_val(word);
        // SAFETY: `word` reaches the worklist only through `seed`/`reach_foreign`, which enqueue
        // exactly the boxed/cons words that are (a) genuine terms and (b) either addressed inside
        // a region the process's collector scans or a container the region classifier called
        // foreign — in both cases live objects of the running process's term graph. `address`
        // therefore heads a live object, so its header word and every payload word the shape
        // implies are live and readable (the same invariant `beam_term`'s own materialization
        // entry points document).
        unsafe {
            if is_list(word) {
                // A cons cell is header-less: both of its words are references.
                self.seed(process, *address);
                self.seed(process, *address.add(1));
                return;
            }
            let header = *address;
            // SAFETY: `address` heads the live object `header` names, so the canonical child
            // enumeration visits exactly that shape's term-reference slots (`for_each_child_slot`
            // yields offsets relative to the header word, and every one is in bounds and live).
            // This is the same enumeration `beam_message::copy` drives, so the walk cannot
            // diverge from the collector's own view of the object.
            for_each_child_slot(header, address, |offset| {
                self.seed(process, *address.add(offset));
            });
        }
    }
}

/// Test-only tallies of the seeding's *scale* and of its **allocation discipline** — the witness
/// behind the module's "the walk allocates nothing after warm-up" claim and the counters a
/// measurement run reports per in-process compile.
///
/// Gated on `#[cfg(test)]` (`super::gc`'s `prefix_counters` is the same pattern), so neither the
/// release binary the differential gate builds nor the debug one it drives carries a single extra
/// instruction.
#[cfg(test)]
pub(crate) mod seed_counters {
    use std::cell::Cell;

    /// One thread's tallies since the last [`reset`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct SeedCounts {
        /// Calls to [`super::Walk::seed_process_roots`] — exactly one per walk, and therefore
        /// exactly the number of root-buffer `Vec`s a per-call `Vec::new()` would have malloc'd.
        pub(crate) walks: usize,
        /// Walks that found the root buffer with **no capacity** and so had to allocate it. One
        /// per warm scratch, never per walk: this is the counter that pins the reuse.
        pub(crate) buffer_allocations: usize,
        /// Every family's root words, summed over every walk — the whole seeded root set.
        pub(crate) root_words: usize,
        /// Register-family words gathered (`x[0..live)` + the whole stack + the extra roots),
        /// summed over every walk — the walk's dominant root family.
        pub(crate) register_words: usize,
    }

    impl SeedCounts {
        /// The all-zero tally (the `const` form the thread-local's initialiser needs).
        pub(crate) const ZERO: SeedCounts = SeedCounts {
            walks: 0,
            buffer_allocations: 0,
            root_words: 0,
            register_words: 0,
        };
    }

    thread_local! {
        /// This thread's tallies since the last [`reset`].
        static COUNTERS: Cell<SeedCounts> = const { Cell::new(SeedCounts::ZERO) };
    }

    /// This thread's tallies since the last [`reset`].
    pub(crate) fn snapshot() -> SeedCounts {
        COUNTERS.with(Cell::get)
    }

    /// Zeroes this thread's tallies (a measurement's "before" reading).
    pub(crate) fn reset() {
        COUNTERS.with(|counters| counters.set(SeedCounts::ZERO));
    }

    /// Records one walk, and whether its root buffer had to be allocated (`cold`).
    pub(crate) fn note_walk(cold: bool) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.walks += 1;
            if cold {
                value.buffer_allocations += 1;
            }
            counters.set(value);
        });
    }

    /// Records one walk's register-family word count.
    pub(crate) fn note_register_words(words: usize) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.register_words += words;
            counters.set(value);
        });
    }

    /// Records one family's root-word count for the walk in progress.
    pub(crate) fn note_root_words(words: usize) {
        COUNTERS.with(|counters| {
            let mut value = counters.get();
            value.root_words += words;
            counters.set(value);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use beam_message::Message;
    use beam_process::{
        ExitFamily, MonitorKind, PendingSignal, ProcessTable, RefAllocator, SignalAtoms,
    };
    use beam_term::heap::Heap;
    use beam_term::prelude::{make_small, NIL};
    use beam_term::AtomTable;    // ---------------------------------------------------------------------------------------
    // The probe quality
    // ---------------------------------------------------------------------------------------

    /// A synthetic allocation region shaped exactly like the immix mature generation: `blocks`
    /// 32 KiB blocks, each of 256 128-byte lines of 16 words, of which the first `words_per_line`
    /// are live object addresses — i.e. every address is `base + (B*4096 + L*16 + w) * 8`.
    fn block_region(base: usize, blocks: usize, words_per_line: usize) -> Vec<usize> {
        let mut out = Vec::with_capacity(blocks * 256 * words_per_line);
        for b in 0..blocks {
            for l in 0..256 {
                for w in 0..words_per_line {
                    out.push(base + (b * 4096 + l * 16 + w) * 8);
                }
            }
        }
        out
    }

    /// Drives `Visited` over `addrs` exactly as [`Walk::seed`] would (one `insert` per address,
    /// growing the table as the walk deepens) and reports (mean probes per input address, slots).
    ///
    /// The probes the `grow` re-insert performs are charged to the address that triggered the
    /// grow, so the figure is "probes per address recorded", not "probes per table write".
    fn mean_probes_per_insert(addrs: &[usize]) -> (f64, usize) {
        let mut visited = Visited::default();
        visited.begin();
        let mut probes = 0u64;
        for &addr in addrs {
            let before = visited.probes;
            visited.insert(addr);
            probes += visited.probes - before;
        }
        (probes as f64 / addrs.len() as f64, visited.slots.len())
    }

    /// The probe index must depend on **every** address bit.
    ///
    /// Every real address this table sees is `base + step * i` for some allocation pattern, so a
    /// probe built from the low address bits alone maps a whole region onto a small fraction of
    /// the slots: the table then behaves as if it were a fraction of its size, and its linear
    /// probe runs grow without bound as it fills. Three line/block geometries (8, 4 and 2 of each
    /// 128-byte line's 16 words, at disjoint bases) reproduce exactly the shapes the walk sees;
    /// the mean probes per insert must stay near one, and the table must not balloon.
    #[test]
    fn the_probe_mix_spreads_a_line_and_block_organised_region() {
        let mut addrs = block_region(0x7f00_0000_0000, 2, 8);
        addrs.extend(block_region(0x7f01_0000_0000, 4, 4));
        addrs.extend(block_region(0x7f02_0000_0000, 8, 2));
        assert_eq!(addrs.len(), 12_288);
        assert!(addrs.len() >= 4_000);
        assert_eq!(addrs.iter().collect::<BTreeSet<_>>().len(), addrs.len());

        let (mean, slots) = mean_probes_per_insert(&addrs);
        // The old `(key >> 3) ^ (key >> 15)` probe measures 7.8 probes per insert here (and the
        // walked shapes are worse); a probe that mixes every address bit measures ~3.1.
        assert!(
            mean < 4.0,
            "mean probes per insert {mean:.3} — the probe index is not mixing the aligned \
             address bits (slots {slots})"
        );
        assert!(
            (addrs.len() / 2..=addrs.len() * 4).contains(&slots),
            "table size {slots} out of proportion for {} addresses",
            addrs.len()
        );
    }

    /// The same region must not make the table's fill degenerate: after every address is
    /// recorded exactly once, every one of them is found by a lookup (no lost key, no false
    /// "already visited").
    #[test]
    fn every_aligned_address_is_recorded_exactly_once() {
        let addrs = block_region(0x7f00_0000_0000, 2, 8);
        let mut visited = Visited::default();
        visited.begin();
        for &addr in &addrs {
            assert!(
                visited.insert(addr),
                "{addr:#x} reported as already visited"
            );
        }
        for &addr in &addrs {
            assert!(!visited.insert(addr), "{addr:#x} lost");
        }
        assert_eq!(visited.len, addrs.len());
    }

    // ---------------------------------------------------------------------------------------
    // The visitation semantics
    // ---------------------------------------------------------------------------------------

    /// The nodes the table holds for the walk in progress, in the shape a walk's graph has.
    fn recorded(visited: &Visited) -> BTreeSet<usize> {
        visited
            .slots
            .iter()
            .filter(|slot| slot.generation == visited.generation)
            .map(|slot| slot.key)
            .collect()
    }
    /// The reference reachability: a plain `O(nodes + edges)` traversal of `graph` from `root`.
    fn reachable(graph: &[Vec<usize>], root: usize) -> BTreeSet<usize> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![root];
        seen.insert(root);
        while let Some(node) = pending.pop() {
            for &child in &graph[node] {
                if seen.insert(child) {
                    pending.push(child);
                }
            }
        }
        seen
    }

    /// Drives `Visited` exactly the way [`Walk::seed`] does — `if insert(k) { pending.push(k) }` —
    /// and reports, per node, how many times it was expanded.
    fn drive(visited: &mut Visited, graph: &[Vec<usize>], root: usize) -> Vec<usize> {
        let mut expansions = vec![0usize; graph.len()];
        let mut pending = Vec::new();
        if visited.insert(root) {
            pending.push(root);
        }
        while let Some(node) = pending.pop() {
            expansions[node] += 1;
            for &child in &graph[node] {
                if visited.insert(child) {
                    pending.push(child);
                }
            }
        }
        expansions
    }

    /// A deterministic graph with `nodes` **reachable** nodes (`0..nodes`) and `islands`
    /// unreachable ones (`nodes..nodes + islands`, wired only among themselves). Its edges give
    /// every shape the walk's real graphs have: a self-loop, several back-edges (cycles), a
    /// shared hub every node points at (fan-in), a chain plus random forward and backward edges
    /// (fan-out), and shared subgraphs.
    fn shared_graph(nodes: usize, islands: usize) -> Vec<Vec<usize>> {
        let total = nodes + islands;
        let mut graph = vec![Vec::new(); total];
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as usize % bound
        };
        for node in 0..nodes {
            graph[node].push(node); // self-loop
            graph[node].push(node); // repeated edge (must not be expanded twice)
            if node + 1 < nodes {
                graph[node].push(node + 1); // forward edge, so every node is reachable from 0
            }
            graph[node].push(if node == 0 { node } else { node - 1 }); // back edge / cycle
            graph[node].push(0); // shared hub, fan-in
            graph[node].push(next(nodes)); // random graph edge
            if node >= 2 {
                graph[node].push(node - 2); // a longer cycle edge
            }
        }
        for island in nodes..total {
            graph[island].push(island);
            graph[island].push(if island == nodes { island } else { island - 1 });
            graph[island].push(nodes); // points back into the island, never at a reachable node
        }
        graph
    }

    /// The visited set must equal the reachable set **exactly** — no missed node, no extra node —
    /// and every node must be expanded exactly once, over a graph with cycles, self-loops,
    /// fan-in/fan-out, repeated edges and shared subgraphs. A larger variant crosses the table's
    /// `grow` threshold, so the re-insert during the grow is covered too.
    #[test]
    fn the_visited_set_is_exactly_the_reachable_set() {
        for (nodes, islands) in [(40usize, 3usize), (2_000, 500), (9_000, 1_000)] {
            let graph = shared_graph(nodes, islands);
            let expected = reachable(&graph, 0);
            // The chain plus the hub make every one of `0..nodes` reachable, and nothing else.
            assert_eq!(expected.len(), nodes);

            let mut visited = Visited::default();
            visited.begin();
            let expansions = drive(&mut visited, &graph, 0);

            assert_eq!(
                recorded(&visited),
                expected,
                "visited set differs from the reachable set for {nodes} nodes"
            );
            assert_eq!(visited.len, expected.len());
            for node in 0..graph.len() {
                let want = usize::from(expected.contains(&node));
                assert_eq!(
                    expansions[node], want,
                    "node {node} expanded {} times (want {want}) with {nodes} nodes",
                    expansions[node]
                );
            }
            // A graph of this size crosses `INITIAL_SLOTS`' growth threshold, so the re-insert
            // path really ran.
            if nodes > INITIAL_SLOTS * 3 / 4 {
                assert!(visited.slots.len() > INITIAL_SLOTS);
            }
        }
    }

    /// A fresh generation must re-admit **every** key: `begin()` is the whole reset, so a walk
    /// that repeats itself records each key again (and a stale generation must never answer
    /// "already visited").
    #[test]
    fn a_fresh_generation_readmits_every_key() {
        let graph = shared_graph(1_500, 200);
        let expected = reachable(&graph, 0);
        let mut visited = Visited::default();

        for _ in 0..3 {
            visited.begin();
            assert!(recorded(&visited).is_empty());
            assert_eq!(visited.len, 0);
            let expansions = drive(&mut visited, &graph, 0);
            assert_eq!(recorded(&visited), expected);
            assert!(expansions.iter().all(|&count| count <= 1));
        }
    }

    /// `begin()` must never be more than `O(1)` in the table's size: it bumps a generation
    /// counter and resets the occupancy, leaving the slots untouched.
    #[test]
    fn begin_is_a_generation_bump() {
        let mut visited = Visited::default();
        visited.begin();
        for addr in block_region(0x7f00_0000_0000, 1, 8) {
            visited.insert(addr);
        }
        let slots = visited.slots.len();
        let generation = visited.generation;
        visited.begin();
        assert_eq!(visited.generation, generation + 1);
        assert_eq!(visited.len, 0);
        assert_eq!(visited.slots.len(), slots);
        assert_eq!(
            visited
                .slots
                .iter()
                .filter(|entry| entry.generation == visited.generation)
                .count(),
            0
        );
    }

    /// A probe that ignores the low address bits is what this fix removed: the regression test
    /// above fails against it. Pinned here directly so a re-introduction is caught wherever it
    /// happens (the assertion is on the mixing property, not on a particular constant).
    #[test]
    fn the_probe_index_uses_the_low_address_bits() {
        let mut visited = Visited::default();
        visited.begin();
        visited.slots.resize(1 << 16, Slot::default());
        let mask = visited.slots.len() - 1;
        let probe = |key: usize| Visited::probe(key) & mask;
        // `key >> 3` maps the 16 words of one line onto 16 slot indices that differ only in the
        // low four bits; the mix must spread them over the whole table.
        let line: Vec<usize> = (0..16).map(|w| 0x7f00_0000_0000 + w * 8).collect();
        let spread: BTreeSet<usize> = line.iter().map(|&k| probe(k)).collect();
        assert_eq!(spread.len(), line.len());
        let min = *spread.iter().next().unwrap();
        let max = *spread.iter().next_back().unwrap();
        assert!(
            max - min > 4096,
            "the 16 words of one line land in [{min}, {max}] — the index ignores the low bits"
        );
    }

    /// The table's rebuilds (`grow` is the only path that re-materialises the slot array) must
    /// preserve **exactly** the keys the walk's generation holds: no key lost, none duplicated,
    /// and the occupancy tracking the new array.
    #[test]
    fn every_key_survives_a_table_rebuild() {
        let addrs = block_region(0x7f00_0000_0000, 2, 8); // 4 096 distinct addresses
        let mut visited = Visited::default();
        visited.begin();
        for &addr in &addrs {
            visited.insert(addr);
        }
        // 4 096 keys cross the initial 1 024 slots' growth threshold several times over, so the
        // rebuild really ran.
        assert!(visited.grows >= 2, "the rebuild path did not run");
        assert!(
            visited.slots.len() >= addrs.len() * 2,
            "the rebuild left the array at {} slots for {} keys",
            visited.slots.len(),
            addrs.len()
        );
        assert_eq!(visited.len, addrs.len());
        assert_eq!(recorded(&visited), addrs.iter().copied().collect());
        for &addr in &addrs {
            assert!(!visited.insert(addr), "{addr:#x} duplicated by the rebuild");
        }
        assert_eq!(visited.len, addrs.len());
    }

    /// The table is grown for the deepest walk a process ever runs and then **reused**: the many
    /// shallow walks that follow a deep one must neither rebuild it nor see a different answer.
    /// This is the property the retained table is there for.
    #[test]
    fn a_deep_walk_leaves_the_table_reusable_by_shallow_walks() {
        let deep = block_region(0x7f00_0000_0000, 2, 8);
        let mut visited = Visited::default();
        visited.begin();
        for &addr in &deep {
            visited.insert(addr);
        }
        let slots = visited.slots.len();
        let grows = visited.grows;
        for round in 0..64usize {
            visited.begin();
            let key = 0x7f10_0000_0000 + round * 8;
            assert!(visited.insert(key));
            assert!(!visited.insert(key));
            assert_eq!(recorded(&visited), BTreeSet::from([key]));
            assert_eq!(
                visited.slots.len(),
                slots,
                "a shallow walk resized the table"
            );
            assert_eq!(visited.grows, grows, "a shallow walk rebuilt the table");
        }
    }

    // ---------------------------------------------------------------------------------------
    // The seeded root set: exactly `Process::collect`'s, family by family
    // ---------------------------------------------------------------------------------------
    //
    // The walk's answer may over-approximate the collected roots — a spurious hit only declines a
    // collection that might have been sound — but a root it **misses** is a memory-safety bug: the
    // collector would relocate a term the valve never classified. The family-by-family cases below
    // are that pin, and they are *hazard-shaped* rather than count-shaped: each plants the one term
    // that makes the walk's answer matter (a container built outside every region the collector
    // scans) in exactly ONE root family, and requires the walk to find it.

    /// A spawned process in a table of its own, so a case can plant a word in any root family and
    /// keep the process alive to read it back.
    fn seeded_process() -> (ProcessTable, ProcessId) {
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()));
        let pid = table.spawn_default().expect("spawn a process");
        (table, pid)
    }

    /// The cons cell a case plants: built on a `beam_term` test arena, i.e. outside every region
    /// the process's collector scans, and header-less — so the collector would have to forward
    /// both of its words, which is exactly the hazard the valve exists for.
    ///
    /// The arena must **outlive** every walk that classifies the cell (the walk reads the cell's
    /// address, and a nested case descends into it).
    fn foreign_container(arena: &mut Heap) -> Eterm {
        arena
            .cons(make_small(11), make_small(12))
            .expect("the arena builds the cons cell")
    }

    /// One root family [`Walk::seed_process_roots`] seeds from, and how a word is planted in it.
    struct RootFamily {
        /// The family's name (the failure messages name it).
        name: &'static str,
        /// Plants `word` in exactly this family and nowhere else.
        plant: fn(&mut Process, Eterm),
    }

    /// The register family: `x[0]` (inside the live prefix).
    fn plant_x(process: &mut Process, word: Eterm) {
        let x = process.registers_mut().x_mut();
        x.set_live(1).expect("x[0] is inside the register file");
        x.set(0, word).expect("x[0] is live");
    }

    /// The register family: the whole Erlang stack is a root (`gather_roots` appends all of it).
    fn plant_stack(process: &mut Process, word: Eterm) {
        process.registers_mut().stack_mut().push(word);
    }

    /// The register family: the host-published extra roots.
    fn plant_extra_root(process: &mut Process, word: Eterm) {
        process.registers_mut().extra_roots_mut().push(word);
    }

    /// The mailbox envelope: the message payload.
    fn plant_mailbox_payload(process: &mut Process, word: Eterm) {
        process.mailbox_mut().enqueue(Message::new(word, NIL));
    }

    /// The process dictionary's key.
    fn plant_dict_key(process: &mut Process, word: Eterm) {
        process.dict_mut().put(word, make_small(1));
    }

    /// The process dictionary's value.
    fn plant_dict_value(process: &mut Process, word: Eterm) {
        process.dict_mut().put(make_small(2), word);
    }

    /// The saved BIF/NIF arguments.
    fn plant_saved_arg(process: &mut Process, word: Eterm) {
        process.saved_args_mut().push(word);
    }

    /// The pending-signal queue: an exit signal's reason.
    fn plant_signal_reason(process: &mut Process, word: Eterm) {
        process.signals_mut().push(PendingSignal::exit(
            ProcessId::new(0, 0),
            word,
            ExitFamily::Explicit,
        ));
    }

    /// The pending-signal queue: a monitor-DOWN's user tag — the one signal word a per-signal
    /// root list could lose while still holding the reason.
    fn plant_signal_tag(process: &mut Process, word: Eterm) {
        let mut references = RefAllocator::new();
        process.signals_mut().push(PendingSignal::down(
            references.next(),
            ProcessId::new(0, 0),
            MonitorKind::Process,
            Some(word),
            make_small(0),
        ));
    }

    /// Every family [`Walk::seed_process_roots`] seeds from, in the order it seeds them
    /// (registers → mailbox → dictionary → saved arguments → pending signals).
    ///
    /// A family the walk stopped seeding must fail
    /// [`a_foreign_container_in_every_seeded_root_family_is_detected`].
    ///
    /// The mailbox's `from` and `token` words are **not** in this list, and that is not an
    /// omission: [`Queue::enqueue`](beam_message::Queue::enqueue) debug-asserts both envelope words
    /// are **immediates** (`beam_message`'s deliberate narrowing of the C's heap-`token`/heap-`from`
    /// latitude), so no in-tree or test producer can put a heap container there to be detected.
    /// Their seeding is pinned by **word count** instead — the envelope contributes exactly three
    /// root words per message
    /// ([`the_mailbox_envelope_contributes_three_root_words_per_message`]), which is
    /// [`Process::collect`]'s own accounting.
    const ROOT_FAMILIES: [RootFamily; 9] = [
        RootFamily {
            name: "x[0]",
            plant: plant_x,
        },
        RootFamily {
            name: "the Erlang stack",
            plant: plant_stack,
        },
        RootFamily {
            name: "the extra roots",
            plant: plant_extra_root,
        },
        RootFamily {
            name: "a mailbox payload",
            plant: plant_mailbox_payload,
        },
        RootFamily {
            name: "a dictionary key",
            plant: plant_dict_key,
        },
        RootFamily {
            name: "a dictionary value",
            plant: plant_dict_value,
        },
        RootFamily {
            name: "a saved BIF/NIF argument",
            plant: plant_saved_arg,
        },
        RootFamily {
            name: "a pending-signal reason",
            plant: plant_signal_reason,
        },
        RootFamily {
            name: "a pending-signal DOWN tag",
            plant: plant_signal_tag,
        },
    ];

    /// The two answers a case reads from the process's root set alone: with the descent gated
    /// **off** (the always-run per-word phase) and with it **on** (the full descent).
    fn seeded_answers(table: &ProcessTable, pid: ProcessId) -> (bool, bool) {
        let process = table.get(pid).expect("the spawned process");
        let mut scratch = WalkScratch::default();
        let gated = any_root_is_unscanned(&mut scratch, process, std::iter::empty(), false);
        let descended = any_root_is_unscanned(&mut scratch, process, std::iter::empty(), true);
        (gated, descended)
    }

    /// The number of root words [`Process::collect`] gathers for `process` — the walk's root set
    /// must be **exactly** this many words (no family dropped, none seeded twice).
    fn collect_root_words(process: &Process) -> usize {
        let mut buf: Vec<Eterm> = Vec::new();
        process.registers().gather_roots(&mut buf);
        let mut total = buf.len();
        total += 3 * process.mailbox().len();
        total += 2 * process.dict().len();
        buf.clear();
        process.saved_args().gather_roots(&mut buf);
        total += buf.len();
        buf.clear();
        process.signals().gather_roots(&mut buf);
        total + buf.len()
    }

    /// **The pin: an unscanned container in ANY single root family is detected, and a clean
    /// process is not.**
    ///
    /// For every family in [`ROOT_FAMILIES`], four cases:
    ///
    /// * **clean** — nothing planted: neither phase may report a hit (the negative control that
    ///   keeps the assertions below from being satisfied by a walk that always answers `true`);
    /// * **direct** — the foreign cons cell *is* the root word: the per-word phase must catch it
    ///   with the descent gate **closed** (so a family the walk seeds only inside the descent
    ///   would fail here);
    /// * **nested** — a scanned young tuple holding the foreign cell is the root word: the gated
    ///   phase must NOT report a hit (the per-word classifier cannot see through a scanned
    ///   container) and the descent MUST (so the family is really seeded as a descent origin);
    /// * **safe** — a young tuple holding an immediate: no hit either way.
    ///
    /// The word count of the gathered set is pinned too ([`collect_root_words`]): the walk must
    /// seed exactly the words `Process::collect` gathers.
    #[test]
    fn a_foreign_container_in_every_seeded_root_family_is_detected() {
        for family in ROOT_FAMILIES {
            let name = family.name;
            // One arena per family: its cell must outlive every walk below that classifies it.
            let mut arena = Heap::new(64);
            let foreign = foreign_container(&mut arena);

            // (0) The word count, on a process whose every family is non-empty — where they are
            //     plantable at all (an empty root set still pins the register family's length).
            {
                let (mut table, pid) = seeded_process();
                {
                    (family.plant)(
                        table.get_mut(pid).expect("the spawned process"),
                        make_small(7),
                    );
                }
                let process = table.get(pid).expect("the spawned process");
                let expected = collect_root_words(process);
                seed_counters::reset();
                let mut scratch = WalkScratch::default();
                let _ = any_root_is_unscanned(&mut scratch, process, std::iter::empty(), true);
                let counts = seed_counters::snapshot();
                assert_eq!(counts.walks, 1);
                assert_eq!(
                    counts.root_words, expected,
                    "{name}: the walk must seed exactly the words `Process::collect` gathers"
                );
            }

            // (a) clean: an untouched process is never declined, gated or descended.
            {
                let (table, pid) = seeded_process();
                let (gated, descended) = seeded_answers(&table, pid);
                assert!(
                    !gated && !descended,
                    "{name}: an empty root set must never report an unscanned root \
                     (gated {gated}, descended {descended})"
                );
            }

            // (b) direct: the foreign cell is the root word itself.
            {
                let (mut table, pid) = seeded_process();
                (family.plant)(table.get_mut(pid).expect("the spawned process"), foreign);
                let (gated, descended) = seeded_answers(&table, pid);
                assert!(
                    gated,
                    "{name}: a foreign container planted directly in this family must be caught \
                     by the per-word phase, with the descent gate closed"
                );
                assert!(
                    descended,
                    "{name}: a foreign container planted directly in this family must be caught \
                     by the full valve"
                );
            }

            // (c) nested: a SCANNED young tuple holding the foreign cell.
            {
                let (mut table, pid) = seeded_process();
                let wrapper = {
                    let process = table.get_mut(pid).expect("the spawned process");
                    let wrapper = process
                        .heap_mut()
                        .tuple(&[foreign])
                        .expect("the young heap builds `{Foreign}`");
                    (family.plant)(process, wrapper);
                    wrapper
                };
                let (gated, descended) = seeded_answers(&table, pid);
                assert!(
                    !gated,
                    "{name}: the per-word phase classifies a scanned young tuple as safe, so it \
                     must not claim a hit for `{{Foreign}}` (wrapper {wrapper:?})"
                );
                assert!(
                    descended,
                    "{name}: the descent must reach the foreign container nested one level below \
                     a scanned young tuple seeded from this family"
                );
            }

            // (d) safe: a scanned young tuple holding an immediate.
            {
                let (mut table, pid) = seeded_process();
                {
                    let process = table.get_mut(pid).expect("the spawned process");
                    let safe = process
                        .heap_mut()
                        .tuple(&[make_small(3), make_small(4)])
                        .expect("the young heap builds `{3,4}`");
                    (family.plant)(process, safe);
                }
                let (gated, descended) = seeded_answers(&table, pid);
                assert!(
                    !gated && !descended,
                    "{name}: a scanned young tuple of immediates must never decline a collection \
                     (gated {gated}, descended {descended})"
                );
            }
        }
    }

    /// **The mailbox envelope contributes three root words per message** — `payload`, `from` and
    /// `token`, exactly as [`Process::collect`] gathers them.
    ///
    /// This is how the `from`/`token` envelope words are pinned: their producer
    /// ([`Queue::enqueue`](beam_message::Queue::enqueue)) debug-asserts both are immediates, so no
    /// heap container can be planted there to be *detected* (see [`ROOT_FAMILIES`]). Counting the
    /// words catches a walk that seeds only the payload, or that seeds an envelope word twice.
    #[test]
    fn the_mailbox_envelope_contributes_three_root_words_per_message() {
        /// The root words one walk gathers for `process`.
        fn walk_root_words(process: &Process) -> usize {
            seed_counters::reset();
            let mut scratch = WalkScratch::default();
            let _ = any_root_is_unscanned(&mut scratch, process, std::iter::empty(), true);
            let counts = seed_counters::snapshot();
            assert_eq!(counts.walks, 1);
            counts.root_words
        }

        let (mut table, pid) = seeded_process();
        let empty = walk_root_words(table.get(pid).expect("the spawned process"));

        for messages in 1..=3usize {
            let (mut table, pid) = seeded_process();
            {
                let process = table.get_mut(pid).expect("the spawned process");
                for _ in 0..messages {
                    process.mailbox_mut().enqueue(Message::new(make_small(1), NIL));
                }
            }
            let process = table.get(pid).expect("the spawned process");
            assert_eq!(process.mailbox().len(), messages);
            assert_eq!(
                walk_root_words(process),
                empty + 3 * messages,
                "{messages} queued messages must contribute `payload`+`from`+`token` each"
            );
            // ... and the same count `Process::collect` would gather.
            assert_eq!(collect_root_words(process), walk_root_words(process));
        }
        let _ = &mut table;
        let _ = empty;
    }

    /// **The root buffer is allocated once per scratch and then reused** — the property the
    /// per-call `Vec::new()` broke, measured by [`seed_counters`] rather than asserted by
    /// inspection.
    ///
    /// A steady root set pays exactly one allocation, on the first walk; every later walk finds
    /// the capacity it needs. A fresh [`WalkScratch`] pays its own one (the runtime's
    /// admission-point scratch and the collection gate's are independent by design), and the
    /// answers of the 64 walks must be identical — a reused buffer must never leak state between
    /// walks.
    #[test]
    fn the_root_buffer_is_allocated_once_and_then_reused() {
        let (mut table, pid) = seeded_process();
        {
            let process = table.get_mut(pid).expect("the spawned process");
            let young = process
                .heap_mut()
                .tuple(&[make_small(5)])
                .expect("the young heap builds `{5}`");
            plant_x(process, young);
            plant_stack(process, make_small(6));
            process.saved_args_mut().push(make_small(7));
            process.signals_mut().push(PendingSignal::exit(
                ProcessId::new(0, 0),
                make_small(8),
                ExitFamily::Explicit,
            ));
        }
        let process = table.get(pid).expect("the spawned process");
        let expected_words = collect_root_words(process);

        seed_counters::reset();
        let mut scratch = WalkScratch::default();
        for round in 0..64 {
            assert!(
                !any_root_is_unscanned(&mut scratch, process, std::iter::empty(), true),
                "round {round}: a fully scanned root set must never be declined"
            );
        }
        let counts = seed_counters::snapshot();
        assert_eq!(counts.walks, 64, "one walk per call");
        assert_eq!(
            counts.buffer_allocations, 1,
            "the scratch's root buffer must be allocated exactly once (on the first walk) and \
             reused by the other 63 — it was allocated on {} of {} walks",
            counts.buffer_allocations, counts.walks
        );
        assert_eq!(
            counts.root_words,
            64 * expected_words,
            "every walk must seed the whole root set, {expected_words} words"
        );

        // A second scratch (the collection gate's own) pays its own first allocation and nothing
        // more: the reuse is per scratch, never per call.
        seed_counters::reset();
        let mut gate_scratch = WalkScratch::default();
        for _ in 0..8 {
            let _ = any_root_is_unscanned(&mut gate_scratch, process, std::iter::empty(), true);
        }
        let counts = seed_counters::snapshot();
        assert_eq!(counts.walks, 8);
        assert_eq!(counts.buffer_allocations, 1);
    }

    // ---------------------------------------------------------------------------------------
    // The scale of the seeding, measured on a real in-process compile
    // ---------------------------------------------------------------------------------------

    /// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
    /// directory (`<root>/erts_rust/crates/beam_rt`).
    fn source_root() -> std::path::PathBuf {
        match std::env::var_os("ERL_TOP") {
            Some(value) => std::path::PathBuf::from(value),
            None => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
        }
    }

    /// A booted runtime on the CLI's own route (`-eval`, driving the real
    /// `bootstrap/bin/start.boot` first) — the route `beam_rt -noshell -eval '<compile:file(…)>'`
    /// takes. The boot script matters to the compiler: it is what makes `epp`'s io requests reach
    /// a real `file_server_2` (`super::super::pump`'s in-process compile suites record the same).
    fn booted_runtime() -> crate::runtime::Runtime {
        let config = crate::config::RuntimeConfig::default()
            .with_resolved_boot_script(Some("start".to_string()));
        let mut runtime = crate::runtime::Runtime::new(config).expect("the default runtime");
        runtime.boot().expect("the boot sequence");
        let script = crate::CodeServer::own_bootstrap_boot_script()
            .expect("this runtime's own release carries `bootstrap/bin/start.boot`");
        runtime
            .boot_from_script(&script)
            .expect("the real boot script runs");
        runtime
    }

    /// **Opt-in measurement — the deterministic counters behind this module's cost claims.**
    ///
    /// Compiles one real stdlib source **in process** (the A.9 route) and reports, per module: the
    /// walk count (the valve's invocation count — one per heap-admission point), the root words and
    /// register words per walk, and how many times the root buffer had to be **allocated** (once
    /// per scratch plus a handful of growths, never once per walk).
    ///
    /// Ignored by default: it compiles `pool`/`ordsets`/`timer` in the **debug** profile, which
    /// takes minutes. Run it with
    /// `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib \
    /// -- --ignored --nocapture the_seeding_cost_of_an_in_process_compile`.
    #[test]
    #[ignore = "measurement: compiles real stdlib sources in-process (minutes in the debug profile)"]
    fn the_seeding_cost_of_an_in_process_compile() {
        let root = source_root();
        let sources: Vec<(&str, std::path::PathBuf)> = ["pool", "ordsets", "timer"]
            .iter()
            .map(|module| {
                (
                    *module,
                    root.join("lib/stdlib/src").join(format!("{module}.erl")),
                )
            })
            .collect();
        for (module, src) in &sources {
            if !src.is_file() {
                crate::context::test_support::skip_loudly(
                    "gc-walk seeding cost",
                    &format!("no stdlib source to compile at {} for `{module}`", src.display()),
                );
                return;
            }
        }

        for (module, src) in &sources {
            let mut runtime = booted_runtime();
            let out = crate::context::test_support::scratch_dir(&format!("gc_walk_cost_{module}"));
            let expr = format!(
                "compile:file({:?}, [no_spawn_compiler_process, deterministic, no_docs, \
                 {{outdir, {:?}}}])",
                src.to_str().expect("an ASCII source path"),
                out.path().to_str().expect("an ASCII temporary path"),
            );

            seed_counters::reset();
            let started = std::time::Instant::now();
            let result = runtime
                .eval(&expr)
                .unwrap_or_else(|error| panic!("the in-process `{module}` compile: {error}"));
            let elapsed = started.elapsed();
            let counts = seed_counters::snapshot();
            let rendered = runtime
                .format_term_w(result, None)
                .expect("the compile result prints");
            assert_eq!(
                rendered.trim(),
                format!("{{ok,{module}}}"),
                "the in-process compile of `{module}` must answer `{{ok,{module}}}`"
            );

            let walks = counts.walks.max(1) as f64;
            eprintln!(
                "gc-walk seeding {module} ({} lines): {:.0} ms | walks {} | roots/walk {:.1} | \
                 register words/walk {:.1} | root-buffer allocations {} | collections {}",
                std::fs::read_to_string(src)
                    .map(|text| text.lines().count())
                    .unwrap_or(0),
                elapsed.as_secs_f64() * 1e3,
                counts.walks,
                counts.root_words as f64 / walks,
                counts.register_words as f64 / walks,
                counts.buffer_allocations,
                runtime.gc_stats().collections,
            );
            // The buffer grows a handful of times as the deepest root set it meets grows; it is
            // **not** allocated per walk (which is what a per-call `Vec::new()` did).
            assert!(
                counts.buffer_allocations <= 32,
                "{module}: the root buffer was allocated {} times over {} walks — it is not being \
                 reused",
                counts.buffer_allocations,
                counts.walks
            );
        }
    }
}