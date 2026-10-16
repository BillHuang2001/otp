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

use super::gc::root_is_unscanned;

/// The **reused** mutable state of the descent: its worklist and its visited set.
///
/// One lives in the runtime's GC state ([`crate::runtime::gc::GcState`]) and is handed to every
/// walk, so a collection **never allocates** the descent's structures (see [`Visited`] for the
/// `O(1)` per-walk reset that makes the reuse free). Both buffers only ever grow, so the steady
/// state of a running process is allocation-free after its first few walks.
#[derive(Default)]
pub(crate) struct WalkScratch {
    /// The objects still to expand (the worklist).
    pending: Vec<Eterm>,
    /// The objects already entered.
    visited: Visited,
}

/// Whether a collection of `process` would be **unsound** — whether a container in a region
/// the collector does not scan is reachable from one it does.
///
/// `machine_roots` are the words the caller is about to publish (the engine's x registers, the
/// whole Erlang stack and the whole FR file); the process's **own** root-set containers are
/// always included, because [`Process::collect`] gathers them on every collection. Passing an
/// over-approximation of the roots is always allowed: a spurious hit only **declines** a
/// collection that might have been sound (the allocation then falls back to the pointer-stable
/// arena), never allowing one that is not.
///
/// `descend` is the caller's **foreign-reachability gate** ([`WalkScratch`]'s module docs): when
/// it is `false` the descent is skipped and only the cheap `O(roots)` per-word phase runs. The
/// gate can only ever cost a collection (an unnecessary descent), never allow a wrong one: see
/// the module docs' soundness argument.
pub(crate) fn any_root_is_unscanned<'a, I>(
    scratch: &mut WalkScratch,
    process: &Process,
    machine_roots: I,
    descend: bool,
) -> bool
where
    I: IntoIterator<Item = &'a Eterm>,
{
    let mut walk = Walk::begin(scratch);
    for &word in machine_roots {
        walk.seed(process, word);
        if walk.hit {
            return true;
        }
    }
    walk.seed_process_roots(process);
    if walk.hit || !descend {
        return walk.hit;
    }
    walk.run(process)
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
/// pays the `O(live objects)` descent for a pid whose bit is clear.
pub(crate) type ForeignLatch = Rc<RefCell<HashSet<ProcessId>>>;

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

/// The walk's mutable state: the reused scratch plus whether an unscanned container has been
/// reached.
struct Walk<'a> {
    scratch: &'a mut WalkScratch,
    hit: bool,
}

/// The descent's **visited set**: the object addresses already expanded.
///
/// The descent runs over the process's whole live object graph (thousands of objects) on the
/// collections that reach it, so the set's per-key cost is the descent's dominant term. It is
/// therefore deliberately *not* a `HashSet`: it is an **open-addressing table keyed directly on
/// the object address** — a word-aligned term pointer, whose own bits already separate the
/// addresses of one allocation region, so no mixing beyond a shift is needed — and every slot
/// carries a **generation tag**, so a fresh walk "clears" the table in `O(1)` (one counter
/// increment) instead of touching one word per slot. That is what makes reusing the table across
/// walks free.
#[derive(Default)]
struct Visited {
    /// The key of each slot (an object address; `0` is never a live term address, but the
    /// *generation* is what marks a slot empty, so the value here is only meaningful when
    /// `generations[slot] == generation`).
    keys: Vec<usize>,
    /// The generation each slot was last written in.
    generations: Vec<u32>,
    /// The generation of the walk in progress.
    generation: u32,
    /// How many slots the current generation occupies.
    len: usize,
}

/// The table's initial slot count (a power of two). Grown (never shrunk) as a walk deepens, so
/// the table reaches the process's live-set size once and stays there.
const INITIAL_SLOTS: usize = 1024;

impl Visited {
    /// Starts a new walk: `O(1)`, without touching a single slot (the generation bump is the
    /// reset). The only path that touches every slot is the wrap of the `u32` generation
    /// counter, once every four billion walks.
    fn begin(&mut self) {
        if self.generation == u32::MAX {
            self.keys.clear();
            self.generations.clear();
            self.generation = 1;
        } else {
            self.generation += 1;
        }
        self.len = 0;
    }

    /// Records `key`; answers whether it is **newly** recorded (`false` when the walk in
    /// progress already holds it — which is what makes a shared or cyclic graph terminate).
    fn insert(&mut self, key: usize) -> bool {
        if self.keys.is_empty() {
            self.keys.resize(INITIAL_SLOTS, 0);
            self.generations.resize(INITIAL_SLOTS, 0);
        } else if (self.len + 1) * 4 > self.keys.len() * 3 {
            self.grow();
        }
        let mask = self.keys.len() - 1;
        let mut slot = Self::probe(key) & mask;
        loop {
            if self.generations[slot] != self.generation {
                self.keys[slot] = key;
                self.generations[slot] = self.generation;
                self.len += 1;
                return true;
            }
            if self.keys[slot] == key {
                return false;
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Doubles the table and re-inserts the current generation's keys (the only allocation the
    /// descent ever performs, and it amortises away).
    fn grow(&mut self) {
        let keys: Vec<usize> = (0..self.keys.len())
            .filter(|&slot| self.generations[slot] == self.generation)
            .map(|slot| self.keys[slot])
            .collect();
        let capacity = self.keys.len() * 2;
        self.keys.clear();
        self.keys.resize(capacity, 0);
        self.generations.clear();
        self.generations.resize(capacity, 0);
        self.len = 0;
        // The re-insert cannot re-enter `grow`: `capacity` is twice a table that was at most
        // three-quarters full, so the resulting load is at most three-eighths.
        for key in keys {
            let _ = self.insert(key);
        }
    }

    /// The initial probe index of `key`: its word index, with two higher address bits folded in
    /// so that a region-aligned allocation pattern does not probe a single congruence class.
    fn probe(key: usize) -> usize {
        (key >> 3) ^ (key >> 15)
    }
}

impl<'a> Walk<'a> {
    /// Starts a walk over `scratch`, resetting the reused state in `O(1)`.
    fn begin(scratch: &'a mut WalkScratch) -> Walk<'a> {
        scratch.pending.clear();
        scratch.visited.begin();
        Walk {
            scratch,
            hit: false,
        }
    }

    /// Classifies one root-set word: records a hit for a container in an unscanned region,
    /// enqueues a container in the scanned region, and ignores everything else.
    fn seed(&mut self, process: &Process, word: Eterm) {
        if self.hit {
            return;
        }
        // An immediate (a small integer, an atom, `NIL`, an internal pid/port id) has no
        // children: neither this walk nor the collector descends into it.
        if !is_boxed(word) && !is_list(word) {
            return;
        }
        // A literal-area pointer is a frozen code constant; see the module docs.
        if is_literal(word) {
            return;
        }
        // The per-word classifier: `true` for a container in a region the collector does not
        // scan, so the collection must be declined. Reused verbatim (never re-derived).
        if root_is_unscanned(process, word) {
            self.hit = true;
            return;
        }
        // A container the collector scans: descend into it (once — the visited set is what
        // makes a shared or cyclic graph terminate).
        if self.scratch.visited.insert(ptr_val(word) as usize) {
            self.scratch.pending.push(word);
        }
    }

    /// Seeds from every container [`Process::collect`] gathers: the registers (`x[0..live)`,
    /// the whole stack and the host-published extra roots), then each queued message's
    /// `payload`/`from`/`token`, the process dictionary, the saved arguments and the pending
    /// signals.
    fn seed_process_roots(&mut self, process: &Process) {
        let mut roots: Vec<Eterm> = Vec::new();
        process.registers().gather_roots(&mut roots);
        for message in process.mailbox().iter() {
            roots.push(message.payload());
            roots.push(message.from());
            roots.push(message.token());
        }
        for (key, value) in process.dict().entries() {
            roots.push(key);
            roots.push(value);
        }
        process.saved_args().gather_roots(&mut roots);
        process.signals().gather_roots(&mut roots);
        for word in roots {
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
        // SAFETY: `word` reaches the worklist only through `seed`, which enqueues exactly the
        // boxed/cons words that are (a) genuine terms and (b) addressed inside a region the
        // process's collector scans — i.e. live objects of the running process. `address`
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
