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
//! `for_each_root_mut`-shaped seam, and the runtime-level traversal (a later change) is the one
//! place that visits them all in a fixed order.
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
//! walks **today**, and the parity test (a later change, `src/runtime/root_tests.rs`) asserts both
//! directions — the value gather's corpus is exactly the multiset union of the `true` families,
//! and every `false` family is a documented, intentional superset member (a root the moving pass
//! must rewrite but the conservative gather deliberately skips).

mod process;

// Re-exported for the runtime-level `Runtime::for_each_root_family_mut` traversal, which lands as
// a separate change — so until then nothing in the crate names the trait.
#[allow(unused_imports)]
pub(crate) use self::process::ProcessRootsMut;

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
// The enum, its `in_value_gather` predicate and the seams that answer with it are written ahead
// of their consumer: the runtime-level `Runtime::for_each_root_family_mut` traversal (and its
// parity test) lands as a separate change, so until then nothing in the crate names them.
#[allow(dead_code)]
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
    #[allow(dead_code)] // consumed by the runtime-level root-enumeration door and its parity test
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