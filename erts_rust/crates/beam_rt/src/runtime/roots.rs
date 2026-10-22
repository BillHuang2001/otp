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

//! The host-side **mutable** root enumeration — the door a relocating pass walks.
//!
//! [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots) answers *which
//! terms the runtime keeps alive*: it pushes read-only `Eterm` words into a collector's root
//! set. This module is that gather's **writable twin**. It hands a visitor the very same terms
//! as mutable slices, so a future relocation/compaction pass can rewrite a root word **in
//! place** — instead of only learning that the word exists — and the owner writes the rewritten
//! buffer back into its own table before the next borrow.
//!
//! The families are per owner because that is where the write-back has to happen: only the owner
//! knows the shape it gathered from (a `Vec` of key/value pairs, a FIFO queue of envelope words,
//! a register triple, an `Eterm`-keyed map). Every family therefore exposes one
//! `for_each_root_mut`-shaped seam, and the runtime-level traversal
//! ([`Runtime::for_each_root_family_mut`]) is the one place that visits them all in a fixed order.
//!
//! # The three contracts every family honours
//!
//! **(a) Every slice is a set of LIVE TERM SLOTS.** Each element is a real term a caller may
//! dereference — tag it, match it, follow its pointers. This is deliberately *not* the
//! word-image discipline of
//! [`Runtime::for_each_off_heap_region`](crate::Runtime::for_each_off_heap_region), whose words
//! must never be dereferenced because a region also holds the raw payload bytes of every on-heap
//! binary it carries (`binary:copy/2`'s `0x01` bytes read as `0x0101_0101_0101_0101`, low bits
//! `TAG_PRIMARY_LIST`) — treating such a word as a term is a memory-safety defect, not an
//! approximation. A root slot is a term by construction, so a relocation pass may inspect it
//! safely.
//!
//! **(b) The visitor may REPLACE elements, never reshape the slice.** It may write any element
//! (that is the whole point) but it must not change the slice's **length**, reorder it (no
//! sorting, no swapping), or depend on any hash/iteration order — a family backed by a
//! `HashMap`/`HashSet` (or any other unordered container) buffers its words into a `Vec` and
//! scatters them back positionally, so the order a visitor sees is an implementation detail of
//! the gather, not a property it may rely on. The `usize` token a gather reports (a start offset,
//! or a register's live-`x` count) is the *only* structural information the write-back needs, and
//! the visitor never sees it.
//!
//! **(c) One slice at a time.** A family is borrowed to visit exactly one slice, the visitor runs,
//! the buffer is written back, and only then is the next family (or the next container within a
//! family) borrowed. No two runtime borrows are ever outstanding at once, and a `RefCell` borrow
//! is **never** held across the visitor: a visitor that needs to reach another table (to resolve a
//! term, say) must be able to do so without tripping a double borrow. That is why the seams take
//! `&mut self` on the runtime's own table and hand over an owned buffer rather than lending a view
//! into the stored container.
//!
//! # An `Err` aborts the walk, and the owner still writes its buffer back first
//!
//! A visitor answers [`beam_common::Result<()>`]; `Err` means "stop, this pass cannot continue".
//! The family writes its (possibly half-rewritten) buffer back into the owning container **before**
//! propagating the error, so a slot the visitor had already replaced is never lost — the container
//! and the error leave together. A family that keeps its terms inside a queue (the mailbox)
//! re-installs each message as it goes, so its write-back is the re-enqueue; a message whose
//! visitor failed is re-installed exactly as it was drained, which makes each message
//! all-or-nothing.
//!
//! # [`RootFamily`] — the tag, and the value-gather parity rule
//!
//! Every visited slice carries the [`RootFamily`] it belongs to, so a traversal can report what it
//! saw and — more importantly — so the corpus can be *checked*: [`RootFamily::in_value_gather`]
//! records which families [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots)
//! walks **today**, and the parity test (`src/runtime/root_tests.rs`) asserts both
//! directions — the value gather's corpus is exactly the multiset union of the `true` families,
//! and every `false` family is a documented, intentional superset member (a root the moving pass
//! must rewrite but the conservative gather deliberately skips).

mod process;

use std::rc::Rc;

use beam_process::ProcessId;
use beam_term::prelude::Eterm;

use self::process::ProcessRootsMut;
use super::{PendingSpawn, Runtime};
use crate::runner::ProcessEntry;

/// Which root family a slice handed to a visitor belongs to.
///
/// The tag names the **owner** and the container, not the kind of term: `ProcessDict` is "the
/// per-process dictionary's key/value pairs", `ProcessMailbox` is "the queued messages'
/// `[payload, from, token]` words". Two families may therefore hold identical words for a while
/// (a term stored in a dictionary *and* queued in a mailbox); a traversal visits both, and the
/// relocation it performs is idempotent per word.
///
/// Ordering is arbitrary (it is a `derive`d total order, useful to sort a report); nothing may
/// depend on it semantically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum RootFamily {
    /// The capture environment of an escaped `-eval` closure
    /// (`Runtime::eval_closures`' `FunBody::env_terms`).
    EvalClosureEnv,
    /// The `-eval` compile cache's **word copy** of a closure's capture environment
    /// (`Runtime::eval_compiled`'s `CompiledClosure::captures`).
    ///
    /// It is a *second* copy of [`RootFamily::EvalClosureEnv`]'s words, and no value gather walks
    /// it — a payload census reaches it transitively, but a **moving** pass must update both copies
    /// in lockstep or one of them keeps pointing at the evacuated address.
    EvalCompiledCaptures,
    /// The sequential-trace tokens' `label`/`from` words (`SeqTrace::for_each_root_mut`), stored
    /// unstabilized, so a label may name an off-heap payload this state alone keeps alive.
    SeqTrace,
    /// The per-process dictionary's key/value pairs (`Dict::gather_roots` — two words per entry,
    /// key then value).
    ProcessDict,
    /// The process mailbox's queued messages (`Message::payload`/`from`/`token` per message, in
    /// FIFO order).
    ProcessMailbox,
    /// A process's register triple: `x[0..live)`, the whole Erlang stack, then the host-published
    /// extra roots (`Registers::gather_roots`), in that fixed order.
    ProcessRegisters,
    /// The saved BIF/NIF arguments (`SavedArgs::gather_roots`).
    ProcessSavedArgs,
    /// The pending-signal queue: one reason word per signal, plus the tag word of a tagged
    /// monitor-`DOWN` (`SignalQueue::gather_roots`).
    ProcessSignals,
    /// A spawn recorded but not yet absorbed into the process table: the child's arguments plus
    /// its entry fun.
    PendingSpawn,
    /// A live engine's arena-stable arguments, before the child has consumed them
    /// (`RunnerState::gather_off_heap_roots`).
    EngineArgs,
    /// The root set of a machine parked in an **un-published** (relocation-exempt) window — the
    /// only family that names a live term of a process whose `receive` park published nothing into
    /// its registers (`RunnerState::gather_off_heap_roots`).
    ParkedMachine,
    /// A pending `erlang:send_after` delivery's `from` and `message`
    /// (`Timers::for_each_delivery_root_mut`).
    TimerDelivery,
    /// A pending timer's `#Ref<>` handle (`Timers::for_each_reference_root_mut`).
    ///
    /// Outside the value gather, like every `Eterm`-**keyed** index in the runtime: a reference's
    /// identity *is* its address, so a key a visitor replaced would have to be *re-keyed* rather
    /// than rewritten in place — a different operation from the one this door offers.
    TimerReference,
    /// A `persistent_term` entry's key and value (`PersistentTerm::for_each_root_mut`).
    PersistentTermEntry,
    /// A trace `MatchSpec::Functions` word set (under the documented "a stabilized copy flattens
    /// a payload" exemption).
    TraceMatchSpec,
    /// A mailbox receive marker's bound `reference` word.
    MailboxMarker,
    /// A recorded runner result (`RunnerState::results`), stabilized off the dying process's heap
    /// before it was recorded.
    RunnerResult,
    /// A recorded runner exit (`RunnerState::exits`), stabilized the same way.
    RunnerExit,
    /// A built-in driver port's stored term data (`PortRegistry`'s `port_set_data` payload and the
    /// driver's emitted terms).
    PortData,
    /// A queued/pending port event's terms (`PortRegistry::for_each_pending_event_root_mut`).
    PortPendingEvent,
    /// An `enif_select` payload (`crate::nif_select`).
    SelectPayload,
    /// A distribution link's queued outbound control message
    /// (`DistClient::for_each_queued_root_mut`).
    DistQueuedMessage,
    /// A servable `erlang:garbage_collect/0,1` request's recorded reference id
    /// (`Runtime::request_gc`).
    AsyncGcRequestId,
}

impl RootFamily {
    /// Whether [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots) — the
    /// read-only value gather — walks this family **today**.
    ///
    /// `true` means the family is part of the conservative root set the runtime already publishes
    /// to a collector; `false` means the family is a documented member of the *moving* pass's
    /// corpus that the value gather deliberately skips (because its words cannot be dereferenced
    /// as terms, because they are only a copy of another family's words, or because the store is
    /// `Eterm`-keyed and needs re-keying rather than rewriting).
    ///
    /// This is the parity rule the corpus is checked against: the later `src/runtime/root_tests.rs`
    /// compares the value gather's output against the **multiset union of the `true` families** and
    /// asserts that the `false` ones are exactly the documented superset — no family may be
    /// silently added to, or dropped from, either side. The comparison is a set/multiset property,
    /// never an ordering one (the two walks deliberately visit the families in different orders:
    /// the value gather pushes the dictionary first, the moving pass follows `beam_process`'s own
    /// canonical order).
    //
    // A **lint allowance only**, not a behaviour claim: the parity suite
    // (`src/runtime/root_tests.rs::the_family_walk_matches_the_value_gather`) is the sole reader of
    // this rule today, so a non-test build sees it as dead code; a relocation/compaction pass
    // consulting it is the intended production caller.
    #[allow(dead_code)]
    pub(crate) fn in_value_gather(self) -> bool {
        match self {
            RootFamily::EvalClosureEnv
            | RootFamily::SeqTrace
            | RootFamily::ProcessDict
            | RootFamily::ProcessMailbox
            | RootFamily::ProcessRegisters
            | RootFamily::ProcessSavedArgs
            | RootFamily::ProcessSignals
            | RootFamily::PendingSpawn
            | RootFamily::EngineArgs
            | RootFamily::ParkedMachine
            | RootFamily::TimerDelivery => true,

            RootFamily::EvalCompiledCaptures
            | RootFamily::TimerReference
            | RootFamily::PersistentTermEntry
            | RootFamily::TraceMatchSpec
            | RootFamily::MailboxMarker
            | RootFamily::RunnerResult
            | RootFamily::RunnerExit
            | RootFamily::PortData
            | RootFamily::PortPendingEvent
            | RootFamily::SelectPayload
            | RootFamily::DistQueuedMessage
            | RootFamily::AsyncGcRequestId => false,
        }
    }
}

/// Re-tags a family-blind visitor with the [`RootFamily`] of the owner it is being lent to.
///
/// Every owner seam takes a plain `FnMut(&mut [Eterm]) -> Result<()>` (an owner has no reason to
/// know the tag vocabulary), while the runtime-level traversal's visitor is family-tagged. This is
/// the one adapter between the two shapes, so each step of the walk is a single uniform call
/// instead of a hand-written closure per family.
fn tagged<'a, F>(
    f: &'a mut F,
    family: RootFamily,
) -> impl FnMut(&mut [Eterm]) -> beam_common::Result<()> + 'a
where
    F: FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
{
    move |words| f(family, words)
}

impl PendingSpawn {
    /// Visits the two term families a recorded-but-not-yet-absorbed spawn holds: the child's
    /// argument vector and, for a [`ProcessEntry::Fun`] entry, the entry fun itself.
    ///
    /// Both slices are tagged [`RootFamily::PendingSpawn`] (the family is the record, not the
    /// field), so a visitor can tell a spawn's words from a process's. This mirrors
    /// [`Runtime::gather_off_heap_roots`]'s spawn clause exactly — arguments first, then the fun,
    /// and no term at all for a [`ProcessEntry::Export`] entry (its `module`/`function` are an
    /// `Arc` and an atom index, never a heap word).
    ///
    /// The spawn lives on the arena before it lives in the process table, so these words are named
    /// by no process yet — which is why they must be enumerated here: a spawn that has been
    /// recorded but not absorbed would otherwise have no writable slot at all.
    ///
    /// Slice lengths never change; an empty argument vector is skipped (a spawn with no arguments
    /// still visits its fun), and an `Err` stops the enumeration and is returned.
    pub(crate) fn for_each_root_mut(
        &mut self,
        f: &mut impl FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
    ) -> beam_common::Result<()> {
        if !self.args.is_empty() {
            f(RootFamily::PendingSpawn, &mut self.args)?;
        }
        if let ProcessEntry::Fun { fun, .. } = &mut self.entry {
            f(RootFamily::PendingSpawn, core::slice::from_mut(fun))?;
        }
        Ok(())
    }
}

impl Runtime {
    /// Hands `f` the writable root slots of every host-side root, one slice at a time.
    ///
    /// The family-blind form of [`Runtime::for_each_root_family_mut`] — same order, same slices,
    /// same contract — for a visitor that does not care which owner a slice belongs to (a
    /// relocation rewrites a word the same way wherever it is stored).
    ///
    /// # Errors
    ///
    /// Whatever `f` answers; the walk stops there, the owner of the failing slice has already
    /// written its buffer back, and every word `f` had replaced before that is in place.
    // The parity test (`src/runtime/root_tests.rs`) and the store move
    // (`crate::runtime::reclaim::compact`) are this door's two callers: the first compares the
    // walk's corpus against the value gather's in every populated state, the second forwards every
    // slot so a moved store region leaves each root naming the destination.
    pub(crate) fn for_each_root_mut(
        &mut self,
        f: &mut impl FnMut(&mut [Eterm]) -> beam_common::Result<()>,
    ) -> beam_common::Result<()> {
        self.for_each_root_family_mut(&mut |_family, words| f(words))
    }

    /// The runtime-level **mutable** root door: hands `f` the writable term slots of every
    /// host-side root this runtime holds, tagged with the [`RootFamily`] each slice belongs to.
    ///
    /// This is the *moving* twin of
    /// [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots), which answers the
    /// read-only question ("which terms does the runtime keep alive?"). Here the same owners lend
    /// the words themselves, so a relocation can rewrite a root in place instead of only learning
    /// that it exists; [`crate::runtime::reclaim::compact`] is that relocation. It **decides
    /// nothing**: it collects nothing, retains nothing, prunes nothing — visiting a word has no
    /// effect beyond the writes the visitor itself makes.
    ///
    /// # Coverage — the traversal order
    ///
    /// One slice (or a run of slices) per owner, in this fixed order:
    ///
    /// 1. [`RootFamily::EvalClosureEnv`] — every escaped `-eval` closure's capture environment
    ///    (`Runtime::eval_closures`, one `Rc<FunBody>` per closure). The body is reached through
    ///    `Rc::make_mut`, i.e. a clone-on-write split of a shared body; that is sound **because
    ///    this walk runs only at the runtime's two non-running sites** (the `-eval` statement
    ///    boundary and a nested run's epilogue), never while a closure is being applied, so no
    ///    application can hold a borrow of the body a split would desynchronize.
    /// 2. [`RootFamily::EvalCompiledCaptures`] — the `-eval` compile cache's word **copy** of the
    ///    same captures (`Runtime::eval_compiled`). It must be rewritten in the same pass as (1):
    ///    see that family's own documentation.
    /// 3. [`RootFamily::SeqTrace`] — the sequential-trace tokens' `[label, from]` words.
    /// 4. [`RootFamily::ProcessRegisters`], [`RootFamily::ProcessMailbox`],
    ///    [`RootFamily::ProcessDict`], [`RootFamily::ProcessSavedArgs`],
    ///    [`RootFamily::ProcessSignals`] — per live process, its complete published root set, in
    ///    `beam_process`'s own canonical collection order (`ProcessRootsMut::for_each_root_mut`).
    /// 5. [`RootFamily::MailboxMarker`] — each pid's `recv_marker_*` markers: the opaque marker
    ///    term and, while bound, its reference.
    /// 6. [`RootFamily::PendingSpawn`] — a spawn recorded but not yet absorbed: its argument vector
    ///    and its entry fun.
    /// 7. [`RootFamily::EngineArgs`] — an engine's arena-stable arguments (plus a
    ///    `ProcessEntry::Fun` entry's fun) that its child has not consumed yet.
    /// 8. [`RootFamily::ParkedMachine`] — the live root set (x registers, Erlang stack, FR prefix)
    ///    of a machine parked in an **un-published** (relocation-exempt) window.
    /// 9. [`RootFamily::RunnerResult`], [`RootFamily::RunnerExit`] — the recorded outcomes of a
    ///    finished/dying process (`RunnerState::results`/`RunnerState::exits`), stabilized off the
    ///    dying process's heap when they were recorded.
    /// 10. [`RootFamily::TimerDelivery`] — a pending `erlang:send_after`'s `[from, message]`.
    /// 11. [`RootFamily::TimerReference`] — a pending timer's `#Ref<>` handle.
    /// 12. [`RootFamily::PersistentTermEntry`] — a `persistent_term` entry's `[key, value]`.
    /// 13. [`RootFamily::TraceMatchSpec`] — an installed function trace pattern's
    ///     `MatchSpec::Functions` word vector.
    /// 14. [`RootFamily::PortPendingEvent`] — the port layer's queued/pending event terms (the
    ///     unreported `pending_events` buffer, held line frames, each port's driver `TermQueue`).
    /// 15. [`RootFamily::PortData`] — the `erlang:port_set_data/2` payloads.
    /// 16. [`RootFamily::SelectPayload`] — each armed select registration's handle + payload.
    /// 17. [`RootFamily::DistQueuedMessage`] — a distribution link's queued outbound control
    ///     message (its target-node atom and its argument vector).
    /// 18. [`RootFamily::AsyncGcRequestId`] — a servable `erlang:garbage_collect` request's
    ///     recorded reference id.
    ///
    /// # The documented superset (families the value gather skips)
    ///
    /// Steps (2), (5), (9), (11)–(18) are **not** in
    /// [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots)
    /// ([`RootFamily::in_value_gather`] is the machine-checkable record). Each is nonetheless a
    /// genuine root, for one of three reasons:
    ///
    /// * **A second copy of another family's words** — (2) `EvalCompiledCaptures`. A moving pass
    ///   must rewrite both copies; a conservative prune needs neither (it reaches the payload
    ///   through the closure the copy was made from).
    /// * **A copy whose words are already stable, but which is read again after the owner died** —
    ///   (9) `RunnerResult`/`RunnerExit`. The prune releases refc payloads by identity and needs no
    ///   copy; a mover does, because the recorded reason is read long after the process is gone.
    /// * **A store the value gather deliberately leaves to another mechanism** — (5) mailbox
    ///   markers (the marker's reference/immediate words name no payload), (11) a timer's reference
    ///   handle, (12) `persistent_term` (the storing BIFs stabilize — i.e. flatten — every value),
    ///   (13) trace match specs (stabilized the same way), (14)–(16) the port layer (its words are
    ///   either stabilized or a driver's own), (17) a dist queue (its words live on the client's
    ///   own scratch heap, reset only when the queue empties), (18) an async GC request id (a
    ///   reference term: an all-immediate leaf). In every one of these cases a *liveness* answer is
    ///   already correct without the family, while a *moving* answer is not: the word is stored and
    ///   will be read again, so its slot must be rewritten when the region it names moves.
    ///
    /// # The visitor contract
    ///
    /// * `f` may **replace any element** of a slice it is handed — that is the whole point.
    /// * `f` must **never change a slice's length**, reorder it, or depend on any container's
    ///   hash/iteration order. An unordered owner buffers its words into a `Vec` and scatters them
    ///   back positionally, so the order a visitor sees is an implementation detail of the gather.
    /// * A slice is **one family of one owner**: a visitor that wants to treat a family as a whole
    ///   (a word's position within a register triple, say) gets it whole and never glued to
    ///   another owner's words.
    /// * **One owner at a time.** No two of the runtime's own borrows are outstanding at once, and
    ///   an `Rc<RefCell<…>>` store (`persistent_terms`, `ports`) is borrowed for exactly the single
    ///   statement that visits it — released before the next step, so a visitor that re-enters
    ///   such a store cannot trip a double borrow.
    /// * An `Err` **aborts the walk** and is returned as-is. The owner of the failing slice writes
    ///   its (possibly half-rewritten) buffer back **first**, so a slot the visitor had already
    ///   replaced is never lost: the container and the error leave together. Families already
    ///   visited keep every write they were given.
    ///
    /// # Completeness — this walk is whole only at a non-running site
    ///
    /// It is **only complete** at the two sites where nothing is executing: the `-eval` statement
    /// boundary and a nested run's epilogue. During a scheduler pass `runner_state`, `processes`
    /// and the scheduler are moved out of the [`Runtime`] for the pass's duration, so the coverage
    /// argument — and the corresponding gap — is exactly the one
    /// [`Runtime::gather_off_heap_roots`](crate::Runtime::gather_off_heap_roots) states in its
    /// `# Residuals` section: a **running** machine's own root set cannot be enumerated from here,
    /// and the in-pass park arm publishes it instead
    /// ([`crate::context::gc::resources::reclaim`]).
    ///
    /// Kept honest by `src/runtime/root_tests.rs::the_family_walk_matches_the_value_gather`, which
    /// compares the value gather's corpus against the multiset union of these families on a
    /// fully-owned runtime and asserts that the families
    /// [`RootFamily::in_value_gather`] marks `false` are exactly the documented superset.
    ///
    /// # Deliberately not enumerated — the relocation checklist's remaining entries
    ///
    /// These holders are **known** and reported rather than visited, each for a reason that makes a
    /// slot impossible or meaningless. A mover must handle them another way (or not at all):
    ///
    /// * The `-eval` evaluator's **environment** and its Rust-stack locals. The evaluator's `Env`
    ///   is a Rust local of `src/eval`, so no `&mut` slot of this walk reaches it — but the caller
    ///   that owns it hands its value slots over separately, as
    ///   [`MoveSlots`](crate::runtime::reclaim::MoveSlots) (`crate::eval::scratch::boundary` /
    ///   `close`). Its deeper stack frames (a partially built term inside a nested `expr`) are
    ///   never live at either site a move runs at. The whole `-eval` driver also runs under
    ///   [`RuntimeContext::with_relocation_exempt`](crate::context::RuntimeContext::with_relocation_exempt)
    ///   (`src/eval/mod.rs`), which is why a *process*-heap term the evaluator holds in a Rust local
    ///   cannot move.
    /// * The **word-image** regions — `eval_heap`, `eval_heap_retired`, `bif_heaps`, `info_heap`.
    ///   They are word images, not term arrays (a region also carries the payload bytes of every
    ///   on-heap binary it holds), and they are enumerated by
    ///   [`Runtime::for_each_off_heap_region`](crate::Runtime::for_each_off_heap_region), whose
    ///   visitor must not dereference a word. Handing them to *this* visitor would be a
    ///   memory-safety defect. (The store move reaches the store's *words* a different way — through
    ///   `beam_gc`'s relocator, which the sources' `FromSet` and the destination region describe.)
    /// * `RunnerState::errors` — a `BeamError` may carry a live reason word
    ///   ([`beam_common::Raise`]'s opaque `live_reason` slot), but `BeamError` exposes no mutable
    ///   term accessor; reaching it needs an upstream `beam_common` seam. The store move therefore
    ///   **declines** a candidate range such a word stands in rather than rewriting it.
    /// * **Address-valued bookkeeping** a mover must *re-key* or *recompute* rather than rewrite:
    ///   `SelectWatch::by_fd_handle` (`SelectWatch::rekey_handles`, called by a move's commit),
    ///   and — the fact behind it — `Eterm`'s `Hash`/`Eq` are the raw word, so every `Eterm`-keyed
    ///   map in the runtime is keyed by **address**. `Runtime::references`, `Timers::refs` and
    ///   `TimerWakes::deliver` need no re-keying, and structurally rather than by judgement: each
    ///   key is minted on a **pooled BIF heap** (`src/native/process.rs`'s `reference_term`,
    ///   `src/native.rs`'s `mint_timer_ref`), which is a different region family from the store —
    ///   see [`crate::runtime::reclaim::compact`]'s *The address-keyed maps*.
    /// * `InMachinePosition` (an in-flight window no collection may run inside) and the per-call
    ///   NIF hosts (`NifHostImpl::envs`, `PendingNifSend`), which exist only inside a callback
    ///   window.
    /// * The distribution/payload scratch `Heap`s — regions owned by other crates with no
    ///   word-range accessor.
    ///
    /// The engine arena is no longer in that list: `beam_engine::MinimalContext` now exposes
    /// `heap_mark`/`heap_mark_matches`/`for_each_heap_region`/`heap_rewind`, and
    /// [`crate::runtime::reclaim`] is their production caller.
    ///
    /// # Errors
    ///
    /// Whatever `f` answers (see the visitor contract above).
    //
    // Two callers: the parity suite (`src/runtime/root_tests.rs::the_family_walk_matches_the_value_gather`)
    // and the store move (`crate::runtime::reclaim::compact`, which forwards every slice so a moved
    // region leaves each root naming the destination).
    pub(crate) fn for_each_root_family_mut(
        &mut self,
        f: &mut impl FnMut(RootFamily, &mut [Eterm]) -> beam_common::Result<()>,
    ) -> beam_common::Result<()> {
        // 1. The escaped `-eval` closures' capture environments. `Rc::make_mut` splits a shared
        //    body out on write, which is sound only at a non-running site (see the docs above).
        for body in &mut self.eval_closures {
            Rc::make_mut(body)
                .for_each_root_mut(&mut tagged(&mut *f, RootFamily::EvalClosureEnv))?;
        }
        // 2. The compile cache's word copy of those same captures. It must be rewritten in the
        //    same pass as (1) or one of the two copies keeps naming the evacuated address.
        for closure in self.eval_compiled.values_mut() {
            let Some(closure) = closure.as_mut() else {
                continue;
            };
            closure.for_each_root_mut(&mut tagged(&mut *f, RootFamily::EvalCompiledCaptures))?;
        }
        // 3. The sequential-trace tokens.
        self.seq_trace
            .for_each_root_mut(&mut tagged(&mut *f, RootFamily::SeqTrace))?;
        // 4. Every live process's complete published root set. `pids()` borrows the table, so the
        //    list is cloned out first (which is also what lets the loop below take `&mut` on the
        //    same table, and what the later `live` filter is built from).
        let mut live_pids: Vec<ProcessId> = self.processes.pids().collect();
        live_pids.sort_unstable();
        for &pid in &live_pids {
            let Some(process) = self.processes.get_mut(pid) else {
                continue;
            };
            ProcessRootsMut::for_each_root_mut(process, &mut *f)?;
        }
        // 5. The receive markers: the opaque marker term and, while bound, its reference.
        for state in self.mailboxes.values_mut() {
            state.for_each_marker_root_mut(&mut tagged(&mut *f, RootFamily::MailboxMarker))?;
        }
        // 6. A spawn recorded but not yet absorbed into the process table.
        for spawn in &mut self.pending_spawns {
            spawn.for_each_root_mut(&mut *f)?;
        }
        // 7.-9. The runner state: an engine's un-consumed arguments, an un-published parked
        //    machine's root set (gated on the pid still being live), and the two recorded-outcome
        //    maps. The `live` predicate is a binary search over the sorted pid list above — a
        //    `Vec::contains` would be the same answer at O(n) per engine.
        let live = |pid: ProcessId| live_pids.binary_search(&pid).is_ok();
        self.runner_state
            .for_each_engine_args_root_mut(&mut tagged(&mut *f, RootFamily::EngineArgs), live)?;
        self.runner_state.for_each_parked_machine_root_mut(
            &mut tagged(&mut *f, RootFamily::ParkedMachine),
            live,
        )?;
        self.runner_state
            .for_each_result_root_mut(&mut tagged(&mut *f, RootFamily::RunnerResult))?;
        self.runner_state
            .for_each_exit_root_mut(&mut tagged(&mut *f, RootFamily::RunnerExit))?;
        // 10.-11. The pending timers: each delivery's `[from, message]`, then each reference
        //    handle. Two sequential borrows of the one `timers` field.
        self.timers
            .for_each_delivery_root_mut(&mut tagged(&mut *f, RootFamily::TimerDelivery))?;
        self.timers
            .for_each_reference_root_mut(&mut tagged(&mut *f, RootFamily::TimerReference))?;
        // 12. `persistent_term`: the store is behind a shared `RefCell`, so the borrow is scoped to
        //    this single statement (released before the next store is touched, so a visitor that
        //    re-enters the store cannot trip a double borrow).
        {
            self.persistent_terms
                .borrow_mut()
                .for_each_root_mut(&mut tagged(&mut *f, RootFamily::PersistentTermEntry))?;
        }
        // 13. The installed function trace patterns.
        self.trace
            .for_each_match_spec_root_mut(&mut tagged(&mut *f, RootFamily::TraceMatchSpec))?;
        // 14.-16. The port layer, likewise behind a shared `RefCell` and therefore in one scoped
        //    borrow: pending event terms, `port_set_data` payloads, armed select registrations.
        {
            let mut ports = self.ports.borrow_mut();
            ports.for_each_pending_event_root_mut(&mut tagged(
                &mut *f,
                RootFamily::PortPendingEvent,
            ))?;
            ports.for_each_port_data_root_mut(&mut tagged(&mut *f, RootFamily::PortData))?;
            ports.for_each_select_root_mut(&mut tagged(&mut *f, RootFamily::SelectPayload))?;
        }
        // 17. A distribution link's queued outbound control messages.
        if let Some(client) = self.dist_client.as_mut() {
            client.for_each_queued_root_mut(&mut tagged(&mut *f, RootFamily::DistQueuedMessage))?;
        }
        // 18. A servable `erlang:garbage_collect` request's recorded reference id.
        self.gc
            .for_each_pending_completion_root_mut(&mut tagged(&mut *f, RootFamily::AsyncGcRequestId))?;
        Ok(())
    }
}