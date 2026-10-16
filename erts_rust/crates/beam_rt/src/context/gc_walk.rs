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

// ============================ END TEMPORARY POLICY SWITCH ============================

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
#[derive(Default)]
struct Visited {
    /// The table's slots, each a key and the generation that wrote it.
    slots: Vec<Slot>,    /// The generation of the walk in progress.
    generation: u32,
    /// How many slots the current generation occupies.
    len: usize,
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

/// The **odd** 64-bit multiplicative constant of the Fibonacci hash —— the integer nearest
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
        let keys: Vec<usize> = self
            .slots
            .iter()
            .filter(|slot| slot.generation == self.generation)
            .map(|slot| slot.key)
            .collect();
        self.resize_table(self.slots.len() * 2);
        #[cfg(test)]
        {
            self.grows += 1;
        }
        // The re-insert cannot re-enter `grow`: the table is twice one that was at most
        // three-quarters full, so the resulting load is at most three-eighths.
        for key in keys {
            let _ = self.insert(key);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // ---------------------------------------------------------------------------------------
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
            assert_eq!(visited.slots.len(), slots, "a shallow walk resized the table");
            assert_eq!(visited.grows, grows, "a shallow walk rebuilt the table");
        }
    }
}
