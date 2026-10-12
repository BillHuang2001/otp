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

//! The process/port table: pid allocation, the local name registry and the signal hub.
//!
//! [`ProcessTable`] is the **single place that mutates two or more processes at once** —
//! links, monitors and exit propagation inherently touch pairs of PCBs, and holding two
//! `&mut Process` out of one `HashMap` is a borrow-checker impossibility. Every such
//! operation therefore lives here and uses a **collect-then-apply worklist**: gather the
//! link targets / watchers, then mutate each target transiently. A [`Process`] itself
//! holds only its own sets (see the crate `CONTEXT.md`, Design Decisions).
//!
//! # Decoupled, ordered signals
//!
//! Exit/link/monitor signals are **never applied where they are generated**. A signalling
//! operation *enqueues* a [`PendingSignal`] on the receiver's own
//! [`SignalQueue`](crate::signal_queue::SignalQueue) — copying the reason into the receiver's
//! heap so it survives the signaller's death — and the signal is applied later:
//!
//! * **at the receiver's schedule-in**, through [`ProcessTable::service_signals`] (the
//!   scheduler calls it on the dequeued pid before running it); and
//! * **eagerly at the end of the sending operation**, for a receiver that is **not** the
//!   currently executing process (ERTS's early signal handling). "Currently executing" is
//!   [`ProcessState::Running`]: the single scheduler sets it before a runner runs, so at most
//!   one process is `Running` at a time, and a signal to that process is deferred to its next
//!   schedule-in. This choice is what preserves the observable end state of a synchronous
//!   propagation (a signal to a blocked/idle peer is still applied before the caller returns).
//!
//! Servicing drains a receiver's queue **in FIFO order** and applies each signal through
//! [`exit_policy`] (terminate / trap as `{'EXIT', …}` / ignore) or builds the `{'DOWN', …}`
//! message. A termination cascades through an explicit worklist (the pending-service queue),
//! never through recursion, so a link cycle (A↔B) terminates in a bounded number of steps.
//! [`crate::signal_queue`] states the exact ordering guarantee (and its limits).
//!
//! A local **link exit** and a local **monitor-DOWN** go through the queue. A **remote**
//! (external-pid) or **port** signal does not: it would carry no `trap_exit` policy decision
//! to defer, so `node_down`/`port_closed` materialise it straight into the mailbox.
//!
//! Every signal-builder path handles a full receiver heap exactly as
//! [`Process::deliver`](crate::process::Process::deliver) does: an **ordinary** receiver's nursery
//! is collected and grown **on demand** to cover the refused request (through
//! [`grow_receiver_for_signal`]) and the build is retried once, so an oversized
//! `{'EXIT', …}`/`{'DOWN', …}` reason is still delivered; a receiver the crate's own
//! [`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots) test covers —
//! a **relocation-exempt** one, whose heap must not move under the host's unpublished roots, or a
//! **`Running`** one, whose machine's `regs` snapshot is stale — **or** one whose relocating
//! collection the host's [`CollectionGate`] declines instead gets the signal delivered through a
//! **heap fragment** ([`enqueue_fragment_signal`]): no collection, no growth, no heap relocation,
//! the message simply does not live on the receiver's heap. That is the model the C always uses for
//! signals (`erl_proc_sig_queue.c`: a signal message is built into the message's own heap fragment,
//! `erts_alloc_message(hsz,&hp)` → `mp->hfrag.off_heap`).
//!
//! The [`CollectionGate`] is the **host's** half of the exhaustion policy: the crate knows a
//! process is `Running` or explicitly exempt, but only the host knows whether a receiver's live
//! root set is fully **scanned** by its collector. Installing a gate
//! ([`ProcessTable::set_collection_gate`]) lets it decline the *relocation* for such a receiver —
//! at every signal site and at [`ProcessTable::deliver`] — while the notification is still
//! delivered, through the fragment route.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_ptab.{c,h}` (the process/port
//! table and the local name registry), `erl_process.c` (`erts_ptab_new_process`) and
//! `erl_proc_sig_queue.c` (the signal queue this module reproduces).

use std::collections::{HashMap, VecDeque};

use beam_alloc::Heap;
use beam_common::{AllocFailure, AllocKind, BeamError, Result};
use beam_gc::HeapPolicy;
use beam_message::{Fragments, HeapFragment, Message, OffHeapTable};
use beam_term::prelude::{atom_val, is_atom, is_immediate, make_atom, Atom, Eterm};
use beam_term::size_object;

use crate::error;
use crate::exit_observer::{ProcessExit, ProcessExitObserver};
use crate::monitor::{
    payload_is_down_for, AliasEntry, Monitor, MonitorKind, MonitorRef, MonitoredBy, PortWatch,
    RefAllocator, UnaliasMode,
};
use crate::pid::{PortId, ProcessId, MAX_PROCESS_NUMBER};
use crate::port::PortTable;
use crate::process::{CollectionGate, DeliverOutcome, Process, ProcessState};
use crate::remote::{DistSignal, DistSignalRouter, PidTarget, RemoteMonitor, RemotePid};
use crate::run_queue::Priority;
use crate::signal::{
    build_down_message_port, build_down_message_remote, build_down_message_term,
    build_exit_message, build_exit_message_term, exit_policy, ExitPolicy, ExitSignal, SignalAtoms,
};
use crate::signal_queue::PendingSignal;

/// The outcome of applying an exit signal to a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitEffect {
    /// The target was terminated.
    Terminated,
    /// The target trapped the signal and received `{'EXIT', From, Reason}`.
    Trapped,
    /// The signal was ignored (a `normal` signal to a non-trapping process).
    Ignored,
}

/// One process-table slot: its current serial and the process it holds.
///
/// A slot is reused with a **bumped serial**, so a stale pid never resolves to the new
/// occupant (see [`crate::pid`]).
struct Slot {
    serial: u64,
    process: Option<Process>,
}

/// The target descriptor of a **named** monitor (`erlang:monitor(process, Name)`).
///
/// `Monitor`/`MonitoredBy` carry a [`ProcessId`] target only, and their shape is fixed (built
/// by struct literal), so the `{Name, LocalNode}` target a named monitor reports in its
/// `{'DOWN', …}` is kept beside them, keyed by the monitor reference, in
/// [`ProcessTable::named_monitors`].
///
/// Both fields are **atoms — immediates** — so this record introduces no GC rootset and never
/// holds a term that could be relocated by a collection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct NamedTarget {
    /// The registered name that was monitored.
    name: Atom,
    /// The node the name was resolved on (the local node, spelled into the DOWN target).
    node: Atom,
}

/// Borrows the live process named by `pid`, verifying the slot serial first.
fn lookup(slots: &[Slot], pid: ProcessId) -> Option<&Process> {
    let slot = slots.get(pid.number() as usize)?;
    if slot.serial != pid.serial() {
        return None;
    }
    slot.process.as_ref()
}

/// Borrows the live process named by `pid` mutably, verifying the slot serial first.
fn lookup_mut(slots: &mut [Slot], pid: ProcessId) -> Option<&mut Process> {
    let slot = slots.get_mut(pid.number() as usize)?;
    if slot.serial != pid.serial() {
        return None;
    }
    slot.process.as_mut()
}

/// The receive-side heap-exhaustion policy the signal builders share with
/// [`Process::deliver`](crate::process::Process::deliver).
///
/// Copying a signal onto a receiver's heap can be refused because the nursery is full — a
/// *normal* condition, not fatal OOM. An **ordinary** receiver's nursery is therefore collected
/// and grown **on demand** to cover the refused request
/// ([`Process::ensure_capacity`](crate::process::Process::ensure_capacity), seeded with the word
/// count the copier reported) and the caller retries once; `Ok(())` means "retry now".
///
/// A receiver whose live terms may be **outside** the crate root set — relocation-exempt,
/// [`ProcessState::Running`] (its own machine is executing, so the `regs` snapshot is stale), or
/// declined by the host's [`CollectionGate`] (`decline`) — must not collect or grow, so the raw
/// [`BeamError::Alloc`] refusal is returned **unchanged** (see
/// [`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots)). The first
/// two are purely **defensive**: every live caller tests `has_unpublished_roots` *first* and routes
/// such a receiver to the heap-fragment model ([`enqueue_fragment_payload`] — the apply-time
/// builders [`build_signal_message`]/[`deliver_head_signal`], and the enqueue-time reason copy
/// [`copy_reason_onto`]) without ever reaching here, so those branches are only reached if a new
/// caller forgets the pre-test. The `decline` case is different: it is a **per-receiver host
/// predicate**, so it is passed in here as well as pre-tested, and refusing here is what keeps the
/// guarantee "declining the relocation never moves a word" true even for the rare interleaving the
/// pre-test could not see.
fn grow_receiver_for_signal(
    process: &mut Process,
    failure: AllocFailure,
    decline: bool,
) -> Result<()> {
    if process.has_unpublished_roots() || decline {
        // The heap cannot be moved under the unpublished live terms: hand the refusal back
        // verbatim (`kind` still Heap/Message) — no collection, no growth, no retry.
        return Err(BeamError::Alloc(failure));
    }
    // The copier reports the whole graph's flat word count in the failure (as bytes), so the
    // words the growth must cover are recoverable from it.
    let need_words = failure.requested / core::mem::size_of::<Eterm>();
    // The growth's own error is deliberately ignored, exactly as `Process::deliver` documents:
    // when the policy's ceiling makes the request unreachable, `ensure_capacity` fails
    // fail-fast without touching the collector, and the caller's retry reproduces the refusal.
    // The retry's own result is authoritative — this is not a silent swallow.
    let _growth = process.ensure_capacity(need_words);
    Ok(())
}

/// Reserved words for the scratch arena a fragment-backed signal payload is built on.
const SIGNAL_SCRATCH_WORDS: usize = 64;

/// Builds a signal payload with `build` and enqueues it as `from` → `recipient`.
///
/// Exhaustion is the shared policy: a `Heap`/`Message` refusal is a normal condition —
/// an ordinary receiver collects and grows its nursery on demand and the build is retried
/// once ([`grow_receiver_for_signal`]), while a receiver whose live terms are **not** all in the
/// crate root set ([`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots)
/// — relocation-exempt, or `Running` with a stale `regs` snapshot) **or** whose relocating
/// collection the host's [`CollectionGate`] declines (`decline`) gets the heap-fragment model
/// instead.
fn build_signal_message<F>(
    recipient: &mut Process,
    from: Eterm,
    reason: Eterm,
    decline: bool,
    mut build: F,
) -> Result<()>
where
    F: FnMut(&mut Heap, Eterm) -> Result<Eterm>,
{
    match build(recipient.heap_mut(), reason) {
        Ok(term) => {
            recipient.mailbox_mut().enqueue(Message::new(term, from));
            Ok(())
        }
        Err(BeamError::Alloc(failure))
            if matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
        {
            if recipient.has_unpublished_roots() || decline {
                return enqueue_fragment_signal(recipient, from, reason, &mut build);
            }
            grow_receiver_for_signal(recipient, failure, decline)?;
            let term = build(recipient.heap_mut(), reason)?;
            recipient.mailbox_mut().enqueue(Message::new(term, from));
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Builds the signal payload on a scratch arena and copies the **whole** graph into a fresh
/// [`HeapFragment`] that the enqueued [`Message`] owns — the local-receiver analogue of the send
/// path's `send_fragmented` ([`beam_message`]), and the model the C always uses for signals
/// (`erl_proc_sig_queue.c` builds every signal message into the message's own heap fragment).
///
/// This is the exhaustion fallback for a receiver whose heap must not move — **relocation-exempt**,
/// `Running`, or declined by the host's [`CollectionGate`]: no collection, no
/// growth, no heap relocation — the message simply does not live on the receiver's heap.
///
/// `src_off_heap` describes the heap `reason` lives in — the table whose refc binaries/MSO the
/// copy must resolve, i.e. the C's *source* process. `None` means the **recipient's own** table,
/// which is the apply-time case (the queued signal's reason was already copied onto the
/// receiver's heap at enqueue time) and the self-signal case; `Some` is a *signaller's* table,
/// which the enqueue-time route passes because there the reason still lives in the signalling
/// process's heap.
fn enqueue_fragment_payload<F>(
    recipient: &mut Process,
    from: Eterm,
    reason: Eterm,
    src_off_heap: Option<&OffHeapTable>,
    build: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Heap, Eterm) -> Result<Eterm>,
{
    // `reason` may still point into a young heap (the receiver's, or the signaller's): build the
    // payload around it on a scratch arena, then copy the whole graph in one go.
    let mut scratch = Heap::new(SIGNAL_SCRATCH_WORDS)?;
    let payload = build(&mut scratch, reason)?;
    let mut fragment = HeapFragment::new(size_object(payload) + 8)?;
    // `copy_struct` walks the graph by pointer, so the heap-resident `reason` subtree is copied
    // in too; the source process's persistent off-heap table resolves its refc binaries/MSO.
    let src = src_off_heap.unwrap_or_else(|| recipient.off_heap());
    let root = fragment.copy_root(payload, src)?;
    let mut fragments = Fragments::new();
    fragments.push(fragment);
    recipient
        .mailbox_mut()
        .enqueue(Message::new(root, from).with_fragments(fragments));
    Ok(())
}

/// [`enqueue_fragment_payload`] for a payload whose `reason` already lives on the **receiver's**
/// own heap — the apply-time case, where the queued signal's reason was copied onto that heap
/// when the signal was enqueued.
fn enqueue_fragment_signal<F>(
    recipient: &mut Process,
    from: Eterm,
    reason: Eterm,
    build: &mut F,
) -> Result<()>
where
    F: FnMut(&mut Heap, Eterm) -> Result<Eterm>,
{
    enqueue_fragment_payload(recipient, from, reason, None, build)
}

/// Copies `reason` — whose heap is described by `src_off_heap` — into `process`'s own heap,
/// registering every referenced off-heap object in the process's own off-heap table, and
/// returns the copied term. Retries once after a collect-and-grow on heap exhaustion
/// ([`grow_receiver_for_signal`]).
///
/// A `process` whose live terms are **not** all in the crate root set
/// ([`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots) — the
/// exception is the same as in [`Process::deliver`](crate::process::Process::deliver)) **or** whose
/// relocating collection the host's [`CollectionGate`] declines (`decline`) is refused:
/// the heap must not move under live terms the root set does not carry (a host's Rust locals, or
/// the running machine's own x/stack/float registers), so the raw [`BeamError::Alloc`] refusal is
/// returned unchanged — no collection, no growth, no retry. That refusal is the **signal the caller
/// keys its heap-fragment route on**: [`ProcessTable::enqueue_down_signal`] and
/// [`ProcessTable::enqueue_exit_signal`] then build the control message into a [`HeapFragment`]
/// ([`enqueue_fragment_payload`]) instead of queueing a [`PendingSignal`] whose reason cannot
/// live anywhere but the receiver's heap.
///
/// `None` means the reason is already owned by `process` (a process signalling itself) and is
/// returned unchanged.
fn copy_reason_onto(
    process: &mut Process,
    reason: Eterm,
    src_off_heap: Option<&OffHeapTable>,
    decline: bool,
) -> Result<Eterm> {
    let Some(src) = src_off_heap else {
        return Ok(reason);
    };
    match process.copy_into_heap(reason, src) {
        Ok(copied) => Ok(copied),
        Err(BeamError::Alloc(failure))
            if matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
        {
            // The pre-test: hand the refusal back *verbatim* (`kind` is still Heap/Message)
            // without touching the collector, so the caller can route the signal into a heap
            // fragment. The defensive branch of `grow_receiver_for_signal` would refuse
            // identically; testing here keeps the "no collect, no grow" guarantee at the one place
            // the enqueue-time copy is performed.
            if process.has_unpublished_roots() || decline {
                return Err(BeamError::Alloc(failure));
            }
            grow_receiver_for_signal(process, failure, decline)?;
            process.copy_into_heap(reason, src)
        }
        Err(error) => Err(error),
    }
}

/// Validates an `erlang:monitor/3` `{tag, Tag}` user tag.
///
/// `None` (no option) and an **immediate** tag pass through unchanged; anything else — a term
/// resident in a per-process heap, which a monitor record (a plain value the collector never
/// scans) could not keep valid across a collection — is refused with the typed
/// [`error::unsupported_tag`], so the caller never gets a dangling tag in a `{'DOWN', …}`.
fn check_tag(tag: Option<Eterm>) -> Result<Option<Eterm>> {
    match tag {
        None => Ok(None),
        Some(term) if is_immediate(term) => Ok(Some(term)),
        Some(_) => Err(error::unsupported_tag()),
    }
}

/// The `erlang:monitor/3` options that **outlive the call**: the `{tag, Tag}` user tag and the
/// `{alias, Mode}` reference alias.
///
/// Both are exactly the options the C's `erts_monitor_opts` parses into its `add_oflags`/`tag`
/// out-parameters (`bif.c:701-756`). This is the seam the BIF layer (`beam_rt`) passes the
/// already-parsed option set down through; every `*_with_options` monitor entry point takes it,
/// and the `*_with_tag` methods are its `alias: None` delegations (so their behaviour is
/// unchanged for existing callers).
///
/// The atom-to-[`UnaliasMode`] parse (`explicit_unalias` / `demonitor` / `reply_demonitor`) is the
/// caller's job, exactly as the atom-to-[`MonitorKind`] parse is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MonitorOptions {
    /// The `{tag, Tag}` user tag that replaces the `'DOWN'` head atom (see `monitor.rs`'s
    /// user-tag contract: it must be a heap-independent immediate).
    pub tag: Option<Eterm>,
    /// The `{alias, Mode}` option: when `Some`, the monitor's own reference **doubles as an
    /// alias** for the watcher, so a message sent to it is delivered to the watcher and it
    /// expires per `mode`.
    pub alias: Option<UnaliasMode>,
}

impl MonitorOptions {
    /// The `erlang:monitor/2` option set: no tag, no alias.
    pub const NONE: MonitorOptions = MonitorOptions {
        tag: None,
        alias: None,
    };

    /// The `erlang:monitor/3` `{tag, Tag}`-only option set.
    pub const fn with_tag(tag: Option<Eterm>) -> MonitorOptions {
        MonitorOptions { tag, alias: None }
    }
}

/// Remembers `reference` as an **alias** owned by `owner`, if the monitor carried an
/// `{alias, Mode}` option.
///
/// Only an alias-flagged reference leaves an entry, so a lookup answering `None` means "not an
/// alias" (a plain monitor reference, or one whose alias has already expired) — exactly the
/// shape the send path needs to map to `badarg`.
fn remember_alias(
    aliases: &mut HashMap<MonitorRef, AliasEntry>,
    reference: MonitorRef,
    owner: ProcessId,
    mode: Option<UnaliasMode>,
) {
    if let Some(mode) = mode {
        aliases.insert(reference, AliasEntry { owner, mode });
    }
}

/// Forgets `reference`'s alias — it has expired (its mode's rule fired), so it must not resolve
/// to its owner any more.
fn forget_alias(aliases: &mut HashMap<MonitorRef, AliasEntry>, reference: MonitorRef) {
    aliases.remove(&reference);
}

/// Remembers `reference`'s `erlang:monitor/3` user tag, if it has one.
///
/// Only a supplied tag is stored — an untagged monitor leaves no entry, so a lookup answering
/// `None` means the default `'DOWN'` head atom (see `monitor.rs`'s user-tag contract). The
/// stored term is a validated immediate.
fn remember_tag(tags: &mut HashMap<MonitorRef, Eterm>, reference: MonitorRef, tag: Option<Eterm>) {
    if let Some(tag) = tag {
        tags.insert(reference, tag);
    }
}

/// Forgets `reference`'s user tag — its monitor is gone, so the entry must not linger.
fn forget_tag(tags: &mut HashMap<MonitorRef, Eterm>, reference: MonitorRef) {
    tags.remove(&reference);
}

/// A standalone copy of `src`'s off-heap handles (each cloned — the refcount is bumped), with
/// the **same slot indices** so a term copied against the snapshot resolves exactly as it would
/// against `src`.
///
/// Used when the copy source lives in the table and the copy destination is mutated through the
/// same `&mut ProcessTable`: the snapshot breaks the aliasing so
/// [`Process::copy_into_heap`](crate::process::Process::copy_into_heap) can borrow the
/// destination. The cloned handles are released when the snapshot drops, so the refcounts net
/// out.
fn snapshot_off_heap(src: &OffHeapTable) -> OffHeapTable {
    let mut snapshot = OffHeapTable::new();
    for handle in src.iter() {
        snapshot.push(handle.clone());
    }
    snapshot
}

/// The process table, the local name registry and the signal/link/monitor hub.
pub struct ProcessTable {
    /// The numbered slots; a `None` process is a free slot awaiting reuse.
    slots: Vec<Slot>,
    /// Free slot numbers available for reuse (with a bumped serial).
    free: Vec<u64>,
    /// The local name registry (`erlang:register/2` / `whereis/1`).
    names: HashMap<Atom, ProcessId>,
    /// The never-reusing monitor-reference allocator.
    refs: RefAllocator,
    /// The well-known signal atoms.
    atoms: SignalAtoms,
    /// The **wake list**: the pids of blocked processes that a delivery/signal has just made
    /// runnable, so the single-threaded scheduler can absorb them (see
    /// [`ProcessTable::drain_woken`] and `crate::scheduler::Scheduler::resume_woken`).
    ready: Vec<ProcessId>,
    /// Per local process: its remote (external-node) link partners.
    remote_links: HashMap<ProcessId, Vec<RemotePid>>,
    /// Per local process: its outgoing monitors on remote processes/ports.
    remote_monitors: HashMap<ProcessId, Vec<RemoteMonitor>>,
    /// The target descriptor of every **named** monitor (`erlang:monitor(process, Name)`), keyed
    /// by its reference (see [`NamedTarget`]). An entry dies with its monitor: `demonitor`, the
    /// dying holder's outgoing-monitor sweep and a delivered DOWN all remove it.
    named_monitors: HashMap<MonitorRef, NamedTarget>,
    /// The `erlang:monitor/3` `{tag, Tag}` **user tag** of every tagged monitor, keyed by its
    /// reference.
    ///
    /// A tag cannot be a field of [`Monitor`]/[`MonitoredBy`]/[`PortWatch`] — those are built by
    /// struct literal from other crates, so their shape is fixed — and it is kept here instead,
    /// exactly like [`NamedTarget`] (see `monitor.rs`'s user-tag contract). An entry is created
    /// only when a tag is supplied, holds a **heap-independent immediate** (so it is not a GC
    /// root and never needs relocating), and is removed the moment its monitor dies —
    /// `demonitor`/`demonitor_flush`, the terminated target's watcher sweep, `demonitor_port` and
    /// `port_closed` — so it cannot leak.
    tags: HashMap<MonitorRef, Eterm>,
    /// The live **reference aliases** (`erlang:alias/1` and the `erlang:monitor/3`
    /// `{alias, Mode}` option), keyed by the alias reference, holding which process a message sent
    /// to the reference is delivered to and the mode that expires it.
    ///
    /// Like [`ProcessTable::tags`] this is a **plain-value side table** the collector never scans
    /// (owner and mode are both plain values), and it holds no term. An entry is removed when its
    /// mode's rule fires — at the monitor's DOWN site, at an explicit `erlang:demonitor/1`, at a
    /// reply through a `reply_demonitor` alias, or at an explicit `erlang:unalias/1` — and every
    /// alias of a dying process goes with it (see [`ProcessTable::terminate_into`]), so a
    /// reference can never resolve to a dead pid.
    aliases: HashMap<MonitorRef, AliasEntry>,
    /// The atom naming the **local node**, used to spell the `{Name, LocalNode}` target of a
    /// named monitor. `None` until [`ProcessTable::set_local_node`] installs it; a named monitor
    /// requested without it is a typed [`error::unsupported`] rather than a wrong target.
    local_node: Option<Atom>,
    /// The distribution seam (see [`DistSignalRouter`]); `None` means **record-only** — the
    /// local relationship is still tracked, only the wire signal is dropped (a node with no
    /// distribution configured).
    router: Option<Box<dyn DistSignalRouter>>,
    /// The process-death observer seam (see [`ProcessExitObserver`]); `None` means no observer is
    /// installed and termination behaves exactly as before the seam existed. Invoked **exactly
    /// once per death** by [`ProcessTable::terminate_into`], before the dying heap is released.
    exit_observer: Option<Box<dyn ProcessExitObserver>>,
    /// The host **collection gate** seam (see [`CollectionGate`]); `None` means no gate is
    /// installed and only the crate's own [`Process::has_unpublished_roots`] test can decline a
    /// relocating collection. Consulted at every signal site (and by
    /// [`ProcessTable::deliver`]) so a host that knows a receiver's live root set is not fully
    /// scanned can refuse the move — the notification is still delivered, through the
    /// heap-fragment route.
    collection_gate: Option<Box<dyn CollectionGate>>,
}

impl ProcessTable {
    /// An empty table using `atoms` to build signal messages.
    pub fn new(atoms: SignalAtoms) -> ProcessTable {
        ProcessTable {
            slots: Vec::new(),
            free: Vec::new(),
            names: HashMap::new(),
            refs: RefAllocator::new(),
            atoms,
            ready: Vec::new(),
            remote_links: HashMap::new(),
            remote_monitors: HashMap::new(),
            named_monitors: HashMap::new(),
            tags: HashMap::new(),
            aliases: HashMap::new(),
            local_node: None,
            router: None,
            exit_observer: None,
            collection_gate: None,
        }
    }    // --- allocation / lookup -------------------------------------------------

    /// Allocates a fresh pid, reusing a free slot with a bumped serial.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the pid number space is exhausted.
    pub fn alloc_pid(&mut self) -> Result<ProcessId> {
        let number = match self.free.pop() {
            Some(number) => number,
            None => {
                // Grow the table: a number that was never handed out yet. The slot's
                // serial is 0 for a brand-new number and already bumped for a reused one.
                let number = self.slots.len() as u64;
                if number > MAX_PROCESS_NUMBER {
                    return Err(error::system_limit());
                }
                self.slots.push(Slot {
                    serial: 0,
                    process: None,
                });
                number
            }
        };
        let serial = self.slots[number as usize].serial;
        Ok(ProcessId::new(number, serial))
    }

    /// Allocates a pid and installs a fresh process with `policy`.
    ///
    /// # Errors
    ///
    /// Propagates pid allocation and initial heap allocation failures.
    pub fn spawn(&mut self, policy: HeapPolicy) -> Result<ProcessId> {
        let pid = self.alloc_pid()?;
        let process = Process::with_policy(pid, policy)?;
        self.insert(process)?;
        Ok(pid)
    }

    /// Allocates a pid and installs a fresh process with the default heap policy.
    ///
    /// # Errors
    ///
    /// Propagates pid allocation and initial heap allocation failures.
    pub fn spawn_default(&mut self) -> Result<ProcessId> {
        self.spawn(HeapPolicy::default())
    }

    /// Installs `process` in the slot its pid names.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::SystemLimit` when the pid's number is out of range.
    pub fn insert(&mut self, process: Process) -> Result<()> {
        let pid = process.pid();
        let number = pid.number();
        if number > MAX_PROCESS_NUMBER {
            return Err(error::system_limit());
        }
        let index = number as usize;
        // A pid built by hand may point past the current table: materialise the gap.
        while self.slots.len() <= index {
            self.slots.push(Slot {
                serial: 0,
                process: None,
            });
        }
        // The slot is no longer available for reuse.
        self.free.retain(|&free| free != number);
        let slot = &mut self.slots[index];
        slot.serial = pid.serial();
        slot.process = Some(process);
        Ok(())
    }

    /// Borrows the process named by `pid`.
    pub fn get(&self, pid: ProcessId) -> Option<&Process> {
        lookup(&self.slots, pid)
    }

    /// Borrows the process named by `pid`, mutably.
    pub fn get_mut(&mut self, pid: ProcessId) -> Option<&mut Process> {
        lookup_mut(&mut self.slots, pid)
    }

    /// Removes the process named by `pid`, leaving the slot free for reuse.
    pub fn take(&mut self, pid: ProcessId) -> Option<Process> {
        let slot = self.slots.get_mut(pid.number() as usize)?;
        if slot.serial != pid.serial() {
            return None;
        }
        let process = slot.process.take()?;
        // A reused slot names a *different* process: bump the serial so every pid that
        // used to name this slot goes stale for good.
        slot.serial = slot.serial.wrapping_add(1);
        self.free.push(pid.number());
        // A pid that is no longer live cannot be resumed: drop any pending wake for it.
        self.ready.retain(|&woken| woken != pid);
        Some(process)
    }
    /// Removes the process named by `pid`.
    pub fn remove(&mut self, pid: ProcessId) -> Option<Process> {
        self.take(pid)
    }

    /// Whether `pid` names a live process.
    pub fn contains(&self, pid: ProcessId) -> bool {
        lookup(&self.slots, pid).is_some()
    }

    /// Whether `pid` names a live process (`erlang:is_process_alive/1`).
    ///
    /// The liveness test for the `spawn*`/`monitor`/`send` BIFs; it is exactly
    /// [`ProcessTable::contains`], named for the observable it answers.
    pub fn is_process_alive(&self, pid: ProcessId) -> bool {
        self.contains(pid)
    }

    /// The termination reason recorded on `pid`'s PCB, if it has one.
    ///
    /// `Ok(None)` means the process is live and has not terminated yet. This is the source for
    /// `erlang:process_info/2`'s exit-reason item and for the reason a linked/monitoring peer is
    /// told.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown (or stale).
    pub fn exit_reason(&self, pid: ProcessId) -> Result<Option<Eterm>> {
        self.get(pid)
            .map(Process::exit_reason)
            .ok_or_else(error::unknown_process)
    }

    /// The number of live processes.
    pub fn len(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.process.is_some())
            .count()
    }

    /// Iterates the live pids.
    pub fn pids(&self) -> impl Iterator<Item = ProcessId> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.process.is_some())
            .map(|(number, slot)| ProcessId::new(number as u64, slot.serial))
    }

    /// Sets the scheduling priority of the process named by `pid`
    /// (`erlang:process_flag(priority, P)`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn set_priority(&mut self, pid: ProcessId, priority: Priority) -> Result<()> {
        self.get_mut(pid)
            .ok_or_else(error::unknown_process)
            .map(|process| process.set_priority(priority))
    }

    // --- GC relocation exemption ---------------------------------------------

    /// Marks the process named by `pid` as **relocation-exempt** (or clears the mark).
    ///
    /// A thin table-level convenience over
    /// [`Process::set_relocation_exempt`](crate::process::Process::set_relocation_exempt): the host
    /// (`beam_rt`) marks a process whose full root set it cannot publish, so the crate never
    /// relocates its heap (see that method for the full contract). The table's own `deliver`
    /// already propagates [`Process::deliver`](crate::process::Process::deliver)'s error unchanged,
    /// so no table-level delivery change is needed.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn set_relocation_exempt(&mut self, pid: ProcessId, exempt: bool) -> Result<()> {
        self.get_mut(pid)
            .ok_or_else(error::unknown_process)
            .map(|process| process.set_relocation_exempt(exempt))
    }

    /// Whether the process named by `pid` is **relocation-exempt** (default `false`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn is_relocation_exempt(&self, pid: ProcessId) -> Result<bool> {
        self.get(pid)
            .map(Process::is_relocation_exempt)
            .ok_or_else(error::unknown_process)
    }

    // --- host collection gate ------------------------------------------------

    /// Installs the host **collection gate** seam the table consults before any *relocating*
    /// collection of a receiver's heap (see [`CollectionGate`]).
    ///
    /// Without a gate only the crate's own [`Process::has_unpublished_roots`] test
    /// (relocation-exempt, or [`ProcessState::Running`]) can decline the move. A host that knows a
    /// receiver's live root set is not fully scanned by its collector installs a gate here, and
    /// every signal site plus [`ProcessTable::deliver`] then declines the relocation for such a
    /// receiver: the notification is still delivered — through the heap-fragment route the
    /// `Running`/relocation-exempt receiver already uses — with no collection, no growth and no
    /// word moved.
    pub fn set_collection_gate(&mut self, gate: Box<dyn CollectionGate>) {
        self.collection_gate = Some(gate);
    }

    /// Takes the collection gate out of the table, returning it to the caller (and restoring the
    /// no-gate behaviour, exactly as [`ProcessTable::take_dist_router`] does for the router).
    pub fn take_collection_gate(&mut self) -> Option<Box<dyn CollectionGate>> {
        self.collection_gate.take()
    }

    /// Whether a collection gate is installed.
    pub fn has_collection_gate(&self) -> bool {
        self.collection_gate.is_some()
    }

    /// Asks the installed collection gate whether a **relocating** collection of `pid`'s heap must
    /// be declined. `false` when no gate is installed and `false` for a pid the table does not hold
    /// (a receiver that died earlier in the same propagation has no heap left to move).
    ///
    /// Split borrows of two disjoint fields (`slots` and `collection_gate`), so the gate can be
    /// handed the receiver's PCB while the slot table stays borrowed.
    fn declines_signal_collection(&mut self, pid: ProcessId) -> bool {
        let ProcessTable {
            slots,
            collection_gate,
            ..
        } = self;
        let Some(process) = lookup(slots, pid) else {
            return false;
        };
        collection_gate
            .as_deref_mut()
            .map_or(false, |gate| gate.declines_relocation(process))
    }
    // --- message delivery ----------------------------------------------------

    /// Delivers `term` (with off-heap references resolved against `src_off_heap`) from `from`
    /// into `to`'s mailbox, waking `to` when it was blocked.
    ///
    /// This is the table-level entry point a runner uses to send between processes: the wake
    /// is recorded on the table's wake list and absorbed by
    /// `crate::scheduler::Scheduler::resume_woken` at the end of the current slice, so a send
    /// during a slice never lets `run_ready` exit with a freshly-woken process still queued.
    ///
    /// # The host collection gate
    ///
    /// When the installed [`CollectionGate`] declines a relocating collection of `to`'s heap
    /// (`declines_signal_collection`), the receiver is marked relocation-exempt for
    /// the duration of **exactly one** [`Process::deliver`](crate::process::Process::deliver) call
    /// and its prior flag is restored unconditionally afterwards. The delivery therefore behaves
    /// exactly as it does for a receiver the crate already declines to move: a payload that fits
    /// the current nursery is enqueued normally, and one that does not returns
    /// [`Process::deliver`](crate::process::Process::deliver)'s **raw**
    /// `BeamError::Alloc(AllocKind::Heap | AllocKind::Message)` refusal — the exact value the
    /// host's heap-fragment fallback keys on — with **no** collection, no growth and no word moved.
    /// Declining the relocation never loses the message: the caller's fragment route still delivers
    /// it, and a no-relocation growth
    /// ([`Process::grow_nursery`](crate::process::Process::grow_nursery)) stays available.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `to` is unknown; propagates the copy engine's
    /// `BeamError::Alloc` when the message still does not fit after a collection.
    pub fn deliver(
        &mut self,
        to: ProcessId,
        from: Eterm,
        term: Eterm,
        src_off_heap: &OffHeapTable,
    ) -> Result<DeliverOutcome> {
        let decline = self.declines_signal_collection(to);
        let outcome = {
            let Some(recipient) = lookup_mut(&mut self.slots, to) else {
                return Err(error::unknown_process());
            };
            if decline {
                // The gate declines the move: refuse the relocating collection for the duration of
                // this one delivery by marking the receiver relocation-exempt, so
                // `Process::deliver` surfaces its raw `AllocKind::Heap`/`AllocKind::Message`
                // refusal UNCHANGED (the caller's heap-fragment key) rather than collecting and
                // moving the heap under the receiver's unscanned roots. The prior flag is restored
                // unconditionally — before the `?` — so a host that exempts a process itself keeps
                // its mark and an ordinary receiver is left exactly as it was.
                let prior = recipient.is_relocation_exempt();
                recipient.set_relocation_exempt(true);
                let delivered = recipient.deliver(from, term, src_off_heap);
                recipient.set_relocation_exempt(prior);
                delivered?
            } else {
                recipient.deliver(from, term, src_off_heap)?
            }
        };
        if outcome.woke() {
            self.ready.push(to);
        }
        Ok(outcome)
    }

    /// The no-off-heap convenience form of [`ProcessTable::deliver`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `to` is unknown; propagates the copy engine's
    /// `BeamError::Alloc` when the message still does not fit after a collection.
    pub fn deliver_simple(
        &mut self,
        to: ProcessId,
        from: Eterm,
        term: Eterm,
    ) -> Result<DeliverOutcome> {
        let no_off_heap = OffHeapTable::new();
        self.deliver(to, from, term, &no_off_heap)
    }

    /// Returns — and clears — the pids of the blocked processes a delivery/signal has just
    /// made runnable, in the order they were woken.
    ///
    /// `crate::scheduler::Scheduler::resume_woken` is the consumer; callers that send from
    /// **outside** a scheduler slice call it (directly or via the scheduler) to learn which
    /// processes to enqueue.
    pub fn drain_woken(&mut self) -> Vec<ProcessId> {
        core::mem::take(&mut self.ready)
    }

    /// The pids of the blocked processes a delivery/signal has just made runnable, without
    /// clearing the wake list (see [`ProcessTable::drain_woken`]).
    pub fn woken(&self) -> &[ProcessId] {
        &self.ready
    }

    // --- signals / links / monitors -----------------------------------------
    /// Sends an exit signal to `target`, following [`crate::signal::exit_policy`].
    ///
    /// The signal is **enqueued** on the target's [`SignalQueue`](crate::signal_queue) — never
    /// applied inline (the reason is copied into the target's own heap, so it outlives the
    /// signaller) — and then serviced **eagerly** unless the target is the currently executing
    /// process ([`ProcessState::Running`]), whose signal is deferred to its next schedule-in
    /// (see [`ProcessTable::service_signals`]).
    ///
    /// # Return value for a deferred signal
    ///
    /// The returned [`ExitEffect`] is the effect [`exit_policy`] predicts **now**: for an
    /// eagerly applied signal it is the observed outcome; for a deferred one it is the
    /// prediction (what will happen when the target next runs).
    ///
    /// # Errors
    ///
    /// Propagates the signalling/termination errors (`BeamError::TermNotFound` for an
    /// unknown target, `BeamError::Alloc` for a trapped message that does not fit).
    pub fn apply_exit_signal(
        &mut self,
        target: ProcessId,
        signal: ExitSignal,
    ) -> Result<ExitEffect> {
        let Some(process) = self.get(target) else {
            return Err(error::unknown_process());
        };
        let trapping = process.is_trapping_exit();
        let effect = match exit_policy(signal.reason, trapping, &self.atoms) {
            ExitPolicy::Ignore => ExitEffect::Ignored,
            ExitPolicy::Trap => ExitEffect::Trapped,
            ExitPolicy::Terminate(_) => ExitEffect::Terminated,
        };

        // Generate: enqueue, never apply inline. The signaller's off-heap handles are
        // snapshotted because the target may be the currently executing process, which cannot
        // be moved out of the table to copy against a borrowed source.
        let fallback = OffHeapTable::new();
        let src = self
            .source_off_heap(target, signal.from, &fallback)
            .map(snapshot_off_heap);
        self.enqueue_exit_signal(target, signal.from, signal.reason, src.as_ref())?;

        // Eagerly service unless the target is the currently executing process, whose signal
        // is deferred to its next schedule-in.
        let mut pending = VecDeque::new();
        self.schedule_service(target, &mut pending);
        // No port table here: a cascade that terminates a process drains its outgoing port
        // monitors but cannot remove the incoming entries (a caller holding the table should
        // terminate through `terminate_with_ports`/`service_signals_with_ports`).
        let mut ports: Option<&mut PortTable> = None;
        self.drain_signals(&mut ports, &mut pending)?;
        Ok(effect)
    }

    /// Links `a` and `b` (both directions).
    ///
    /// Links are **set** semantics, not reference-counted: a re-link of an already-linked pair
    /// changes nothing (see [`crate::link::Links`]).
    ///
    /// A **non-live `b`** (a dead pid, or one that never existed) is not an error and records no
    /// link: the oracle answers `true` and signals a `noproc` exit **from that pid** at the
    /// linking caller, so a **trapping** caller is handed `{'EXIT', b, noproc}` and a
    /// non-trapping one is terminated with `noproc`. The signal goes through
    /// [`ProcessTable::apply_exit_signal`] (no new mechanism), so it inherits that path's
    /// eager-vs-deferred rule: a caller that is *not* currently executing sees the effect before
    /// this returns, while a `Running` caller — the one inside `erlang:link/1` — has it queued
    /// and applied at its next schedule-in, exactly as `erlang:exit(self(), Reason)` already is.
    ///
    /// Documented approximation: the oracle *raises* `error:noproc` in the non-trapping caller
    /// (whose uncaught reason is `{noproc, Stacktrace}`), whereas this signals the exit reason as
    /// the bare atom `noproc`. The trapping case matches the oracle exactly; the raise's
    /// stacktrace shape belongs to `beam_rt`'s link floor.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **caller** `a` is unknown; propagates the
    /// signal path's allocation/termination errors for a non-live target.
    pub fn link(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        if !self.contains(a) {
            return Err(error::unknown_process());
        }
        if !self.contains(b) {
            // Nothing to link to: the oracle signals `noproc` from the target at the caller (a
            // pid the process table does not hold is exactly the "there is no such process" the
            // signal reports). `b == a` cannot reach this branch — `a` is live here.
            let reason = self.atoms.noproc_term();
            return self
                .apply_exit_signal(a, ExitSignal { from: b, reason })
                .map(|_effect| ());
        }
        if a == b {
            // A self-link would only ever signal the process to itself on exit, which
            // termination already handles; keep the link set free of it.
            return Ok(());
        }
        if let Some(process) = self.get_mut(a) {
            process.links_mut().add(b);
        }
        if let Some(process) = self.get_mut(b) {
            process.links_mut().add(a);
        }
        Ok(())
    }

    /// Unlinks `a` and `b` (both directions).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when either pid is unknown.
    pub fn unlink(&mut self, a: ProcessId, b: ProcessId) -> Result<()> {
        if !self.contains(a) || !self.contains(b) {
            return Err(error::unknown_process());
        }
        if a == b {
            return Ok(());
        }
        if let Some(process) = self.get_mut(a) {
            process.links_mut().remove(b);
        }
        if let Some(process) = self.get_mut(b) {
            process.links_mut().remove(a);
        }
        Ok(())
    }

    /// Sends an exit signal from `from` to `to` with `reason` (`erlang:exit/2`).
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::apply_exit_signal`]'s errors.
    pub fn send_exit(&mut self, from: ProcessId, to: ProcessId, reason: Eterm) -> Result<()> {
        self.apply_exit_signal(to, ExitSignal { from, reason })?;
        Ok(())
    }

    /// Installs a monitor from `watcher` on `target` and returns its reference.
    ///
    /// A **live** `target` records both halves of the monitor (an outgoing [`Monitor`] on the
    /// watcher, an incoming [`MonitoredBy`] on the target). A `target` that is not a live local
    /// process — a dead pid, or a pid that never existed — is **not** an error: the reference is
    /// still allocated and the watcher immediately receives the
    /// `{'DOWN', Ref, process, Target, noproc}` the oracle delivers, and **no** monitor entry is
    /// recorded (there is nothing left to watch).
    ///
    /// This is the `erlang:monitor/2` form (no `{tag, Tag}` option); it delegates to
    /// [`ProcessTable::monitor_with_options`] with [`MonitorOptions::NONE`], so the notification
    /// carries the default `'DOWN'` head atom.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **watcher** is unknown; propagates the message
    /// builder's `BeamError::Alloc` when the immediate DOWN does not fit the watcher's heap.
    pub fn monitor(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        self.monitor_with_options(watcher, target, kind, MonitorOptions::NONE)
    }

    /// [`ProcessTable::monitor`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**: the
    /// eventual `{'DOWN', Ref, Kind, Target, Reason}` becomes `{Tag, Ref, Kind, Target, Reason}`.
    ///
    /// `tag = None` reproduces [`ProcessTable::monitor`] byte-for-byte. A tag that is not a
    /// heap-independent immediate (see `monitor.rs`'s user-tag contract) is refused with the
    /// typed [`error::unsupported_tag`] rather than stored as a word a collection could
    /// invalidate.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor`], plus [`error::unsupported_tag`] for a non-immediate tag.
    pub fn monitor_with_tag(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        self.monitor_with_options(watcher, target, kind, MonitorOptions::with_tag(tag))
    }

    /// [`ProcessTable::monitor`] with the full `erlang:monitor/3` option set: the `{tag, Tag}`
    /// user tag **and** the `{alias, Mode}` reference alias.
    ///
    /// With `options.alias = Some(mode)` the monitor's reference also becomes an **alias** for
    /// `watcher`, so `Ref ! Msg` delivers `Msg` to `watcher`
    /// ([`ProcessTable::resolve_alias`]), and the alias expires per `mode`
    /// ([`UnaliasMode`]) — see [`ProcessTable::unalias`] and
    /// [`ProcessTable::alias_reply_delivered`].
    ///
    /// The alias is registered **before** the born-down check, so a monitor on an already-dead
    /// target still applies its mode's rule at the DOWN site: an `explicit_unalias` alias lives on
    /// as a standalone alias, while a `demonitor`/`reply_demonitor` one is gone by the time this
    /// returns — exactly the oracle's behaviour.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor`], plus [`error::unsupported_tag`] for a non-immediate tag.
    pub fn monitor_with_options(
        &mut self,
        watcher: ProcessId,
        target: ProcessId,
        kind: MonitorKind,
        options: MonitorOptions,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let tag = check_tag(options.tag)?;
        let reference = self.refs.next();
        remember_alias(&mut self.aliases, reference, watcher, options.alias);
        if !self.contains(target) {
            // Nothing to watch: the monitor is born down (`noproc`), and the watcher is told so
            // right away. No monitor entry is recorded, so a later `demonitor` finds no local half
            // — it reports `false`, exactly like a monitor that is already gone. An alias does not
            // change that: `deliver_noproc_down` applies the mode's rule to it (an
            // `explicit_unalias` alias survives as a standalone alias, the other two are gone).
            self.deliver_noproc_down(watcher, reference, target.to_eterm(), tag)?;
            return Ok(reference);
        }
        if let Some(process) = self.get_mut(watcher) {
            process.monitors_mut().push(Monitor {
                reference,
                target,
                kind,
            });
        }
        if let Some(process) = self.get_mut(target) {
            process.monitored_by_mut().push(MonitoredBy {
                reference,
                watcher,
                kind,
            });
        }
        // The tag is kept in the side table, not in the records above (see the field docs).
        remember_tag(&mut self.tags, reference, tag);
        Ok(reference)
    }

    /// Enqueues the immediate `{'DOWN', Ref, process, Target, noproc}` of a monitor on a
    /// non-live target into `watcher`'s mailbox and wakes a blocked watcher.
    ///
    /// When `tag` is `Some`, the head atom is the caller's user tag instead of `'DOWN'`
    /// (the `erlang:monitor/3` form). Mirrors
    /// [`ProcessTable::enqueue_port_down_message`]: the build goes through
    /// [`build_signal_message`] (so a full heap is a collection and a retry — or, for a
    /// **relocation-exempt**/`Running` watcher or one the host's [`CollectionGate`] declines, a
    /// heap fragment — never fatal) and the wake is recorded on
    /// the wake list. `target` must be an immediate (a pid term; the named form materialises its
    /// `{Name, Node}` pair inside the closure); the `noproc` reason is an atom immediate, so
    /// nothing relocates. An unknown watcher is a no-op (the caller has already checked it, so
    /// only a mid-call death can reach this).
    fn deliver_noproc_down(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        target: Eterm,
        tag: Option<Eterm>,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let reason = atoms.noproc_term();
        let decline = self.declines_signal_collection(watcher);
        let Some(recipient) = lookup_mut(&mut self.slots, watcher) else {
            return Ok(());
        };
        build_signal_message(recipient, target, reason, decline, |heap, reason| {
            let reference_term = reference.to_eterm(heap)?;
            build_down_message_term(
                heap,
                &atoms,
                tag,
                reference_term,
                atoms.process(),
                target,
                reason,
            )
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(watcher);
        }
        // A monitor on an already-dead target is born down, so its `{alias, Mode}` alias must
        // obey the DOWN-site rule right here: `demonitor`/`reply_demonitor` are gone by the time
        // this returns, `explicit_unalias` lives on as a standalone alias (the oracle's
        // behaviour, pinned by `alias.rs`).
        self.apply_alias_on_down(reference);
        Ok(())
    }

    /// Enqueues the immediate `{'DOWN', Ref, process, {Name, Node}, noproc}` of a **named**
    /// monitor on an unregistered name into `watcher`'s mailbox, and wakes a blocked watcher.
    ///
    /// When `tag` is `Some`, the head atom is the caller's user tag instead of `'DOWN'`.
    /// The `{Name, Node}` target is materialised *inside* the [`build_signal_message`] closure, on
    /// whichever heap (the watcher's, or a fragment's scratch arena) the closure is handed, so a
    /// collect-and-retry never leaves a stale word behind.
    ///
    /// The message's **`from`** is the `system` atom — an immediate — exactly as the C stores
    /// `am_system` for a monitor that failed by name (`erl_proc_sig_queue.c:4991`). `from` is
    /// **not** a GC root and is never relocated, so it may never hold a heap term.
    fn deliver_named_noproc_down(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        name: Atom,
        node: Atom,
        tag: Option<Eterm>,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let reason = atoms.noproc_term();
        let from = atoms.system_term();
        let decline = self.declines_signal_collection(watcher);
        let Some(recipient) = lookup_mut(&mut self.slots, watcher) else {
            return Ok(());
        };
        build_signal_message(recipient, from, reason, decline, |heap, reason| {
            let reference_term = reference.to_eterm(heap)?;
            let target = heap.tuple(&[make_atom(name), make_atom(node)])?;
            build_down_message_term(
                heap,
                &atoms,
                tag,
                reference_term,
                atoms.process(),
                target,
                reason,
            )
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(watcher);
        }
        // Born down (an unregistered name), so the DOWN-site alias rule applies here too.
        self.apply_alias_on_down(reference);
        Ok(())
    }
    // --- named monitors ------------------------------------------------------

    /// Installs the atom that names the **local node** (e.g. `nonode@nohost`).
    ///
    /// A named monitor spells its DOWN target as `{Name, <this atom>}`, so the runtime (which
    /// owns the node's identity) must install it. Until then a named monitor is a typed
    /// [`error::unsupported`], never a guessed target.
    pub fn set_local_node(&mut self, node: Atom) {
        self.local_node = Some(node);
    }

    /// The atom naming the local node, once [`ProcessTable::set_local_node`] installed it.
    pub fn local_node(&self) -> Option<Atom> {
        self.local_node
    }

    /// Installs a monitor from `watcher` on the **registered name** `name`, resolving the target
    /// on the node `node` (`erlang:monitor(process, Name)`).
    ///
    /// A **live** registered name resolves to its process and records the ordinary monitor halves
    /// (an outgoing [`Monitor`] on the watcher, an incoming [`MonitoredBy`] on the target); the
    /// `{Name, node}` descriptor is recorded beside them, so the eventual `{'DOWN', …}` carries
    /// the **name** rather than the pid — exactly what the oracle does. An **unregistered** name
    /// (or one whose process has died) yields the immediate
    /// `{'DOWN', Ref, process, {Name, node}, noproc}` and records nothing.
    ///
    /// A [`MonitorKind::Port`] is rejected with the typed [`error::named_port_monitor`]: this
    /// registry resolves names to processes, so a **named port** monitor
    /// (`erlang:monitor(port, Name)`) has no faithful resolution here (see
    /// [`ProcessTable::monitor_port_term`] for the port-term form).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the watcher is unknown; propagates the message
    /// builder's `BeamError::Alloc` when the immediate DOWN does not fit the watcher's heap.
    pub fn monitor_named(
        &mut self,
        watcher: ProcessId,
        name: Atom,
        node: Atom,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        self.monitor_named_with_options(watcher, name, node, kind, MonitorOptions::NONE)
    }

    /// [`ProcessTable::monitor_named`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**.
    ///
    /// `tag = None` reproduces [`ProcessTable::monitor_named`] byte-for-byte; a non-immediate
    /// tag is the typed [`error::unsupported_tag`] (see `monitor.rs`'s user-tag contract).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_named`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag.
    pub fn monitor_named_with_tag(
        &mut self,
        watcher: ProcessId,
        name: Atom,
        node: Atom,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        self.monitor_named_with_options(watcher, name, node, kind, MonitorOptions::with_tag(tag))
    }

    /// [`ProcessTable::monitor_named`] with the full `erlang:monitor/3` option set (see
    /// [`ProcessTable::monitor_with_options`] for the `{alias, Mode}` contract).
    ///
    /// The alias is registered **before** the born-down check, so a named monitor on an
    /// unregistered name applies its mode's rule at the immediate `noproc` DOWN.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_named`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag.
    pub fn monitor_named_with_options(
        &mut self,
        watcher: ProcessId,
        name: Atom,
        node: Atom,
        kind: MonitorKind,
        options: MonitorOptions,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        if kind == MonitorKind::Port {
            // `erlang:monitor(port, Name)`: this registry resolves names to **processes**, so a
            // named port monitor has no faithful resolution here. Recording the process the
            // name resolves to would be a wrong (process) monitor; the typed diagnostic is the
            // honest answer.
            return Err(error::named_port_monitor());
        }
        let tag = check_tag(options.tag)?;
        let reference = self.refs.next();
        remember_alias(&mut self.aliases, reference, watcher, options.alias);
        // `whereis` already resolves "registered but its process is gone" to `None`.
        let Some(target) = self.whereis(name) else {
            self.deliver_named_noproc_down(watcher, reference, name, node, tag)?;
            return Ok(reference);
        };
        if let Some(process) = self.get_mut(watcher) {
            process.monitors_mut().push(Monitor {
                reference,
                target,
                kind,
            });
        }
        if let Some(process) = self.get_mut(target) {
            process.monitored_by_mut().push(MonitoredBy {
                reference,
                watcher,
                kind,
            });
        }
        self.named_monitors
            .insert(reference, NamedTarget { name, node });
        // The tag is kept in the side table, not in the records above (see the field docs).
        remember_tag(&mut self.tags, reference, tag);
        Ok(reference)
    }

    // --- port monitors -------------------------------------------------------

    /// Installs a **port** monitor from `watcher` on `port` and returns its reference
    /// (`erlang:monitor(port, Port)`).
    ///
    /// Both halves are recorded: the watcher keeps the outgoing [`PortWatch`] — so
    /// [`ProcessTable::demonitor_port`] and `erlang:process_info(P, monitors)` can see it —
    /// and the port keeps the incoming [`PortMonitor`](crate::port::PortMonitor), so
    /// [`ProcessTable::port_closed`] resolves the pair with
    /// `{'DOWN', Ref, port, Port, Reason}`.
    ///
    /// A `port` that is **not** currently allocated is *not* an error: like
    /// [`ProcessTable::monitor`] on a dead pid, the monitor is born down and the watcher
    /// immediately receives `{'DOWN', Ref, port, Port, noproc}`, while no entry is recorded on
    /// either side (so a later [`ProcessTable::demonitor_port`] reports `false`). This is the
    /// oracle's behaviour (`erlang:monitor(port, ClosedPort)` answers a reference and fires a
    /// `noproc` DOWN).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **watcher** is unknown; propagates the
    /// message builder's `BeamError::Alloc` when the immediate DOWN does not fit the watcher's
    /// heap.
    pub fn monitor_port(
        &mut self,
        watcher: ProcessId,
        port: PortId,
        ports: &mut PortTable,
    ) -> Result<MonitorRef> {
        self.monitor_port_with_options(watcher, port, ports, MonitorOptions::NONE)
    }

    /// [`ProcessTable::monitor_port`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**: the
    /// eventual `{'DOWN', Ref, port, Port, Reason}` becomes `{Tag, Ref, port, Port, Reason}`.
    ///
    /// `tag = None` reproduces [`ProcessTable::monitor_port`] byte-for-byte; a non-immediate tag
    /// is the typed [`error::unsupported_tag`] (see `monitor.rs`'s user-tag contract).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_port`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag.
    pub fn monitor_port_with_tag(
        &mut self,
        watcher: ProcessId,
        port: PortId,
        ports: &mut PortTable,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        self.monitor_port_with_options(watcher, port, ports, MonitorOptions::with_tag(tag))
    }

    /// [`ProcessTable::monitor_port`] with the full `erlang:monitor/3` option set.
    ///
    /// # Port monitors cannot carry an alias (a typed capability gap)
    ///
    /// `{alias, Mode}` on a **port** monitor is refused with the typed
    /// [`error::unsupported_port_monitor_alias`]. A `reply_demonitor` alias must be consumed where
    /// a message is delivered *through* it, and that site ([`ProcessTable::alias_reply_delivered`])
    /// has no handle on the [`PortTable`] — so the incoming
    /// [`PortMonitor`](crate::port::PortMonitor) on the port could not be removed with the alias.
    /// Leaving it behind would make a later [`ProcessTable::port_closed`] deliver a `{'DOWN', …}`
    /// the oracle would not (a **wrong result**), so the honest typed diagnostic is returned
    /// instead. No caller in OTP combines the two (`gen`'s call paths alias a **process** monitor),
    /// and the `{tag, Tag}` option is unaffected.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_port`], plus [`error::unsupported_tag`] for a non-immediate tag
    /// and [`error::unsupported_port_monitor_alias`] for an `{alias, Mode}` option.
    pub fn monitor_port_with_options(
        &mut self,
        watcher: ProcessId,
        port: PortId,
        ports: &mut PortTable,
        options: MonitorOptions,
    ) -> Result<MonitorRef> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        if options.alias.is_some() {
            // See the method docs: the reply site has no port table, so a port alias could not be
            // resolved faithfully (a later `port_closed` would deliver a spurious DOWN).
            return Err(error::unsupported_port_monitor_alias());
        }
        let tag = check_tag(options.tag)?;
        let reference = self.refs.next();
        if !ports.contains(port) {
            // Nothing to watch: the port is already gone. Tell the watcher right away and
            // record nothing, exactly as the pid path does for a dead pid.
            let reason = self.atoms.noproc_term();
            self.enqueue_port_down_message(watcher, reference, port, tag, reason)?;
            return Ok(reference);
        }
        ports.monitor(port, watcher, reference)?;
        if let Some(process) = self.get_mut(watcher) {
            process
                .port_monitors_mut()
                .push(PortWatch { reference, port });
        }
        // The tag is kept in the side table, not in the watch above (see the field docs).
        remember_tag(&mut self.tags, reference, tag);
        Ok(reference)
    }

    /// `erlang:monitor(port, Target)` against a term: the typed port-monitor entry point.
    ///
    /// `Target` must be a live-or-closed **port** term; a registered **name** is rejected with
    /// the typed [`error::named_port_monitor`] (the local registry holds processes only), so
    /// this never records a process monitor for a port-typed request.
    ///
    /// This is the `erlang:monitor/2` form (no `{tag, Tag}` option).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `target` is not a port — the oracle's `badarg` for
    /// `erlang:monitor(port, NotAPort)`; otherwise propagates [`ProcessTable::monitor_port`].
    pub fn monitor_port_term(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        ports: &mut PortTable,
    ) -> Result<MonitorRef> {
        self.monitor_port_term_with_options(watcher, target, ports, MonitorOptions::NONE)
    }

    /// [`ProcessTable::monitor_port_term`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_port_term`], plus [`error::unsupported_tag`] for a
    /// non-immediate tag.
    pub fn monitor_port_term_with_tag(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        ports: &mut PortTable,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        self.monitor_port_term_with_options(watcher, target, ports, MonitorOptions::with_tag(tag))
    }

    /// [`ProcessTable::monitor_port_term`] with the full `erlang:monitor/3` option set.
    ///
    /// A `{alias, Mode}` option is the typed [`error::unsupported_port_monitor_alias`] (see
    /// [`ProcessTable::monitor_port_with_options`]).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_port_term`], plus [`error::unsupported_tag`] and
    /// [`error::unsupported_port_monitor_alias`].
    pub fn monitor_port_term_with_options(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        ports: &mut PortTable,
        options: MonitorOptions,
    ) -> Result<MonitorRef> {
        if is_atom(target) {
            return Err(error::named_port_monitor());
        }
        let port = PortId::from_eterm(target)?;
        self.monitor_port_with_options(watcher, port, ports, options)
    }

    /// Removes the **port** monitor `watcher` holds under `reference`, reporting whether it
    /// existed.
    ///
    /// This is the port counterpart of [`ProcessTable::demonitor`], which has no handle on a
    /// [`PortTable`]. The `erlang:demonitor/1` BIF layer must therefore try this **first** and
    /// fall back to [`ProcessTable::demonitor`] (then to
    /// [`ProcessTable::demonitor_flush`] for the `[flush]` option) when it reports `false`:
    ///
    /// ```text
    /// if !table.demonitor_port(watcher, reference, ports)? {
    ///     table.demonitor(watcher, reference)?;
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the **watcher** is unknown. A port that has
    /// already closed is **not** an error: [`ProcessTable::port_closed`] has already resolved
    /// and dropped the monitor, so this reports `false` (like `demonitor` of a monitor that is
    /// already gone).
    pub fn demonitor_port(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        ports: &mut PortTable,
    ) -> Result<bool> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let watch = self.get_mut(watcher).and_then(|process| {
            let entries = process.port_monitors_mut();
            let index = entries
                .iter()
                .position(|watch| watch.reference == reference)?;
            Some(entries.remove(index))
        });
        let Some(watch) = watch else {
            return Ok(false);
        };
        // The incoming half lives on the port; a port that closed earlier has already drained
        // it, so an unallocated id here is expected rather than an error.
        if ports.contains(watch.port) {
            ports.demonitor(watch.port, reference)?;
        }
        // The monitor is gone, so its user tag goes with it — and so does its `{alias, Mode}`
        // alias, under the same rule the DOWN site uses: `demonitor`/`reply_demonitor` expire,
        // `explicit_unalias` survives as a standalone alias (the C's `demonitor` replaces the
        // `UNALIAS` monitor entry with an `ERTS_MON_TYPE_ALIAS` one — `bif.c:466-489`).
        forget_tag(&mut self.tags, reference);
        self.apply_alias_on_down(reference);
        Ok(true)
    }

    /// Removes the outgoing monitor `watcher` holds under `reference`, reporting whether it
    /// existed.
    ///
    /// The reference may name a **local** monitor (the ordinary case, whose incoming entry on
    /// the target is dropped too) or an outgoing **remote** monitor, which is removed from the
    /// table and routed to the remote node as a `DOP_DEMONITOR_P` control signal.
    ///
    /// A standalone `erlang:alias/1` alias is **not** a monitor: `demonitor(Ref)` of one answers
    /// `false` and leaves the alias alone, exactly as the C does for an `ERTS_MON_TYPE_ALIAS`
    /// entry (`bif.c:456`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the watcher is unknown; propagates the router's
    /// transport errors for a remote monitor.
    pub fn demonitor(&mut self, watcher: ProcessId, reference: MonitorRef) -> Result<bool> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let local = self
            .get_mut(watcher)
            .and_then(|process| process.monitors_mut().remove(reference));
        let Some(monitor) = local else {
            // No local monitor under that reference: it may be an outgoing remote one. A
            // *standalone alias* is neither, so this reports `false` and leaves the alias in
            // place (the oracle's answer for `demonitor(alias())`).
            let existed = self.demonitor_remote(watcher, reference)?;
            if existed {
                self.apply_alias_on_down(reference);
            }
            return Ok(existed);
        };
        // Drop the matching incoming entry from the (still living) target.
        if let Some(target) = self.get_mut(monitor.target) {
            target
                .monitored_by_mut()
                .retain(|entry| entry.reference != reference);
        }
        // A named monitor's `{Name, node}` descriptor dies with it — and so does its user tag.
        // The `{alias, Mode}` alias follows the same rule as at the DOWN site (see
        // `demonitor_port`): `demonitor`/`reply_demonitor` expire, `explicit_unalias` survives as
        // a standalone alias (the C replaces the monitor entry with an `ERTS_MON_TYPE_ALIAS` one,
        // `bif.c:466-489`).
        self.named_monitors.remove(&reference);
        forget_tag(&mut self.tags, reference);
        self.apply_alias_on_down(reference);
        Ok(true)
    }

    /// `erlang:demonitor(Ref, [flush])`: [`ProcessTable::demonitor`] **and** the removal of any
    /// already-queued `{'DOWN', Ref, …}` from `watcher`'s mailbox.
    ///
    /// The monitor halves are dropped first (so the usual local/remote routing still happens),
    /// then the mailbox is scanned with the queue cursor API and every message recognised by
    /// [`payload_is_down_for`] as *this* reference's DOWN is removed. A fresh cursor is derived
    /// after each removal because a removal shifts every later message down by one.
    ///
    /// # Return value
    ///
    /// `true` when the monitor existed **or** a queued DOWN was removed; the BIF layer renders
    /// `erlang:demonitor/2` as unconditional `true` (the oracle's result), so this `bool` means
    /// "something was removed here", exactly like [`ProcessTable::demonitor`]'s.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `watcher` is unknown; propagates the router's
    /// transport errors for a remote monitor.
    pub fn demonitor_flush(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
    ) -> Result<bool> {
        if !self.contains(watcher) {
            return Err(error::unknown_process());
        }
        let existed = self.demonitor(watcher, reference)?;
        let down = self.atoms.down();
        let removed = {
            let Some(process) = self.get_mut(watcher) else {
                return Ok(existed);
            };
            let mailbox = process.mailbox_mut();
            let mut removed = false;
            let mut cursor = mailbox.cursor();
            loop {
                let position = mailbox.position(cursor);
                match mailbox.message_at(cursor).map(|message| message.payload()) {
                    Some(payload) if payload_is_down_for(payload, reference, down) => {
                        mailbox.remove_at(cursor);
                        removed = true;
                        // Positions shifted down by one: re-derive a cursor at the same index.
                        cursor = mailbox.cursor_at(position);
                    }
                    Some(_) => mailbox.advance(&mut cursor),
                    None => break,
                }
            }
            removed
        };
        Ok(existed || removed)
    }

    /// Removes the outgoing **remote** monitor `watcher` holds under `reference`, reporting
    /// whether it existed, and routes the `DOP_DEMONITOR_P` control signal.
    ///
    /// # Errors
    ///
    /// Propagates the router's transport errors.
    fn demonitor_remote(&mut self, watcher: ProcessId, reference: MonitorRef) -> Result<bool> {
        let (monitor, emptied) = {
            let Some(monitors) = self.remote_monitors.get_mut(&watcher) else {
                return Ok(false);
            };
            let Some(index) = monitors
                .iter()
                .position(|monitor| monitor.reference == reference)
            else {
                return Ok(false);
            };
            let monitor = monitors.remove(index);
            (monitor, monitors.is_empty())
        };
        if emptied {
            self.remote_monitors.remove(&watcher);
        }
        self.route(DistSignal::Demonitor {
            watcher,
            target: monitor.remote,
            reference,
        })?;
        Ok(true)
    }

    // --- reference aliases (`erlang:alias/1`, `erlang:unalias/1`) ------------

    /// Resolves a monitor reference used as a **send destination** to the process a message sent
    /// to it must be delivered to, or `None` when the reference is not a live alias.
    ///
    /// This is the send path's lookup for the `alias/1` and `erlang:monitor/3 {alias, Mode}`
    /// features: a reference minted by either is a valid `Ref ! Msg` destination, and the message
    /// then goes to the **alias owner** — the process that created the alias
    /// ([`AliasEntry::owner`]). Anything else answers `None`, which the BIF layer maps onto the
    /// oracle's `badarg` for `SomeNonRef ! Msg`:
    ///
    /// * a plain monitor reference (never an alias, or one whose mode's rule already fired), and
    /// * a reference whose **owner has died** — every alias of a dying process is swept by
    ///   [`ProcessTable::terminate_into`], so this is a belt-and-braces check that a reference can
    ///   never resolve to a dead pid.
    ///
    /// The lookup is read-only and holds no borrow across the check.
    pub fn resolve_alias(&self, reference: MonitorRef) -> Option<ProcessId> {
        let entry = self.aliases.get(&reference)?;
        if self.contains(entry.owner) {
            Some(entry.owner)
        } else {
            None
        }
    }

    /// Mints a **standalone reference alias** owned by `owner` and returns it — the
    /// `erlang:alias/1` operation.
    ///
    /// The new reference is a valid send destination
    /// ([`ProcessTable::resolve_alias`] answers `owner`) and is removed only by
    /// [`ProcessTable::unalias`] (for [`UnaliasMode::ExplicitUnalias`]) or by the mode's own rule
    /// at the DOWN/reply sites. **No monitor is recorded**: like the C's `ERTS_MON_TYPE_ALIAS`
    /// entry (`bif.c:5930-5945`), a standalone alias has no target, so
    /// [`ProcessTable::demonitor`] of it reports `false`.
    ///
    /// The name "standalone" is the C's distinction: a `{alias, Mode}` alias on a monitor shares
    /// its reference with a real monitor entry, whereas this one exists on its own.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `owner` is not a live local process.
    pub fn create_alias(&mut self, owner: ProcessId, mode: UnaliasMode) -> Result<MonitorRef> {
        if !self.contains(owner) {
            return Err(error::unknown_process());
        }
        let reference = self.refs.next();
        remember_alias(&mut self.aliases, reference, owner, Some(mode));
        Ok(reference)
    }

    /// `erlang:unalias/1`: clears the alias `owner` holds under `reference`, reporting whether one
    /// was cleared.
    ///
    /// This is the C's `unalias_1` (`bif.c:5947-5978`) one-for-one:
    ///
    /// * an entry keyed by `reference` whose owner is `owner` and which genuinely carries an alias
    ///   ⇒ the alias is cleared and `true` is returned. A **monitor** that shares the reference (a
    ///   `{alias, Mode}` monitor) is left intact — the C only clears the alias flags, and deletes
    ///   the entry solely when it is an `ERTS_MON_TYPE_ALIAS` standalone alias; the monitor's
    ///   `{'DOWN', …}` is still delivered,
    /// * no such alias (a plain monitor reference, an already-cleared alias, a reference belonging
    ///   to **another** process, or a reference this table never minted) ⇒ `false`.
    ///
    /// The `Reference` parse itself (a non-reference ⇒ the oracle's `badarg`) is the BIF layer's
    /// job: it calls [`MonitorRef::from_eterm`] before this, exactly as it does for
    /// `erlang:demonitor/1`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `owner` is not a live local process.
    pub fn unalias(&mut self, owner: ProcessId, reference: MonitorRef) -> Result<bool> {
        if !self.contains(owner) {
            return Err(error::unknown_process());
        }
        match self.aliases.get(&reference) {
            Some(entry) if entry.owner == owner => {
                // Only the alias goes: a monitor sharing the reference keeps its `Monitor` /
                // `MonitoredBy` records, its `named_monitors` descriptor and its user tag (the C
                // merely clears `ERTS_ML_STATE_ALIAS_MASK`, `bif.c:5963`).
                forget_alias(&mut self.aliases, reference);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Consumes the alias `reference` **because a message was delivered through it** — the
    /// reply site of a `reply_demonitor` (`ERTS_ML_STATE_ALIAS_ONCE`) alias.
    ///
    /// The C's `handle_altact_msg` (`erl_proc_sig_queue.c:6041-6049`) does two things for an
    /// `ALIAS_ONCE` alias when a message arrives through it: it deletes the alias entry, and —
    /// when the entry is a **monitor** — it sends the demonitor, so the monitor dies with the
    /// alias. A message through a `demonitor`/`explicit_unalias` alias does **not** consume it, so
    /// any other mode (and a reference that is not a live alias at all) is a no-op.
    ///
    /// When a monitor shares the reference, the removal is routed through
    /// [`ProcessTable::demonitor`] so **everything** the demonitor site resolves is resolved here
    /// too: the watcher's outgoing record, the target's incoming one, the `named_monitors`
    /// descriptor, the user tag and any outgoing remote half. A **standalone** `alias/1` alias has
    /// no monitor, so `demonitor` reports `false` and only the alias entry is dropped — exactly
    /// the C's `ERTS_MON_TYPE_ALIAS` arm.
    ///
    /// # Errors
    ///
    /// For a live owner, propagates [`ProcessTable::demonitor`]'s errors (the router's transport
    /// errors for a remote monitor). An alias whose owner has since died is a no-op — nothing
    /// could have been delivered through it anyway.
    pub fn alias_reply_delivered(&mut self, reference: MonitorRef) -> Result<()> {
        let Some(entry) = self.aliases.get(&reference).copied() else {
            // Not an alias, or its rule already fired: the message was still delivered (the
            // caller resolved it), but there is nothing to consume here.
            return Ok(());
        };
        if entry.mode != UnaliasMode::ReplyDemonitor {
            // `demonitor`/`explicit_unalias` aliases survive a delivery (the latter forever, the
            // former until the monitored target goes down).
            return Ok(());
        }
        if self.contains(entry.owner) {
            // A monitor sharing the reference dies with it (`erts_proc_sig_send_demonitor`); a
            // standalone alias is not a monitor, so this answers `false` and changes nothing.
            self.demonitor(entry.owner, reference)?;
        }
        // `demonitor` already cleared the alias for the `ReplyDemonitor` mode (the DOWN-site
        // rule); this covers the standalone-alias case `demonitor` deliberately leaves alone.
        forget_alias(&mut self.aliases, reference);
        Ok(())
    }

    /// Applies the **DOWN-site** alias rule for `reference`.
    ///
    /// The C's DOWN path (`erl_proc_sig_queue.c:6466-6493`) resolves an origin monitor's alias
    /// exactly as its `demonitor` does (`bif.c:466-489`): `ALIAS_ONCE` (`reply_demonitor`) and
    /// `ALIAS_DEMONITOR` (`demonitor`) delete the entry — so the alias dies with the monitor —
    /// while `ALIAS_UNALIAS` (`explicit_unalias`) degrades the monitor entry into a standalone
    /// `ERTS_MON_TYPE_ALIAS` one, so the **alias survives** its target's death.
    ///
    /// This is the rule the two DOWN materialisation sites share (the dying target's watcher sweep
    /// in [`ProcessTable::terminate_into`], the born-down `noproc` paths, and
    /// [`ProcessTable::port_closed`]), and it is why the born-down case needs it: the alias is
    /// registered before the target's liveness is checked, so an `explicit_unalias` alias on an
    /// already-dead target lives on while a `demonitor`/`reply_demonitor` one is gone by the time
    /// the monitor call returns (oracle-verified).
    ///
    /// It is also the rule [`ProcessTable::demonitor`] / [`ProcessTable::demonitor_port`] apply,
    /// because the C's `demonitor` case analysis is literally the same one.
    ///
    /// A reference with no alias entry is a no-op, so every site may call this unconditionally.
    fn apply_alias_on_down(&mut self, reference: MonitorRef) {
        match self.aliases.get(&reference) {
            Some(entry) if entry.mode != UnaliasMode::ExplicitUnalias => {
                forget_alias(&mut self.aliases, reference);
            }
            // An `explicit_unalias` alias survives the DOWN as a standalone alias (the C replaces
            // the monitor entry rather than deleting it).
            _ => {}
        }
    }
    /// Terminates `pid` with `reason`, propagating to its links and monitors and cascading
    /// through the pending-signal worklist.
    ///
    /// The dying process's links are sent **exit signals** and its monitors `{'DOWN', …}`
    /// **signals**, queued on each receiver ([`ProcessTable::service_signals`] applies them);
    /// a receiver that is not the currently executing process is serviced eagerly, so the
    /// observable end state matches a synchronous propagation.
    ///
    /// This form has **no** [`PortTable`] handle, so the dying process's outgoing **port
    /// monitors** are drained but the matching *incoming* entries cannot be removed from the
    /// port table. A caller that has the port table should use
    /// [`ProcessTable::terminate_with_ports`] so a port-watcher's death also cleans the port
    /// side; this method is the `None`-ports delegation of it (today's behaviour).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the pid is unknown. Once the pid **is** live the
    /// termination completes: a per-peer signalling failure is dropped-and-logged and the cascade
    /// continues (see [`ProcessTable::terminate_into`]), so this returns `Ok(())` for a live pid.
    pub fn terminate(&mut self, pid: ProcessId, reason: Eterm) -> Result<()> {
        let mut ports: Option<&mut PortTable> = None;
        self.terminate_and_cascade(&mut ports, pid, reason)
    }

    /// [`ProcessTable::terminate`] with the [`PortTable`] in hand, so a dying process's outgoing
    /// **port monitors** (`erlang:monitor(port, Port)`) also remove their matching *incoming*
    /// entries from `ports` — the outgoing [`PortWatch`] set is drained on death, and without
    /// this the port would keep a dangling monitor for a watcher that no longer exists
    /// (`erlang:port_info(Port, monitors)` would still list the dead pid).
    ///
    /// A link cascade reached from here (a peer that also dies) is cleaned too: the port table
    /// is threaded through the private cascade, exactly as the schedule-in path is through
    /// [`ProcessTable::service_signals_with_ports`].
    ///
    /// `ProcessTable` and `PortTable` are distinct types passed in separately, so `&mut self`
    /// and `&mut ports` are disjoint borrows (the same shape [`ProcessTable::port_closed`] uses).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::terminate`].
    pub fn terminate_with_ports(
        &mut self,
        ports: &mut PortTable,
        pid: ProcessId,
        reason: Eterm,
    ) -> Result<()> {
        let mut ports: Option<&mut PortTable> = Some(ports);
        self.terminate_and_cascade(&mut ports, pid, reason)
    }

    /// The shared body of [`ProcessTable::terminate`] and
    /// [`ProcessTable::terminate_with_ports`]: verifies `pid` is live, runs the signal-generation
    /// half ([`ProcessTable::terminate_into`]) and then drains the cascade.
    fn terminate_and_cascade(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
        reason: Eterm,
    ) -> Result<()> {
        if !self.contains(pid) {
            return Err(error::unknown_process());
        }
        let mut pending = VecDeque::new();
        self.terminate_into(ports, pid, reason, &mut pending)?;
        self.drain_signals(ports, &mut pending)
    }

    /// The signal-generation half of termination: reclaims `pid` and **queues** the signals its
    /// death implies on the affected receivers, pushing each receiver onto the `pending`
    /// worklist. It never recurses, so a cyclic link cascade is bounded.
    ///
    /// `reason` may live in `pid`'s own heap (a terminated exit reason queued for it); it is
    /// copied into every receiver's heap before `pid`'s heap is released.
    ///
    /// `ports` is `Some` when the caller holds the [`PortTable`]: the dying process's drained
    /// outgoing [`PortWatch`]es then remove their incoming [`PortMonitor`](crate::port::PortMonitor)
    /// entries. The **process-death observer** (see [`ProcessExitObserver`]) is notified exactly
    /// once, here, after the death has been propagated and before `drop(process)`.
    ///
    /// # Best-effort propagation (a dying process's termination always completes)
    ///
    /// `pid` is reclaimed **first** (the `take` below bumps its slot serial), so by the time the
    /// per-peer signals are generated the death has *already happened*. Every per-peer step is
    /// therefore **best-effort**: a failure to queue a local `{'EXIT', …}`/`{'DOWN', …}` (an
    /// unreachable receiver heap, or a relocation-exempt / host-gate-declined receiver refusing to
    /// grow) or to route a
    /// remote [`DistSignal::Exit`] is **logged** — never silently swallowed — and the cascade
    /// **continues** to the remaining peers, so the process-death observer (step 5) and the port
    /// monitor cleanup (step 6) always run. Aborting mid-cascade instead would leave later peers
    /// unsignalled, the observer unrun (leaking the host resources it releases, e.g. ETS tables
    /// owned by the pid) and the port table holding dangling incoming `PortMonitor` entries.
    ///
    /// This mirrors what ERTS does with a control signal it cannot deliver: the signal is
    /// **dropped and logged** (`erts_dsig_drop` / `dist.c` — a down remote node, an unreachable
    /// peer), never turned into an abort of the death. A watcher's outgoing monitor record is
    /// resolved regardless of whether its `{'DOWN', …}` reached the wire, because the target is
    /// gone and a retained record could never be resolved.
    ///
    /// # Errors
    ///
    /// Always `Ok(())` today: the process is already reclaimed when the cascade starts, so there
    /// is no remaining table-level failure to report. The `Result` is retained so a genuine
    /// table-level failure can be surfaced without changing every caller.
    fn terminate_into(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
        reason: Eterm,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<()> {
        // A process can be signalled twice within one propagation (two links into the same
        // target); the second visit finds the slot already reclaimed and is a no-op.
        let Some(mut process) = self.take(pid) else {
            return Ok(());
        };

        // The registered name, captured before the registry entry is dropped below and before
        // the dying heap is released: the exit observer needs it (see `ProcessExit::name`).
        let name = self
            .names
            .iter()
            .find(|(_, registered)| **registered == pid)
            .map(|(name, _)| *name);

        // A dying process keeps no name registration.
        self.names.retain(|_, registered| *registered != pid);
        // Snapshot every relationship before the PCB is recycled.
        let links = process.links_mut().drain();
        let watchers: Vec<MonitoredBy> = core::mem::take(process.monitored_by_mut());
        let outgoing = process.monitors_mut().drain();
        // The outgoing **port** monitors are drained here too (they cannot outlive the watcher):
        // their matching incoming entries are removed from the port table below, and the exit
        // observer is shown which ports the dying process was watching. `PortWatch` is a pair of
        // plain values, so this snapshot is heap-independent.
        let port_monitors = core::mem::take(process.port_monitors_mut());
        process.set_exit_reason(reason);
        process.set_state(ProcessState::Terminated);
        // `process` stays alive until the end of this body: `reason` lives in its heap, so
        // every receiver must be handed a *copy* before the dying heap is released (the drop
        // sits after the loops). The dying process's own pending signals go with it.
        //
        // Each receiver is *recorded* here and scheduled for servicing **after** the drop: the
        // dying process has already been reclaimed (`take` above), so `is_running` would report
        // it wrongly, and — more importantly — a receiver that is a still-`Running` process
        // must keep its signal pending. The eager-vs-deferred decision is therefore made by
        // `schedule_service`, once the receivers are fully populated.
        let mut receivers: Vec<ProcessId> = Vec::new();

        // 1. Links: drop the back-link, then queue an exit signal on the peer. The peer decides
        //    (through `exit_policy`, at service time) whether it terminates or traps.
        //
        //    Best-effort (see the fn docs): a peer whose heap cannot take the reason copy is
        //    dropped-and-logged and the cascade continues — `pid` is already reclaimed, so the
        //    remaining peers must still be told.
        for target in links {
            if let Some(peer) = self.get_mut(target) {
                peer.links_mut().remove(pid);
            }
            if !self.contains(target) {
                // The peer died earlier in this same propagation.
                continue;
            }
            if let Err(failure) =
                self.enqueue_exit_signal(target, pid, reason, Some(process.off_heap()))
            {
                beam_common::beam_error!(
                    "beam_process: dropping exit signal {:?} -> {:?}: {:?}; termination continues",
                    pid,
                    target,
                    failure
                );
                continue;
            }
            receivers.push(target);
        }

        // 2. Monitors: every watcher is told the target is down, carrying the monitor's
        //    `erlang:monitor/3` user tag when it has one. The records hold no tag field, so it is
        //    read from the side table; the DOWN that consumes it is its last use, so the entry is
        //    dropped as soon as the signal is queued.
        //
        //    The watcher's **outgoing** record goes too: the oracle resolves both halves when the
        //    target's death is signalled, so `erlang:process_info(W, monitors)` stops listing the
        //    monitor and a later `demonitor(Ref)` reports that it is gone (only
        //    `demonitor(Ref, [info])` distinguishes them — `demonitor/1` answers `true`
        //    unconditionally). Leaving it behind would dangle forever. The `named_monitors`
        //    `{Name, node}` descriptor is **not** dropped here: it is consumed by
        //    `deliver_down_message` when the (possibly deferred) DOWN is materialised, and the
        //    DOWN must still carry the name.
        for watcher in watchers {
            if !self.contains(watcher.watcher) {
                // The watcher died earlier in this same propagation.
                continue;
            }
            let tag = self.tags.get(&watcher.reference).copied();
            let queued = match self.enqueue_down_signal(
                watcher.watcher,
                watcher.reference,
                pid,
                watcher.kind,
                tag,
                reason,
                Some(process.off_heap()),
            ) {
                Ok(()) => true,
                Err(failure) => {
                    // Best-effort (see the fn docs): the watcher's heap could not take the DOWN.
                    // Log and carry on — the target is dead either way, so the outgoing record is
                    // still resolved below (it could never be resolved afterwards), and the named
                    // monitor's `{Name, node}` descriptor goes with it: nothing will ever
                    // materialise the DOWN that would have consumed it.
                    beam_common::beam_error!(
                        "beam_process: dropping DOWN signal {:?} -> {:?}: {:?}; termination \
                         continues",
                        pid,
                        watcher.watcher,
                        failure
                    );
                    self.named_monitors.remove(&watcher.reference);
                    false
                }
            };
            forget_tag(&mut self.tags, watcher.reference);
            // The monitor is resolved, so its `{alias, Mode}` alias follows the DOWN-site rule:
            // `demonitor`/`reply_demonitor` expire with their monitor, while an
            // `explicit_unalias` alias survives as a standalone alias (the C replaces the monitor
            // entry with an `ERTS_MON_TYPE_ALIAS` one — `erl_proc_sig_queue.c:6466-6493`).
            self.apply_alias_on_down(watcher.reference);
            if let Some(watcher_process) = self.get_mut(watcher.watcher) {
                watcher_process.monitors_mut().remove(watcher.reference);
            }
            if queued {
                receivers.push(watcher.watcher);
            }
        }

        // 3. The dying process's own outgoing monitors vanish with it — and so do the
        //    `{Name, node}` descriptor and the user tag of any named/tagged monitor among them
        //    (nothing will ever report them down).
        for monitor in &outgoing {
            let reference = monitor.reference;
            if let Some(target) = self.get_mut(monitor.target) {
                target
                    .monitored_by_mut()
                    .retain(|entry| entry.reference != reference);
            }
            self.named_monitors.remove(&reference);
            forget_tag(&mut self.tags, reference);
        }

        // 3b. Reference aliases owned by the dying process all go with it — **including** the
        //     standalone `erlang:alias/1` aliases, which are not monitor records and so are not
        //     among `outgoing` above. A message sent to a reference whose owner has died has no
        //     destination: `resolve_alias` would answer `None` anyway (it re-checks liveness), but
        //     dropping the entries here keeps the table from accumulating records for dead pids
        //     and makes the guarantee unconditional.
        self.aliases.retain(|_, entry| entry.owner != pid);
        // 4. Remote links and monitors: a dying process leaves the node's records, and every
        //    remote link partner must be told the process is gone. This runs before
        //    `drop(process)` because `reason` may live in the dying heap the router reads. A
        //    link partner's death is the **link** exit, so it routes `DistSignal::Exit`
        //    (`DOP_EXIT`) — never `DistSignal::Exit2` (`erlang:exit/2`'s `DOP_EXIT2`).
        //
        //    Best-effort (see the fn docs): a router transport failure (a down remote node, an
        //    I/O error) drops-and-logs that signal — exactly ERTS's `erts_dsig_drop` — and the
        //    remaining partners are still routed; the death must not abort on a peer that cannot
        //    be reached. `remote_links` was already drained, so no record is left behind.
        for remote in self.remote_links.remove(&pid).unwrap_or_default() {
            if let Err(failure) = self.route(DistSignal::Exit {
                from: pid,
                to: remote,
                reason,
            }) {
                beam_common::beam_error!(
                    "beam_process: dropping remote exit signal {:?} -> {:?}: {:?}; termination \
                     continues",
                    pid,
                    remote,
                    failure
                );
            }
        }

        // Outgoing *remote* monitors die with the watcher: the remote node will simply
        // never see a `DOP_DEMONITOR_P`, which mirrors the local case above.
        self.remote_monitors.remove(&pid);
        // 5. Notify the process-death observer exactly once, now that the death has been
        //    propagated and *before* `drop(process)`: `reason` may live in the dying heap, so the
        //    payload is only valid up to the drop below. The observer is isolated (a panic is
        //    caught and logged, never propagated — see `ProcessExitObserver`).
        self.notify_process_exit(pid, reason, name, &port_monitors);

        // 6. Port monitors: the dying process was watching every `port_monitors` entry, so its
        //    matching *incoming* `PortMonitor` must be removed from the port table — otherwise a
        //    port would keep a dangling monitor for a watcher that no longer exists
        //    (`erlang:port_info(Port, monitors)` would still list the dead pid). With no port
        //    table in hand this is skipped (the observer still saw the watches). A port that has
        //    already closed has drained its side, so an unallocated id is not an error.
        //
        //    Best-effort per watch (see the fn docs): one watch that cannot be cleaned must not
        //    leave the remaining watches dangling.
        if let Some(ports) = ports.as_deref_mut() {
            for watch in &port_monitors {
                if ports.contains(watch.port) {
                    if let Err(failure) = ports.demonitor(watch.port, watch.reference) {
                        beam_common::beam_error!(
                            "beam_process: port-monitor cleanup for {:?} on port {:?} failed: \
                             {:?}; termination continues",
                            pid,
                            watch.port,
                            failure
                        );
                    }
                }
            }
        }
        // Every copy that read `reason` from the dying heap is done — release it.
        drop(process);

        // Now that the dying PCB is gone, schedule the receivers the death signalled: each is
        // serviced eagerly unless it is the currently executing process.
        for receiver in receivers {
            self.schedule_service(receiver, pending);
        }
        Ok(())
    }

    /// Applies every signal queued for `pid`, in FIFO order, cascading to any process those
    /// signals affect.
    ///
    /// This is the **schedule-in** entry point: the scheduler calls it on the dequeued pid
    /// before running it, so a process signalled while it was blocked sees the signal before
    /// its next reduction. A pid that is no longer live has nothing to apply, so it is a no-op.
    ///
    /// This form has **no** [`PortTable`] handle: a cascade that *terminates* a process drains
    /// its outgoing port monitors but cannot remove the matching incoming entries from the port
    /// table. A caller holding the port table should use
    /// [`ProcessTable::service_signals_with_ports`]; this method is its `None`-ports delegation
    /// (today's behaviour).
    ///
    /// # Errors
    ///
    /// Propagates the message builders' `BeamError::Alloc` (after a collection) and the
    /// cascading termination's errors.
    pub fn service_signals(&mut self, pid: ProcessId) -> Result<()> {
        let mut ports: Option<&mut PortTable> = None;
        self.service_signals_and_cascade(&mut ports, pid)
    }

    /// [`ProcessTable::service_signals`] with the [`PortTable`] in hand, so a schedule-in cascade
    /// that terminates a process also removes that process's incoming port monitors (see
    /// [`ProcessTable::terminate_with_ports`], whose port-cleaning this reuses).
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::service_signals`].
    pub fn service_signals_with_ports(
        &mut self,
        ports: &mut PortTable,
        pid: ProcessId,
    ) -> Result<()> {
        let mut ports: Option<&mut PortTable> = Some(ports);
        self.service_signals_and_cascade(&mut ports, pid)
    }

    /// The shared body of [`ProcessTable::service_signals`] and
    /// [`ProcessTable::service_signals_with_ports`].
    fn service_signals_and_cascade(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
    ) -> Result<()> {
        if !self.contains(pid) {
            return Ok(());
        }
        let mut pending = VecDeque::new();
        pending.push_back(pid);
        self.drain_signals(ports, &mut pending)
    }

    /// Drains the signal queues of every pid in `pending`, and of any process the servicing
    /// itself signals, until no queue is left to drain.
    ///
    /// Iterative — the `pending` worklist is the only recursion the cascade has, so a link
    /// cycle (A↔B) terminates in a bounded number of steps.
    ///
    /// `ports` is threaded through so a cascade that terminates a process can clean its incoming
    /// port monitors (see [`ProcessTable::terminate_with_ports`]).
    fn drain_signals(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<()> {
        while let Some(pid) = pending.pop_front() {
            while self.apply_head_signal(ports, pid, pending)? {}
        }
        Ok(())
    }

    /// Applies the signal at the head of `pid`'s queue, removing it, and returns whether the
    /// process **survived** (`false` when the queue is empty, the process is gone, or the
    /// signal terminated it).
    ///
    /// A terminating signal adds the affected receivers to `pending` (through
    /// [`ProcessTable::terminate_into`]) rather than recurring.
    fn apply_head_signal(
        &mut self,
        ports: &mut Option<&mut PortTable>,
        pid: ProcessId,
        pending: &mut VecDeque<ProcessId>,
    ) -> Result<bool> {
        let Some(signal) = self.head_signal(pid) else {
            return Ok(false);
        };
        match signal {
            PendingSignal::Exit { from, reason } => {
                let Some(process) = self.get(pid) else {
                    return Ok(false);
                };
                let trapping = process.is_trapping_exit();
                match exit_policy(reason, trapping, &self.atoms) {
                    ExitPolicy::Ignore => {
                        // A `normal` reason to a non-trapping process is discarded.
                        self.pop_head_signal(pid);
                        Ok(true)
                    }
                    ExitPolicy::Trap => {
                        self.deliver_trapped_exit(pid, from)?;
                        Ok(true)
                    }
                    ExitPolicy::Terminate(reason) => {
                        // `reason` still lives in `pid`'s heap, which `terminate_into`
                        // reclaims last, so it can be copied to the peers first.
                        self.terminate_into(ports, pid, reason, pending)?;
                        Ok(false)
                    }
                }
            }
            PendingSignal::Down {
                reference,
                target,
                kind,
                tag,
                ..
            } => {
                self.deliver_down_message(pid, reference, target, kind, tag)?;
                Ok(true)
            }
        }
    }

    /// The signal at the head of `pid`'s queue, if any (a copy of the small descriptor).
    fn head_signal(&self, pid: ProcessId) -> Option<PendingSignal> {
        lookup(&self.slots, pid)?.signals().peek().copied()
    }

    /// The reason of the signal at the head of `pid`'s queue, if any.
    fn head_signal_reason(&self, pid: ProcessId) -> Option<Eterm> {
        lookup(&self.slots, pid)?
            .signals()
            .peek()
            .map(PendingSignal::reason)
    }

    /// Removes the signal at the head of `pid`'s queue.
    fn pop_head_signal(&mut self, pid: ProcessId) {
        if let Some(process) = lookup_mut(&mut self.slots, pid) {
            process.signals_mut().pop();
        }
    }

    /// Whether `pid` names the currently executing process (`ProcessState::Running`).
    fn is_running(&self, pid: ProcessId) -> bool {
        matches!(
            lookup(&self.slots, pid).map(Process::state),
            Some(ProcessState::Running)
        )
    }

    /// Adds `pid` to the `pending` service worklist **unless** it is the currently executing
    /// process, whose signals are deferred to its next schedule-in.
    ///
    /// This is the single place the eager-vs-deferred rule is decided; every signal-generation
    /// path funnels its receivers through it. The rule is stated in the module docs: a signal
    /// generated for a process that is *not* running right now is serviced before the generating
    /// operation returns (ERTS's early signal handling), which is what makes the observable end
    /// state of `terminate`/`apply_exit_signal`/`send_exit` match a synchronous propagation for
    /// a non-running receiver, while a running receiver keeps its signal pending.
    fn schedule_service(&self, pid: ProcessId, pending: &mut VecDeque<ProcessId>) {
        if !self.is_running(pid) {
            pending.push_back(pid);
        }
    }

    /// Builds the head signal's message with `build` and enqueues it as `from` → `pid`.
    ///
    /// The queued signal is an in-heap sub-rootset, so a heap-exhaustion collection relocates
    /// its `reason` in place: `build` is handed a **freshly read** reason each attempt. Exhaustion
    /// follows the shared policy: an ordinary `pid` collects and grows its nursery on demand and
    /// the build is retried once ([`grow_receiver_for_signal`]); a `pid` whose live terms are not
    /// all in the crate root set
    /// ([`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots)) **or**
    /// whose relocating collection the host's [`CollectionGate`] declines must not have its heap
    /// move under them, so its message is built into a heap fragment instead
    /// ([`enqueue_fragment_signal`]) — no collection, no growth, no retry. On success the head
    /// signal is removed and a blocked `pid` is woken (and recorded on the wake list).
    fn deliver_head_signal<F>(&mut self, pid: ProcessId, from: Eterm, mut build: F) -> Result<()>
    where
        F: FnMut(&mut Heap, Eterm) -> Result<Eterm>,
    {
        // Read once, before the delivery loop: the gate is consulted before the recipient borrow
        // is taken (it needs `&mut self`), and the decline is stable for this one delivery.
        let decline = self.declines_signal_collection(pid);
        let mut attempt = 0;
        loop {
            let Some(reason) = self.head_signal_reason(pid) else {
                // The queue emptied (or the process is gone): nothing to deliver.
                return Ok(());
            };
            let Some(recipient) = lookup_mut(&mut self.slots, pid) else {
                return Ok(());
            };
            match build(recipient.heap_mut(), reason) {
                Ok(term) => {
                    recipient.mailbox_mut().enqueue(Message::new(term, from));
                    break;
                }
                Err(BeamError::Alloc(failure))
                    if attempt == 0 && matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
                {
                    attempt += 1;
                    if recipient.has_unpublished_roots() || decline {
                        // The heap must not move under the live terms the root set does not carry,
                        // nor under a receiver whose collection the host declined.
                        enqueue_fragment_signal(recipient, from, reason, &mut build)?;
                        break;
                    }                    // A collection relocates the queued reason word: re-read it next iteration.
                    grow_receiver_for_signal(recipient, failure, decline)?;
                }
                Err(error) => return Err(error),
            }
        }
        self.pop_head_signal(pid);
        let woke = lookup_mut(&mut self.slots, pid)
            .map(|process| process.wake())
            .unwrap_or(false);
        if woke {
            self.ready.push(pid);
        }
        Ok(())
    }

    /// Delivers the head (exit) signal of `pid` as a `{'EXIT', from, reason}` message.
    ///
    /// The reason is already owned by `pid`'s heap, so it is built without copying (the
    /// earlier `copy_reason_onto` at enqueue time did the ownership transfer). The message's
    /// **`from`** is the signalling pid's immediate, as [`ProcessTable::deliver_down_message`]
    /// documents.
    fn deliver_trapped_exit(&mut self, pid: ProcessId, from: ProcessId) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let from_term = from.to_eterm();
        self.deliver_head_signal(pid, from_term, move |heap, reason| {
            build_exit_message(heap, &atoms, from, reason)
        })
    }

    /// Delivers the head (monitor-DOWN) signal of `pid` as a `{'DOWN', …}` message — or
    /// `{Tag, …}` when the queued signal carried an `erlang:monitor/3` user tag.
    ///
    /// A **named** monitor's DOWN carries the `{Name, node}` pair it was created with rather than
    /// the pid the name resolved to (the oracle's behaviour); an ordinary monitor carries the
    /// pid. The named descriptor is consumed here: the DOWN is its last use.
    ///
    /// The message's **`from`** is always the resolved internal pid **immediate**
    /// ([`ProcessId::to_eterm`]): the C requires `is_immed(ERL_MESSAGE_FROM(mp))`
    /// (`erl_proc_sig_queue.c:8441`), and a `{Name, node}`/external-pid target belongs in the
    /// message **body**, never in `from` — which is not a GC root.
    fn deliver_down_message(
        &mut self,
        pid: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let named = self.named_monitors.get(&reference).copied();
        let kind_atom = match kind {
            MonitorKind::Process => atoms.process(),
            MonitorKind::Port => atoms.port(),
        };
        let from_term = target.to_eterm();
        self.deliver_head_signal(pid, from_term, move |heap, reason| {
            let reference_term = reference.to_eterm(heap)?;
            let target_term = match named {
                Some(nt) => heap.tuple(&[make_atom(nt.name), make_atom(nt.node)])?,
                None => target.to_eterm(),
            };
            build_down_message_term(
                heap,
                &atoms,
                tag,
                reference_term,
                kind_atom,
                target_term,
                reason,
            )
        })?;
        // The named descriptor is consumed by the delivered DOWN (its last use).
        self.named_monitors.remove(&reference);
        Ok(())
    }
    /// Enqueues an exit signal on `to`'s signal queue, copying `reason` out of the heap
    /// described by `src` (the signaller's off-heap table, or `None` for a self-signal whose
    /// reason already lives in `to`'s heap) so the queued reason always outlives the signaller.
    ///
    /// A `to` whose live terms are not all in the crate root set
    /// ([`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots)) **or**
    /// whose relocating collection the host's [`CollectionGate`] declines, and whose
    /// full nursery refuses that copy, is the one case the queue cannot serve — the queued reason
    /// must live on the receiver's own heap (the queue is an in-heap sub-rootset) and that heap
    /// must not be collected or grown — so a **trapping** receiver gets the
    /// `{'EXIT', From, Reason}` the queue would have delivered built into a heap fragment instead
    /// ([`ProcessTable::deliver_exit_fragment`]). A **non-trapping** one would receive no message
    /// at all (the signal carries the exit *policy*, and terminating the receiver is the caller's
    /// cascade), so its raw refusal is surfaced unchanged.
    ///
    /// An unknown `to` is skipped: the caller is propagating a signal through the table and
    /// the target may have died earlier in the same propagation.
    fn enqueue_exit_signal(
        &mut self,
        to: ProcessId,
        from: ProcessId,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let decline = self.declines_signal_collection(to);
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        // Both are read before the copy: the fragment route below needs `self` again, and
        // `recipient`'s borrow must be over by then.
        let refuses_collect = recipient.has_unpublished_roots() || decline;
        let trapping = recipient.is_trapping_exit();
        match copy_reason_onto(recipient, reason, src, decline) {
            Ok(reason) => {
                recipient
                    .signals_mut()
                    .push(PendingSignal::exit(from, reason));
                Ok(())
            }
            Err(BeamError::Alloc(failure))
                if refuses_collect
                    && matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
            {
                if trapping {
                    self.deliver_exit_fragment(to, from, reason, src)
                } else {
                    Err(BeamError::Alloc(failure))
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Enqueues a monitor-DOWN signal on `to`'s signal queue, copying `reason` out of the heap
    /// described by `src` (see [`ProcessTable::enqueue_exit_signal`]). `tag` is the monitor's
    /// `erlang:monitor/3` user tag — read from the side table by the caller, since the records
    /// carry none — and is a heap-independent immediate, carried verbatim.
    ///
    /// A `to` whose live terms are not all in the crate root set
    /// ([`Process::has_unpublished_roots`](crate::process::Process::has_unpublished_roots)) **or**
    /// whose relocating collection the host's [`CollectionGate`] declines, and whose
    /// full nursery refuses that copy, cannot hold the queued signal at all (see
    /// [`ProcessTable::enqueue_exit_signal`]), so the DOWN is built into a heap fragment instead
    /// ([`ProcessTable::deliver_down_fragment`]) — a DOWN carries no exit policy, so it is the whole
    /// signal.
    fn enqueue_down_signal(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let decline = self.declines_signal_collection(to);
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        // Read before the copy: the fragment route below needs `self` again, and `recipient`'s
        // borrow must be over by then.
        let refuses_collect = recipient.has_unpublished_roots() || decline;
        match copy_reason_onto(recipient, reason, src, decline) {
            Ok(reason) => {
                recipient
                    .signals_mut()
                    .push(PendingSignal::down(reference, target, kind, tag, reason));
                Ok(())
            }
            Err(BeamError::Alloc(failure))
                if refuses_collect
                    && matches!(failure.kind, AllocKind::Heap | AllocKind::Message) =>
            {
                self.deliver_down_fragment(to, reference, target, kind, tag, reason, src)
            }
            Err(error) => Err(error),
        }
    }

    /// Builds the `{'EXIT', From, Reason}` a **trapping** receiver's queued exit signal would have
    /// been materialised into and enqueues it as a **heap fragment** — the enqueue-time route of
    /// [`enqueue_fragment_payload`], taken when `to` is **relocation-exempt**/`Running` or the
    /// host's [`CollectionGate`] declines and its full nursery
    /// refused the reason copy ([`ProcessTable::enqueue_exit_signal`]).
    ///
    /// The signal never reaches the queue (its reason cannot live there), so this wakes a blocked
    /// `to` itself, exactly as [`ProcessTable::deliver_head_signal`] does for the queued form.
    ///
    /// An unknown `to` is skipped (it died earlier in the same propagation).
    fn deliver_exit_fragment(
        &mut self,
        to: ProcessId,
        from: ProcessId,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let from_term = from.to_eterm();
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        enqueue_fragment_payload(recipient, from_term, reason, src, &mut |heap, reason| {
            build_exit_message(heap, &atoms, from, reason)
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(to);
        }
        Ok(())
    }

    /// Builds the `{'DOWN', Ref, Kind, Target, Reason}` message the queue's materialiser
    /// ([`ProcessTable::deliver_down_message`]) would have built and enqueues it as a **heap
    /// fragment** — the enqueue-time route of [`enqueue_fragment_payload`], taken when `to` is
    /// **relocation-exempt**/`Running` or the host's [`CollectionGate`] declines and its full
    /// nursery refused the reason copy
    /// ([`ProcessTable::enqueue_down_signal`]).
    ///
    /// The DOWN never reaches the queue (its reason cannot live there), so this wakes a blocked
    /// `to` itself, exactly as [`ProcessTable::deliver_head_signal`] does for the queued form. The
    /// named monitor's `{Name, node}` descriptor (if any) is consumed here, exactly as the
    /// materialiser consumes it: the DOWN is its last use.
    ///
    /// An unknown `to` is skipped (it died earlier in the same propagation).
    fn deliver_down_fragment(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        target: ProcessId,
        kind: MonitorKind,
        tag: Option<Eterm>,
        reason: Eterm,
        src: Option<&OffHeapTable>,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        // Read before the recipient lookup borrows the table, exactly as `deliver_down_message`
        // does: a named monitor's DOWN carries the `{Name, node}` pair rather than the pid.
        let named = self.named_monitors.get(&reference).copied();
        let kind_atom = match kind {
            MonitorKind::Process => atoms.process(),
            MonitorKind::Port => atoms.port(),
        };
        let from_term = target.to_eterm();
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        enqueue_fragment_payload(recipient, from_term, reason, src, &mut |heap, reason| {
            let reference_term = reference.to_eterm(heap)?;
            let target_term = match named {
                Some(nt) => heap.tuple(&[make_atom(nt.name), make_atom(nt.node)])?,
                None => target.to_eterm(),
            };
            build_down_message_term(
                heap,
                &atoms,
                tag,
                reference_term,
                kind_atom,
                target_term,
                reason,
            )
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(to);
        }
        // The named descriptor is consumed by the delivered DOWN (its last use).
        self.named_monitors.remove(&reference);
        Ok(())
    }

    /// The number of signals queued for `pid` (pending-signal introspection).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn pending_signals(&self, pid: ProcessId) -> Result<usize> {
        self.get(pid)
            .map(Process::pending_signals)
            .ok_or_else(error::unknown_process)
    }

    /// The signal queue of `pid`, for iteration over the pending signals.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when `pid` is unknown.
    pub fn signals(&self, pid: ProcessId) -> Result<&crate::signal_queue::SignalQueue> {
        self.get(pid)
            .map(Process::signals)
            .ok_or_else(error::unknown_process)
    }

    // --- remote (distributed) links and monitors -----------------------------

    /// Installs the distribution seam the table routes remote signals through.
    ///
    /// Without a router the table is **record-only**: local relationships are tracked but no
    /// wire control signal is emitted (see [`ProcessTable::route`]).
    pub fn set_dist_router(&mut self, router: Box<dyn DistSignalRouter>) {
        self.router = Some(router);
    }

    /// Takes the distribution seam out of the table, returning it to the caller.
    pub fn take_dist_router(&mut self) -> Option<Box<dyn DistSignalRouter>> {
        self.router.take()
    }

    /// Whether a distribution seam is installed.
    pub fn has_dist_router(&self) -> bool {
        self.router.is_some()
    }

    /// Routes `signal` through the distribution seam, if one is installed.
    ///
    /// With no seam the local record stands and the signal is dropped: the relationship is
    /// still known to the table, there is simply nowhere to send it.
    ///
    /// # Errors
    ///
    /// Propagates the router's transport failure (an unknown/unconnected node, an I/O error).
    fn route(&mut self, signal: DistSignal) -> Result<()> {
        match &mut self.router {
            Some(router) => router.route(signal),
            None => Ok(()),
        }
    }

    // --- process-death observer ----------------------------------------------

    /// Installs the process-death observer seam the table calls once per death.
    ///
    /// The observer is invoked from `ProcessTable::terminate_into` after a dying process's
    /// links/monitors have been signalled and before its heap is released — the one place the
    /// runtime can release the resources it allocated on the pid's behalf (the ETS tables the
    /// pid owns are the motivating case). Without an observer termination is unchanged
    /// (see [`ProcessExitObserver`]).
    pub fn set_process_exit_observer(&mut self, observer: Box<dyn ProcessExitObserver>) {
        self.exit_observer = Some(observer);
    }

    /// Takes the process-death observer out of the table, returning it to the caller.
    pub fn take_process_exit_observer(&mut self) -> Option<Box<dyn ProcessExitObserver>> {
        self.exit_observer.take()
    }

    /// Whether a process-death observer is installed.
    pub fn has_process_exit_observer(&self) -> bool {
        self.exit_observer.is_some()
    }

    /// Notifies the installed process-death observer that `pid` died with `reason`, exactly once
    /// per death.
    ///
    /// Called by [`ProcessTable::terminate_into`] after the death has been propagated and before
    /// the dying heap is released (so `reason`, which may live there, is still valid). A
    /// **panicking** observer is contained — the panic is caught, reported through
    /// [`beam_common`]'s logging and swallowed — so it can never abort or corrupt termination
    /// (see [`ProcessExitObserver`]'s failure policy). With no observer installed this is a
    /// no-op.
    fn notify_process_exit(
        &mut self,
        pid: ProcessId,
        reason: Eterm,
        name: Option<Atom>,
        port_monitors: &[PortWatch],
    ) {
        let Some(observer) = self.exit_observer.as_mut() else {
            return;
        };
        let exit = ProcessExit::new(pid, reason, name, port_monitors);
        // SAFETY (unwind): the observer receives only an immutable payload and holds no
        // reference to the table, so a panic inside it cannot leave the table half-updated —
        // see `ProcessExitObserver`'s failure policy. The panic is reported and contained so
        // termination still completes.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            observer.process_exited(&exit);
        }));
        if outcome.is_err() {
            beam_common::beam_error!(
                "beam_process: process-exit observer panicked for pid {:?}; termination continues",
                pid
            );
        }
    }
    /// `erlang:link/1` against an arbitrary pid term: a local pid links through
    /// [`ProcessTable::link`], an external pid is recorded as a remote link and routed as a
    /// `DOP_LINK` control signal.
    ///
    /// The local branch inherits both of [`ProcessTable::link`]'s properties for a **non-live**
    /// local target: no error, and a `noproc` exit signal from that pid applied to `a` (deferred
    /// when `a` is currently executing).
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `b` is not a pid; for a local `b`,
    /// propagates [`ProcessTable::link`]'s `BeamError::TermNotFound` when the **caller** `a` is
    /// unknown; for a remote `b`, `BeamError::TermNotFound` when `a` is unknown; and the
    /// router's transport errors.
    pub fn link_term(&mut self, a: ProcessId, b: Eterm) -> Result<()> {
        match PidTarget::from_eterm(b)? {
            PidTarget::Local(pid) => self.link(a, pid),
            PidTarget::Remote(remote) => {
                if !self.contains(a) {
                    return Err(error::unknown_process());
                }
                let links = self.remote_links.entry(a).or_default();
                // Like `Links::add`: a re-link of an already-linked pair changes nothing.
                if !links.contains(&remote) {
                    links.push(remote);
                }
                self.route(DistSignal::Link { from: a, to: remote })
            }
        }
    }

    /// `erlang:unlink/1` against an arbitrary pid term: a local pid unlinks through
    /// [`ProcessTable::unlink`], an external pid drops the recorded remote link and routes a
    /// `DOP_UNLINK_ID` control signal.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `b` is not a pid; for a local `b`,
    /// propagates [`ProcessTable::unlink`]'s `BeamError::TermNotFound`; and the router's
    /// transport errors.
    pub fn unlink_term(&mut self, a: ProcessId, b: Eterm) -> Result<()> {
        match PidTarget::from_eterm(b)? {
            PidTarget::Local(pid) => self.unlink(a, pid),
            PidTarget::Remote(remote) => {
                let emptied = match self.remote_links.get_mut(&a) {
                    Some(links) => {
                        if let Some(index) = links.iter().position(|entry| *entry == remote) {
                            links.remove(index);
                        }
                        links.is_empty()
                    }
                    None => false,
                };
                if emptied {
                    // An emptied link set carries no information.
                    self.remote_links.remove(&a);
                }
                self.route(DistSignal::Unlink { from: a, to: remote })
            }
        }
    }

    /// `erlang:monitor/2` against an arbitrary pid, port **or name** term: a local pid monitors
    /// through [`ProcessTable::monitor`], an external pid is recorded as an outgoing remote
    /// monitor and routed as a `DOP_MONITOR_P` control signal, and an **atom** is a registered
    /// name resolved through [`ProcessTable::monitor_named`] on the local node.
    ///
    /// A **port** target cannot be monitored here: a port monitor's incoming half lives in a
    /// [`PortTable`], which this entry point has no handle on. The faithful entry points are
    /// [`ProcessTable::monitor_port`]/[`ProcessTable::monitor_port_term`], so a port target with
    /// [`MonitorKind::Port`] is a typed [`error::unsupported`] rather than a wrong result (and,
    /// as the oracle requires, a port target with [`MonitorKind::Process`] is `badarg`).
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `target` is not a pid, port or atom, and
    /// for a port target with [`MonitorKind::Process`]; the typed [`error::unsupported`] when
    /// `target` is an atom but no local node has been installed
    /// ([`ProcessTable::set_local_node`]), when the atom names a **port** monitor
    /// ([`error::named_port_monitor`]), or for a port target with [`MonitorKind::Port`]; for a
    /// local `target`, propagates [`ProcessTable::monitor`]'s `BeamError::TermNotFound`; for a
    /// remote `target`, `BeamError::TermNotFound` when `watcher` is unknown; and the router's
    /// transport errors.
    pub fn monitor_term(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        kind: MonitorKind,
    ) -> Result<MonitorRef> {
        self.monitor_term_with_options(watcher, target, kind, MonitorOptions::NONE)
    }

    /// [`ProcessTable::monitor_term`] with an `erlang:monitor/3` `{tag, Tag}` **user tag**: a
    /// local pid dispatches to [`ProcessTable::monitor_with_tag`] and an atom name to
    /// [`ProcessTable::monitor_named_with_tag`].
    ///
    /// A **remote** (external-pid) target with a tag is the typed [`error::unsupported`]: the
    /// monitor signal on the wire (`DistSignal::Monitor` → `DOP_MONITOR_P`) has no tag field, so
    /// the tag could not be carried to the node that will eventually build the `{Tag, …}` DOWN —
    /// a typed diagnostic rather than a silently untagged notification.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_term`], plus [`error::unsupported_tag`] for a non-immediate
    /// tag and [`error::unsupported`] for a tag on a remote target.
    pub fn monitor_term_with_tag(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        kind: MonitorKind,
        tag: Option<Eterm>,
    ) -> Result<MonitorRef> {
        self.monitor_term_with_options(watcher, target, kind, MonitorOptions::with_tag(tag))
    }

    /// [`ProcessTable::monitor_term`] with the full `erlang:monitor/3` option set: the `{tag, Tag}`
    /// user tag **and** the `{alias, Mode}` reference alias.
    ///
    /// Every local dispatch reaches an alias-capable entry point
    /// ([`ProcessTable::monitor_with_options`] for a pid,
    /// [`ProcessTable::monitor_named_with_options`] for a registered name), so a
    /// `monitor(process, Target, [{alias, Mode}])` behaves exactly like the direct call.
    ///
    /// A **port** term is refused in each of its forms, as before: `MonitorKind::Port` is the typed
    /// [`error::unsupported`] (a port term needs the port table — use
    /// [`ProcessTable::monitor_port_term`]) and `MonitorKind::Process` is the oracle's `badarg`.
    /// The port-table entry points do accept the `{tag, Tag}` option; an `{alias, Mode}` one is the
    /// typed [`error::unsupported_port_monitor_alias`] there.
    ///
    /// A **remote** (external-pid) target accepts neither option: the wire monitor signal
    /// (`DistSignal::Monitor` → `DOP_MONITOR_P`) carries no tag and no alias, so each is a typed
    /// [`error::unsupported`] rather than a silently dropped option.
    ///
    /// # Errors
    ///
    /// As [`ProcessTable::monitor_term_with_tag`], plus [`error::unsupported`] for an
    /// `{alias, Mode}` option on a remote target.
    pub fn monitor_term_with_options(
        &mut self,
        watcher: ProcessId,
        target: Eterm,
        kind: MonitorKind,
        options: MonitorOptions,
    ) -> Result<MonitorRef> {
        let tag = check_tag(options.tag)?;
        if PortId::is_port(target) {
            return match kind {
                MonitorKind::Port => Err(error::unsupported(
                    "monitor/2: port target requires the port table (use monitor_port)",
                )),
                // `erlang:monitor(process, Port)` is `badarg` in the oracle.
                MonitorKind::Process => Err(error::bad_pid()),
            };
        }
        if is_atom(target) {
            if kind == MonitorKind::Port {
                return Err(error::named_port_monitor());
            }
            // `monitor(process, Name)` names a locally-registered process; the `{Name, LocalNode}`
            // target needs the node's atom, which only the runtime can install. Guessing one would
            // be a wrong result, so its absence is a typed diagnostic.
            let Some(node) = self.local_node else {
                return Err(error::unsupported(
                    "monitor/2: name target requires the local node (set_local_node)",
                ));
            };
            return self.monitor_named_with_options(watcher, atom_val(target), node, kind, options);
        }
        match PidTarget::from_eterm(target)? {
            PidTarget::Local(pid) => self.monitor_with_options(watcher, pid, kind, options),
            PidTarget::Remote(remote) => {
                if tag.is_some() {
                    // The wire monitor signal has no tag field (see the method docs).
                    return Err(error::unsupported(
                        "monitor/3: tag on a remote target (DOP_MONITOR_P carries no tag)",
                    ));
                }
                if options.alias.is_some() {
                    // ...and no alias field either: the remote node builds the DOWN from its own
                    // monitor record, so it could not know when to expire the alias.
                    return Err(error::unsupported(
                        "monitor/3: {alias, Mode} on a remote target (DOP_MONITOR_P carries no alias)",
                    ));
                }
                if !self.contains(watcher) {
                    return Err(error::unknown_process());
                }
                let reference = self.refs.next();
                self.remote_monitors
                    .entry(watcher)
                    .or_default()
                    .push(RemoteMonitor {
                        reference,
                        remote,
                        kind,
                    });
                self.route(DistSignal::Monitor {
                    watcher,
                    target: remote,
                    reference,
                    kind,
                })?;
                Ok(reference)
            }
        }
    }
    /// `erlang:exit/2` against an arbitrary pid term: a local pid exits through
    /// [`ProcessTable::send_exit`], an external pid is routed as a `DOP_EXIT2` control signal
    /// ([`DistSignal::Exit2`] — a plain exit signal, *not* the link exit `DOP_EXIT`).
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `to` is not a pid; for a local `to`,
    /// propagates [`ProcessTable::send_exit`]'s errors; and the router's transport errors.
    pub fn send_exit_term(&mut self, from: ProcessId, to: Eterm, reason: Eterm) -> Result<()> {
        match PidTarget::from_eterm(to)? {
            PidTarget::Local(pid) => self.send_exit(from, pid, reason),
            PidTarget::Remote(remote) => self.route(DistSignal::Exit2 {
                from,
                to: remote,
                reason,
            }),
        }
    }

    /// The remote link partners recorded for `pid`.
    ///
    /// A pid that is no longer live resolves to nothing — the same rule
    /// [`ProcessTable::whereis`] applies to a name whose process is gone.
    pub fn remote_links(&self, pid: ProcessId) -> &[RemotePid] {
        if !self.contains(pid) {
            return &[];
        }
        self.remote_links
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The outgoing remote monitors recorded for `pid`.
    ///
    /// A pid that is no longer live resolves to nothing (see
    /// [`ProcessTable::remote_links`]).
    pub fn remote_monitors(&self, pid: ProcessId) -> &[RemoteMonitor] {
        if !self.contains(pid) {
            return &[];
        }
        self.remote_monitors
            .get(&pid)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// A whole remote **node** went away (its distribution connection dropped): every local
    /// relationship with that node is resolved the way the signals would have been, had the
    /// node been alive to send them, and then purged.
    ///
    /// This is a collect-then-apply sweep, matching the crate's discipline (the table is the
    /// only place that mutates two processes at once, so it never mutates while iterating):
    ///
    /// 1. every outgoing monitor on `node` queues `{'DOWN', Ref, process, RemotePid, Reason}`
    ///    on its watcher and wakes a blocked watcher;
    /// 2. every link to `node` follows [`exit_policy`] on the local peer: `Ignore` does
    ///    nothing, `Trap` queues `{'EXIT', RemotePid, Reason}`, anything else terminates the
    ///    peer (with the usual cascading propagation);
    /// 3. every record mentioning `node` is purged.
    ///
    /// `reason` is the node-down reason (an immediate such as `noconnection`) and is embedded
    /// verbatim — there is no source heap to copy it from, so the caller must pass a term that
    /// does not live in a heap that may be recycled underneath this call.
    ///
    /// # Errors
    ///
    /// Propagates the message builders' `BeamError::Alloc` (after a collection) and the
    /// cascading termination's errors.
    pub fn node_down(&mut self, node: Eterm, reason: Eterm) -> Result<()> {
        // 1. Monitors: snapshot the (watcher, monitor) pairs that watch `node`.
        let down: Vec<(ProcessId, RemoteMonitor)> = self
            .remote_monitors
            .iter()
            .flat_map(|(watcher, monitors)| {
                monitors
                    .iter()
                    .filter(|monitor| monitor.remote.node() == node)
                    .map(move |monitor| (*watcher, *monitor))
            })
            .collect();
        for (watcher, monitor) in down {
            self.enqueue_remote_down_message(watcher, monitor.reference, monitor.remote, reason)?;
        }

        // 2. Links: snapshot the (peer, remote) pairs linked to `node`.
        let linked: Vec<(ProcessId, RemotePid)> = self
            .remote_links
            .iter()
            .flat_map(|(peer, partners)| {
                partners
                    .iter()
                    .filter(|remote| remote.node() == node)
                    .map(move |remote| (*peer, *remote))
            })
            .collect();
        for (peer, remote) in linked {
            let Some(process) = self.get(peer) else {
                // The peer died earlier in this sweep.
                continue;
            };
            let trapping = process.is_trapping_exit();
            match exit_policy(reason, trapping, &self.atoms) {
                ExitPolicy::Ignore => {}
                ExitPolicy::Trap => self.enqueue_remote_exit_message(peer, remote, reason)?,
                ExitPolicy::Terminate(peer_reason) => self.terminate(peer, peer_reason)?,
            }
        }

        // 3. Purge every record mentioning `node`: the relationships are gone with it.
        for partners in self.remote_links.values_mut() {
            partners.retain(|remote| remote.node() != node);
        }
        self.remote_links.retain(|_, partners| !partners.is_empty());
        for monitors in self.remote_monitors.values_mut() {
            monitors.retain(|monitor| monitor.remote.node() != node);
        }
        self.remote_monitors
            .retain(|_, monitors| !monitors.is_empty());
        Ok(())
    }

    /// Enqueues `{'DOWN', Ref, process, RemotePid, Reason}` into `watcher`'s mailbox — built on
    /// the watcher's own heap — and wakes a blocked watcher.
    ///
    /// The message's **`from`** is the remote **node atom** — an immediate — exactly as the C
    /// stores the node atom for a `DIST_PROC`/`DIST_PORT` monitor
    /// (`erl_proc_sig_queue.c:4984`/`:5014`); the external-pid target stays in the message
    /// **body**. `from` is not a GC root, so it may never hold the heap-materialised external pid.
    ///
    /// An unknown watcher is skipped (it died earlier in the same sweep).
    fn enqueue_remote_down_message(
        &mut self,
        watcher: ProcessId,
        reference: MonitorRef,
        remote: RemotePid,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let decline = self.declines_signal_collection(watcher);
        let Some(recipient) = lookup_mut(&mut self.slots, watcher) else {
            return Ok(());
        };
        build_signal_message(recipient, remote.node(), reason, decline, |heap, reason| {
            let reference_term = reference.to_eterm(heap)?;
            // A remote monitor carries no user tag (see `monitor_term_with_tag`), so the
            // DOWN keeps the default `'DOWN'` head atom; the closure materialises the remote
            // pid into the message *body*.
            build_down_message_remote(heap, &atoms, None, reference_term, remote, reason)
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(watcher);
        }
        Ok(())
    }
    /// Enqueues `{'EXIT', RemotePid, Reason}` into `peer`'s mailbox — built on the peer's own
    /// heap — and wakes a blocked peer.
    ///
    /// The message's **`from`** is the remote **node atom** — an immediate — as the C stores for
    /// a node-down exit (`erl_proc_sig_queue.c:4984`/`:5014`); the external-pid link partner
    /// stays in the message **body**. `from` is not a GC root, so it may never hold the
    /// heap-materialised external pid.
    ///
    /// An unknown peer is skipped (it died earlier in the same sweep).
    fn enqueue_remote_exit_message(
        &mut self,
        peer: ProcessId,
        remote: RemotePid,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let decline = self.declines_signal_collection(peer);
        let Some(recipient) = lookup_mut(&mut self.slots, peer) else {
            return Ok(());
        };
        build_signal_message(recipient, remote.node(), reason, decline, |heap, reason| {
            let from = remote.to_eterm(heap)?;
            build_exit_message_term(heap, &atoms, from, reason)
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(peer);
        }
        Ok(())
    }
    // --- name registry -------------------------------------------------------

    /// Registers `pid` under `name` (`erlang:register/2`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the name is already taken **or** when `pid` is not a live
    /// local process — `erlang:register/2` raises `badarg` for a dead/foreign pid, and a name
    /// must never be bound to a process that can never run.
    pub fn register(&mut self, name: Atom, pid: ProcessId) -> Result<()> {
        if self.names.contains_key(&name) {
            return Err(error::name_taken());
        }
        if !self.contains(pid) {
            return Err(error::bad_pid());
        }
        self.names.insert(name, pid);
        Ok(())
    }

    /// Removes `name` from the registry (`erlang:unregister/1`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the name is not registered — `erlang:unregister/1`
    /// raises `badarg`, not `TermNotFound`.
    pub fn unregister(&mut self, name: Atom) -> Result<()> {
        match self.names.remove(&name) {
            Some(_) => Ok(()),
            None => Err(error::bad_pid()),
        }
    }

    /// The process registered under `name`, if any.
    pub fn whereis(&self, name: Atom) -> Option<ProcessId> {
        let pid = self.names.get(&name).copied()?;
        // A name whose process is gone resolves to nothing.
        if self.contains(pid) {
            Some(pid)
        } else {
            None
        }
    }

    /// Iterates the registered `(name, pid)` pairs.
    pub fn registered(&self) -> impl Iterator<Item = (Atom, ProcessId)> + '_ {
        self.names
            .iter()
            .filter(|(_, pid)| self.contains(**pid))
            .map(|(name, pid)| (*name, *pid))
    }

    /// The name `pid` is registered under (`erlang:process_info(Pid, registered_name)`).
    ///
    /// `None` for an unregistered process **and** for a pid that is not live: a stale
    /// registration left behind by a dead process reports nothing, the same rule
    /// [`ProcessTable::whereis`] applies in the other direction.
    pub fn name_of(&self, pid: ProcessId) -> Option<Atom> {
        if !self.contains(pid) {
            return None;
        }
        self.names
            .iter()
            .find(|(_, registered)| **registered == pid)
            .map(|(name, _)| *name)
    }

    // --- message builders ----------------------------------------------------

    /// The off-heap table a signal's reason must be copied *from*: the source process's table,
    /// `None` when the source and the recipient are the same process (the reason then already
    /// lives in the recipient's own heap), or `fallback` when the source is not (or no longer)
    /// a live process.
    fn source_off_heap<'a>(
        &'a self,
        recipient: ProcessId,
        source: ProcessId,
        fallback: &'a OffHeapTable,
    ) -> Option<&'a OffHeapTable> {
        if source == recipient {
            None
        } else {
            Some(self.get(source).map(Process::off_heap).unwrap_or(fallback))
        }
    }

    // --- port close propagation ----------------------------------------------

    /// Reports that `port` has closed with `reason`: delivers
    /// `{'DOWN', Ref, port, Port, Reason}` to every process monitoring the port (via
    /// [`PortTable::close`]) and an `{'EXIT', Port, Reason}` signal to every process linked
    /// to it.
    ///
    /// A monitoring watcher's outgoing [`PortWatch`] is removed here too — the port's death
    /// resolves the monitor on both sides, exactly as [`ProcessTable::terminate`] resolves a
    /// process monitor — so `erlang:process_info(Watcher, monitors)` stops listing it.
    ///
    /// A linked process follows [`exit_policy`]: a trapping peer receives the `{'EXIT', …}`
    /// message, a non-trapping one terminates with `reason` (cascading through its own links
    /// and monitors), and a `normal` reason is ignored. A peer that already died is skipped.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `port` is not allocated; propagates the message
    /// builders' `BeamError::Alloc` (after a collection) and the cascading termination's
    /// errors.
    pub fn port_closed(&mut self, ports: &mut PortTable, port: PortId, reason: Eterm) -> Result<()> {
        let closed = ports.close(port)?;

        // 1. Monitors: the port's death resolves the incoming half here (drained by `close`)
        //    and the watcher's outgoing half below; then the watcher is told the port is down.
        //    The records hold no tag field, so the monitor's `erlang:monitor/3` user tag is read
        //    from the side table; the DOWN built below is its last use, so the entry is dropped.
        for monitor in closed.monitors {
            if let Some(watcher) = self.get_mut(monitor.watcher) {
                watcher
                    .port_monitors_mut()
                    .retain(|watch| watch.reference != monitor.reference);
            }
            let tag = self.tags.get(&monitor.reference).copied();
            forget_tag(&mut self.tags, monitor.reference);
            // A port monitor cannot carry an alias in this crate
            // (`error::unsupported_port_monitor_alias`), so this is a no-op today; it is called
            // anyway so the DOWN site applies the one shared rule rather than relying on the
            // invariant holding forever.
            self.apply_alias_on_down(monitor.reference);
            self.enqueue_port_down_message(monitor.watcher, monitor.reference, port, tag, reason)?;
        }

        // 2. Links: each linked process either traps the exit or follows it.
        for pid in closed.links {
            let Some(peer) = self.get(pid) else {
                // The peer died earlier.
                continue;
            };
            let trapping = peer.is_trapping_exit();
            match exit_policy(reason, trapping, &self.atoms) {
                ExitPolicy::Ignore => {}
                ExitPolicy::Trap => self.enqueue_port_exit_message(pid, port, reason)?,
                ExitPolicy::Terminate(peer_reason) => self.terminate_with_ports(ports, pid, peer_reason)?,
            }
        }
        Ok(())
    }

    /// Enqueues `{'DOWN', Ref, port, Port, Reason}` into `to`'s mailbox — built on `to`'s own
    /// heap — and wakes `to` when it was blocked. With `tag = Some`, the head atom is the
    /// monitor's `erlang:monitor/3` user tag (`{Tag, Ref, port, Port, Reason}`).
    ///
    /// An unknown `to` is skipped (the watcher died earlier).
    fn enqueue_port_down_message(
        &mut self,
        to: ProcessId,
        reference: MonitorRef,
        port: PortId,
        tag: Option<Eterm>,
        reason: Eterm,
    ) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let from = port.to_eterm();
        let decline = self.declines_signal_collection(to);
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        build_signal_message(recipient, from, reason, decline, |heap, reason| {
            let reference_term = reference.to_eterm(heap)?;
            build_down_message_port(heap, &atoms, tag, reference_term, port, reason)
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(to);
        }
        Ok(())
    }

    /// Enqueues `{'EXIT', Port, Reason}` into `to`'s mailbox — built on `to`'s own heap, or into
    /// a heap fragment when `to` is **relocation-exempt**/`Running` or the host's
    /// [`CollectionGate`] declines (see
    /// [`ProcessTable::deliver_head_signal`]) — and wakes `to` when it was blocked.
    ///
    /// An unknown `to` is skipped (the linked peer died earlier).
    fn enqueue_port_exit_message(&mut self, to: ProcessId, port: PortId, reason: Eterm) -> Result<()> {
        let atoms = self.atoms; // Copy: the build closure captures it by value.
        let from = port.to_eterm();
        let decline = self.declines_signal_collection(to);
        let Some(recipient) = lookup_mut(&mut self.slots, to) else {
            return Ok(());
        };
        build_signal_message(recipient, from, reason, decline, |heap, reason| {
            build_exit_message_term(heap, &atoms, from, reason)
        })?;
        let woke = recipient.wake();
        if woke {
            self.ready.push(to);
        }
        Ok(())
    }
}#[cfg(test)]
mod tests {
    //! Unit tests for pid allocation/serial bumping/stale-pid rejection, the name
    //! registry, link/unlink, the exit-signal outcomes and monitor/demonitor. The cascading
    //! link/monitor propagation is covered by `tests/signals.rs` and the port
    //! ownership/link/monitor surface by `crate::port` and `tests/ports.rs`.

    use beam_term::compare;
    use beam_term::prelude::{
        is_tuple, make_atom, make_small, tuple_element, tuple_size, AtomTable,
    };

    use super::*;

    /// A table with a fresh atom table behind it.
    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    /// A table plus the atom table its [`SignalAtoms`] were interned from, so a test can
    /// resolve the well-known names (`normal`, `kill`, …) back to terms.
    fn table_and_names() -> (ProcessTable, AtomTable) {
        let mut names = AtomTable::new();
        let atoms = SignalAtoms::from_table(&mut names);
        (ProcessTable::new(atoms), names)
    }

    /// The term for the interned name `name`.
    fn named(names: &AtomTable, name: &str) -> Eterm {
        make_atom(names.lookup(name).expect("the signal atoms interned it"))
    }

    #[test]
    fn alloc_pid_hands_out_distinct_pids() {
        let mut table = table();
        let first = table.alloc_pid().expect("room in the number space");
        let second = table.alloc_pid().expect("room in the number space");
        let third = table.alloc_pid().expect("room in the number space");
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_ne!(first, third);
        // Allocating a pid does not by itself install a process.
        assert_eq!(table.len(), 0);
        assert!(!table.contains(first));
    }

    #[test]
    fn spawn_installs_and_remove_frees_then_reuse_bumps_the_serial() {
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        assert!(table.contains(pid));
        assert_eq!(table.len(), 1);

        let process = table.remove(pid).expect("the pid was live");
        assert_eq!(process.pid(), pid);
        assert!(!table.contains(pid), "a removed pid no longer resolves");
        assert_eq!(table.len(), 0);

        let reused = table.spawn_default().expect("the freed number is reused");
        assert_eq!(reused.number(), pid.number());
        assert!(
            reused.serial() > pid.serial(),
            "a reused slot carries a bumped serial"
        );
        assert!(table.contains(reused));
        // The stale pid still does not resolve.
        assert!(!table.contains(pid));
        assert!(table.get(pid).is_none());
    }

    #[test]
    fn take_of_an_unknown_or_stale_pid_is_none() {
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        assert!(table.take(pid).is_some());
        assert!(table.take(pid).is_none(), "the second take finds nothing");
        assert!(table.take(ProcessId::new(999, 0)).is_none());
    }

    #[test]
    fn pids_iterates_exactly_the_live_processes() {
        let mut table = table();
        let a = table.spawn_default().expect("a");
        let b = table.spawn_default().expect("b");
        let c = table.spawn_default().expect("c");
        table.remove(b);

        let mut live: Vec<ProcessId> = table.pids().collect();
        live.sort();
        let mut expected = vec![a, c];
        expected.sort();
        assert_eq!(live, expected);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn get_mut_reaches_the_pcb() {
        let mut table = table();
        let pid = table.spawn_default().expect("a");
        table
            .get_mut(pid)
            .expect("the pcb is there")
            .bump_reductions(3);
        assert_eq!(table.get(pid).expect("still there").reductions(), 3);
        assert!(table.get(ProcessId::new(0, 7)).is_none());
    }

    #[test]
    fn deliver_wakes_a_blocked_target_and_records_it_on_the_wake_list() {
        let mut table = table();
        let sender = table.spawn_default().expect("sender");
        let receiver = table.spawn_default().expect("receiver");

        // A runnable target is enqueued but not woken.
        assert_eq!(
            table
                .deliver_simple(receiver, sender.to_eterm(), make_small(1))
                .expect("the receiver is live"),
            DeliverOutcome::Enqueued
        );
        assert!(table.woken().is_empty());

        // A blocked target is woken and put on the wake list.
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_state(ProcessState::Waiting);
        assert_eq!(
            table
                .deliver_simple(receiver, sender.to_eterm(), make_small(2))
                .expect("the receiver is live"),
            DeliverOutcome::Woke
        );
        assert_eq!(table.woken(), &[receiver]);
        assert_eq!(
            table.get(receiver).expect("the receiver is live").state(),
            ProcessState::Runnable
        );

        // Draining returns and clears the list.
        assert_eq!(table.drain_woken(), vec![receiver]);
        assert!(table.woken().is_empty());

        // Delivering to an unknown pid is an error.
        assert!(matches!(
            table.deliver_simple(ProcessId::new(999, 0), sender.to_eterm(), make_small(3)),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn taking_a_woken_process_clears_its_pending_wake() {
        let mut table = table();
        let sender = table.spawn_default().expect("sender");
        let receiver = table.spawn_default().expect("receiver");
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_state(ProcessState::Waiting);
        table
            .deliver_simple(receiver, sender.to_eterm(), make_small(1))
            .expect("the receiver is live");
        assert_eq!(table.woken(), &[receiver]);

        // The process dies before it is resumed: its stale wake must be dropped.
        table.remove(receiver).expect("the receiver is live");
        assert!(table.woken().is_empty());
    }

    #[test]
    fn a_trapped_exit_message_wakes_a_blocked_recipient() {
        let (mut table, _names) = table_and_names();
        let sender = table.spawn_default().expect("sender");
        let receiver = table.spawn_default().expect("receiver");
        table.get_mut(receiver).expect("receiver").set_trap_exit(true);
        table
            .get_mut(receiver)
            .expect("receiver")
            .set_state(ProcessState::Waiting);

        assert_eq!(
            table
                .apply_exit_signal(
                    receiver,
                    ExitSignal {
                        from: sender,
                        reason: make_small(9),
                    },
                )
                .expect("the receiver is live"),
            ExitEffect::Trapped
        );
        assert_eq!(table.get(receiver).expect("receiver").mailbox().len(), 1);
        assert_eq!(
            table.get(receiver).expect("receiver").state(),
            ProcessState::Runnable,
            "the trapped exit message woke the receiver"
        );
        assert_eq!(table.woken(), &[receiver]);
    }

    #[test]
    fn a_terminated_link_wakes_a_blocked_trapping_peer() {
        let (mut table, _names) = table_and_names();
        let a = table.spawn_default().expect("a");
        let b = table.spawn_default().expect("b");
        table.get_mut(b).expect("b").set_trap_exit(true);
        table.link(a, b).expect("both pids are live");
        table.get_mut(b).expect("b").set_state(ProcessState::Waiting);

        table.terminate(a, make_small(7)).expect("a is live");
        assert!(!table.contains(a));
        assert_eq!(
            table.get(b).expect("b").state(),
            ProcessState::Runnable,
            "the link exit message woke the trapping peer"
        );
        assert_eq!(table.woken(), &[b]);
    }

    #[test]
    fn a_down_message_wakes_a_blocked_watcher() {
        let (mut table, _names) = table_and_names();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both pids are live");
        table
            .get_mut(watcher)
            .expect("watcher")
            .set_state(ProcessState::Waiting);

        table.terminate(target, make_small(3)).expect("target is live");
        assert_eq!(
            table.get(watcher).expect("watcher").state(),
            ProcessState::Runnable,
            "the down message woke the watcher"
        );
        assert_eq!(table.woken(), &[watcher]);
    }

    #[test]
    fn link_and_unlink_are_two_sided_and_reject_unknown_pids() {
        let mut table = table();
        let a = table.spawn_default().expect("a");
        let b = table.spawn_default().expect("b");

        table.link(a, b).expect("both live");
        assert!(table.get(a).expect("a").links().contains(b));
        assert!(table.get(b).expect("b").links().contains(a));
        // Re-linking is a no-op.
        table.link(a, b).expect("idempotent");
        assert_eq!(table.get(a).expect("a").links().len(), 1);

        table.unlink(a, b).expect("both live");
        assert!(!table.get(a).expect("a").links().contains(b));
        assert!(!table.get(b).expect("b").links().contains(a));

        // An unknown **caller** is still `TermNotFound` (the dead-target case is a signal, not an
        // error — see `link_to_a_non_live_target_signals_noproc_from_it`).
        assert!(matches!(
            table.link(ProcessId::new(999, 0), a),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.link_term(ProcessId::new(999, 0), a.to_eterm()),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.unlink(ProcessId::new(999, 0), a),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn link_to_a_non_live_target_signals_noproc_from_it() {
        // Oracle probe (OTP 29.1.1): a **trapping** caller's `link(DeadPid)` answers `true` and
        // delivers `{'EXIT', DeadPid, noproc}`; a non-trapping caller dies with the `noproc` raise
        // (whose uncaught reason is `{noproc, Stacktrace}`). Links are set semantics: nothing is
        // recorded for a pid that is not there.
        let (mut table, names) = table_and_names();
        let noproc = named(&names, "noproc");
        let dead = ProcessId::new(999, 0);

        // The caller of `erlang:link/1` is the **currently executing** process, so its signal is
        // queued (the `exit/2` model) rather than applied inline.
        let trapper = table.spawn_default().expect("trapper");
        table.get_mut(trapper).expect("trapper").set_trap_exit(true);
        table
            .get_mut(trapper)
            .expect("trapper")
            .set_state(ProcessState::Running);
        table.link(trapper, dead).expect("a non-live target is not an error");
        assert_eq!(
            table.pending_signals(trapper).expect("trapper is live"),
            1,
            "one noproc signal is queued for the caller"
        );
        assert!(
            table.get(trapper).expect("trapper").links().is_empty(),
            "no link is recorded to a pid that is not there"
        );
        assert!(table.get(dead).is_none(), "the target stays non-live");

        table.service_signals(trapper).expect("trapper is live");
        assert!(table.contains(trapper), "the trapping caller survives");
        let payload = table
            .get(trapper)
            .expect("trapper")
            .mailbox()
            .peek()
            .expect("the trapped exit is queued")
            .payload();
        // SAFETY: the payload is the live 3-tuple the signal builder wrote on the trapper's heap.
        unsafe {
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 0), named(&names, "EXIT"));
            assert_eq!(
                tuple_element(payload, 1),
                dead.to_eterm(),
                "the signal came *from* the non-live target"
            );
            assert_eq!(tuple_element(payload, 2), noproc);
        }

        // A non-trapping caller is terminated with `noproc` once its signals are serviced.
        let victim = table.spawn_default().expect("victim");
        table
            .get_mut(victim)
            .expect("victim")
            .set_state(ProcessState::Running);
        table.link(victim, dead).expect("a non-live target is not an error");
        assert!(table.contains(victim));
        assert_eq!(table.pending_signals(victim).expect("victim"), 1);
        table.service_signals(victim).expect("victim is live");
        assert!(
            !table.contains(victim),
            "the non-trapping caller is terminated with noproc"
        );

        // `link_term`'s local branch routes through the same path.
        let via_term = table.spawn_default().expect("via_term");
        table.get_mut(via_term).expect("via_term").set_trap_exit(true);
        table
            .get_mut(via_term)
            .expect("via_term")
            .set_state(ProcessState::Running);
        table
            .link_term(via_term, dead.to_eterm())
            .expect("a non-live local target is not an error");
        assert_eq!(table.pending_signals(via_term).expect("via_term"), 1);
    }

    #[test]
    fn apply_exit_signal_ignores_traps_or_terminates() {
        let (mut table, names) = table_and_names();
        let normal = named(&names, "normal");

        // `normal` to a non-trapping target is ignored.
        let target = table.spawn_default().expect("target");
        let from = table.spawn_default().expect("from");
        assert_eq!(
            table
                .apply_exit_signal(
                    target,
                    ExitSignal { from, reason: normal }
                )
                .expect("a live target"),
            ExitEffect::Ignored
        );
        assert!(table.contains(target));

        // A non-atom "other" reason: to a non-trapping target it terminates.
        let other = make_small(1234);
        assert_eq!(
            table
                .apply_exit_signal(
                    target,
                    ExitSignal { from, reason: other }
                )
                .expect("a live target"),
            ExitEffect::Terminated
        );
        assert!(!table.contains(target));

        // A trapping target gets `{'EXIT', from, reason}` and survives.
        let target = table.spawn_default().expect("target");
        table.get_mut(target).expect("target").set_trap_exit(true);
        assert_eq!(
            table
                .apply_exit_signal(
                    target,
                    ExitSignal { from, reason: other }
                )
                .expect("a live target"),
            ExitEffect::Trapped
        );
        assert!(table.contains(target));
        let mailbox = table.get(target).expect("target").mailbox();
        assert_eq!(mailbox.len(), 1);
        // SAFETY: the payload is the live 3-tuple built by the signal builder.
        unsafe {
            let payload = mailbox.peek().expect("one message").payload();
            assert_eq!(tuple_size(payload), 3);
            assert_eq!(tuple_element(payload, 1), from.to_eterm());
            assert_eq!(tuple_element(payload, 2), other);
        }

        assert!(matches!(
            table.apply_exit_signal(
                ProcessId::new(999, 0),
                ExitSignal { from, reason: other }
            ),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn kill_is_untrappable() {
        let (mut table, names) = table_and_names();
        let kill = named(&names, "kill");

        let target = table.spawn_default().expect("target");
        let from = table.spawn_default().expect("from");
        table.get_mut(target).expect("target").set_trap_exit(true);
        assert_eq!(
            table
                .apply_exit_signal(target, ExitSignal { from, reason: kill })
                .expect("a live target"),
            ExitEffect::Terminated,
            "kill terminates even a trapping process"
        );
        assert!(!table.contains(target));
    }

    #[test]
    fn monitor_and_demonitor_keep_both_halves_in_sync() {
        let (mut table, names) = table_and_names();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");

        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both live");
        assert_eq!(
            table
                .get(watcher)
                .expect("watcher")
                .monitors()
                .find(reference)
                .map(|m| m.target),
            Some(target)
        );
        assert_eq!(table.get(target).expect("target").monitored_by().len(), 1);

        assert!(table.demonitor(watcher, reference).expect("watcher live"));
        assert!(table.get(watcher).expect("watcher").monitors().is_empty());
        assert!(table.get(target).expect("target").monitored_by().is_empty());
        assert!(!table.demonitor(watcher, reference).expect("watcher live"));

        // An unknown *watcher* is still an error.
        assert!(matches!(
            table.demonitor(ProcessId::new(999, 0), reference),
            Err(BeamError::TermNotFound)
        ));

        // A pid that is not a live local process is NOT an error: the monitor is born down, its
        // `{'DOWN', Ref, process, Target, noproc}` is delivered immediately, and no half of the
        // monitor is recorded (there is nothing left to watch).
        let dead = ProcessId::new(999, 0);
        let down_atom = table.atoms.down();
        let mailbox_before = table.get(watcher).expect("watcher").mailbox().len();
        let dead_ref = table
            .monitor(watcher, dead, MonitorKind::Process)
            .expect("a non-live target is not an error");
        assert_ne!(dead_ref, reference);
        assert!(table.get(dead).is_none(), "the target stays non-live");
        let watcher_process = table.get(watcher).expect("watcher");
        assert_eq!(
            watcher_process.mailbox().len(),
            mailbox_before + 1,
            "the immediate noproc DOWN landed in the watcher's mailbox"
        );
        assert!(
            watcher_process.monitors().find(dead_ref).is_none(),
            "a non-live target records no outgoing monitor"
        );
        let payload = watcher_process
            .mailbox()
            .peek()
            .expect("the DOWN is queued")
            .payload();
        assert!(
            payload_is_down_for(payload, dead_ref, down_atom),
            "a 5-tuple `{{'DOWN', Ref, ..}}` for the fresh reference"
        );
        // SAFETY: the payload is the live 5-tuple the DOWN builder wrote.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
            assert_eq!(tuple_element(payload, 2), named(&names, "process"));
            assert_eq!(tuple_element(payload, 3), dead.to_eterm());
            assert_eq!(tuple_element(payload, 4), named(&names, "noproc"));
        }
    }

    #[test]
    fn demonitor_after_the_target_died_still_drops_the_outgoing_entry() {
        let mut table = table();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both live");
        table.remove(target);
        assert!(table.demonitor(watcher, reference).expect("watcher live"));
        assert!(table.get(watcher).expect("watcher").monitors().is_empty());
    }

    #[test]
    fn demonitor_flush_removes_only_the_matching_down() {
        let mut table = table();
        let watcher = table.spawn_default().expect("watcher");

        // Two monitors on two non-live pids: each queues its own immediate noproc DOWN.
        let first = table
            .monitor(watcher, ProcessId::new(999, 0), MonitorKind::Process)
            .expect("a non-live target is not an error");
        let second = table
            .monitor(watcher, ProcessId::new(998, 0), MonitorKind::Process)
            .expect("a non-live target is not an error");
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 2);

        // A reference the table never handed out flushes nothing (and is not an error).
        let bogus = table.refs.next();
        assert!(!table.demonitor(watcher, bogus).expect("watcher live"));
        assert!(!table.demonitor_flush(watcher, bogus).expect("watcher live"));
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 2);

        // Flushing the second reference removes exactly its own DOWN.
        assert!(table
            .demonitor_flush(watcher, second)
            .expect("watcher live"));
        assert_eq!(
            table.get(watcher).expect("watcher").mailbox().len(),
            1,
            "only the second DOWN was flushed"
        );
        let remaining = table
            .get(watcher)
            .expect("watcher")
            .mailbox()
            .peek()
            .expect("one message")
            .payload();
        assert!(
            payload_is_down_for(remaining, first, table.atoms.down()),
            "the surviving DOWN is the first monitor's"
        );

        // The first DOWN goes too; re-flushing changes nothing.
        assert!(table.demonitor_flush(watcher, first).expect("watcher live"));
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
        assert!(!table.demonitor_flush(watcher, first).expect("watcher live"));

        // An unknown watcher is still an error.
        assert!(matches!(
            table.demonitor_flush(ProcessId::new(999, 0), first),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn plain_demonitor_leaves_a_queued_down_while_flush_removes_it() {
        let mut table = table();
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");

        let reference = table
            .monitor(watcher, target, MonitorKind::Process)
            .expect("both live");
        table.terminate(target, make_small(5)).expect("live");
        assert_eq!(
            table.get(watcher).expect("watcher").mailbox().len(),
            1,
            "the eagerly applied DOWN is queued"
        );

        // `demonitor/1` (no flush) drops the monitor half but leaves the DOWN queued.
        table.demonitor(watcher, reference).expect("watcher live");
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 1);

        // `demonitor/2` with `[flush]` removes it.
        assert!(table
            .demonitor_flush(watcher, reference)
            .expect("watcher live"));
        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
    }

    #[test]
    fn monitor_named_reports_the_name_node_target_on_death() {
        let (mut table, names) = table_and_names();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let node = atoms_table.intern("nonode@nohost");
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        table.register(name, target).expect("the name is free");

        let reference = table
            .monitor_named(watcher, name, node, MonitorKind::Process)
            .expect("the name resolves to a live process");
        // The monitor halves are the ordinary ones: the name resolved to a pid.
        assert_eq!(
            table
                .get(watcher)
                .expect("watcher")
                .monitors()
                .find(reference)
                .map(|m| m.target),
            Some(target)
        );
        assert_eq!(table.get(target).expect("target").monitored_by().len(), 1);

        table.terminate(target, make_small(7)).expect("target live");
        let payload = table
            .get(watcher)
            .expect("watcher")
            .mailbox()
            .peek()
            .expect("the DOWN is queued")
            .payload();
        // SAFETY: the payload is the live 5-tuple the DOWN builder wrote.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
            assert_eq!(tuple_element(payload, 2), named(&names, "process"));
            let target_term = tuple_element(payload, 3);
            assert!(is_tuple(target_term), "the target is the `{{Name, Node}}` pair");
            assert_eq!(tuple_size(target_term), 2);
            assert_eq!(tuple_element(target_term, 0), make_atom(name));
            assert_eq!(tuple_element(target_term, 1), make_atom(node));
            assert_eq!(tuple_element(payload, 4), make_small(7), "the death reason");
        }
        // The descriptor is consumed by the DOWN it produced.
        assert!(table.named_monitors.is_empty());
    }

    #[test]
    fn monitor_named_of_an_unregistered_name_is_born_down() {
        let (mut table, names) = table_and_names();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("nobody");
        let node = atoms_table.intern("nonode@nohost");
        let watcher = table.spawn_default().expect("watcher");
        let down_atom = table.atoms.down();

        assert_eq!(table.get(watcher).expect("watcher").mailbox().len(), 0);
        let reference = table
            .monitor_named(watcher, name, node, MonitorKind::Process)
            .expect("an unregistered name is not an error");
        let watcher_process = table.get(watcher).expect("watcher");
        assert_eq!(
            watcher_process.mailbox().len(),
            1,
            "the immediate noproc DOWN landed"
        );
        assert!(
            watcher_process.monitors().is_empty(),
            "an unregistered name records no monitor"
        );
        assert!(table.named_monitors.is_empty());
        let payload = watcher_process
            .mailbox()
            .peek()
            .expect("the DOWN is queued")
            .payload();
        assert!(payload_is_down_for(payload, reference, down_atom));
        // SAFETY: the payload is the live 5-tuple the DOWN builder wrote.
        unsafe {
            assert_eq!(tuple_size(payload), 5);
            assert_eq!(tuple_element(payload, 0), named(&names, "DOWN"));
            assert_eq!(tuple_element(payload, 2), named(&names, "process"));
            let target_term = tuple_element(payload, 3);
            assert_eq!(tuple_size(target_term), 2);
            assert_eq!(tuple_element(target_term, 0), make_atom(name));
            assert_eq!(tuple_element(target_term, 1), make_atom(node));
            assert_eq!(tuple_element(payload, 4), named(&names, "noproc"));
        }
    }

    #[test]
    fn monitor_term_routes_an_atom_target_to_the_named_path() {
        let (mut table, _names) = table_and_names();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let node = atoms_table.intern("nonode@nohost");
        let watcher = table.spawn_default().expect("watcher");
        let target = table.spawn_default().expect("target");
        table.register(name, target).expect("the name is free");

        // Without a local node installed, a name target is a typed unsupported — never a guessed
        // target, and never a recorded monitor.
        match table.monitor_term(watcher, make_atom(name), MonitorKind::Process) {
            Err(BeamError::Raise(raise)) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported: monitor/2: name target requires the local node (set_local_node)")
            ),
            other => panic!("expected a typed unsupported raise, got {other:?}"),
        }
        assert!(table.get(watcher).expect("watcher").mailbox().is_empty());

        // With the node installed it routes to `monitor_named`.
        assert_eq!(table.local_node(), None);
        table.set_local_node(node);
        assert_eq!(table.local_node(), Some(node));
        let reference = table
            .monitor_term(watcher, make_atom(name), MonitorKind::Process)
            .expect("the name resolves");
        assert_eq!(
            table
                .get(watcher)
                .expect("watcher")
                .monitors()
                .find(reference)
                .map(|m| m.target),
            Some(target)
        );

        // A non-pid, non-atom term is still `badarg`.
        assert!(matches!(
            table.monitor_term(watcher, make_small(1), MonitorKind::Process),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn terminate_of_an_unknown_pid_is_term_not_found() {
        let mut table = table();
        assert!(matches!(
            table.terminate(ProcessId::new(1, 0), make_small(0)),
            Err(BeamError::TermNotFound)
        ));
    }

    #[test]
    fn terminate_drops_the_name_registration() {
        let mut table = table();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let pid = table.spawn_default().expect("server");
        table.register(name, pid).expect("the name is free");
        assert_eq!(table.whereis(name), Some(pid));

        table.terminate(pid, make_small(1)).expect("live");
        assert!(!table.contains(pid));
        assert_eq!(
            table.whereis(name),
            None,
            "a name whose process is gone is gone"
        );
    }

    #[test]
    fn name_registry_rejects_duplicates_and_unknown_unregister() {
        let mut table = table();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("registered");
        let other = atoms_table.intern("unregistered");
        let pid = table.spawn_default().expect("a");
        let pid2 = table.spawn_default().expect("b");

        table.register(name, pid).expect("the name is free");
        assert!(matches!(table.register(name, pid2), Err(BeamError::BadArg)));
        // `erlang:register/2` of a pid that is not a live local process is `badarg`.
        assert!(matches!(
            table.register(other, ProcessId::new(999, 0)),
            Err(BeamError::BadArg)
        ));
        assert_eq!(table.whereis(name), Some(pid));
        assert_eq!(table.whereis(other), None);

        let registered: Vec<(Atom, ProcessId)> = table.registered().collect();
        assert_eq!(registered, vec![(name, pid)]);

        table.unregister(name).expect("it was registered");
        // `erlang:unregister/1` of an unregistered name is `badarg`, not `TermNotFound`.
        assert!(matches!(table.unregister(name), Err(BeamError::BadArg)));
        // A name registered to a dead process is not reported.
        table.register(name, pid2).expect("free again");
        table.remove(pid2);
        assert_eq!(table.registered().count(), 0);
    }

    #[test]
    fn register_rejects_a_dead_pid_and_name_of_reports_liveness() {
        let mut table = table();
        let mut atoms_table = AtomTable::new();
        let name = atoms_table.intern("server");
        let pid = table.spawn_default().expect("server");

        assert_eq!(table.name_of(pid), None, "not registered yet");
        table.register(name, pid).expect("the pid is live");
        assert_eq!(table.name_of(pid), Some(name));

        // A dead pid cannot be registered (nor keep a registration).
        let dead = {
            let dying = table.spawn_default().expect("dying");
            table.remove(dying);
            dying
        };
        assert!(matches!(
            table.register(atoms_table.intern("dead"), dead),
            Err(BeamError::BadArg)
        ));

        table.unregister(name).expect("it was registered");
        assert_eq!(table.name_of(pid), None, "an unregistered pid names nothing");

        // A stale registration left by a dead process reports nothing.
        table.register(name, pid).expect("free again");
        table.remove(pid);
        assert_eq!(
            table.name_of(pid),
            None,
            "a dead pid reports no name even if a registration lingers"
        );
    }

    #[test]
    fn is_process_alive_tracks_contains_and_exit_reason_reports_a_recorded_reason() {
        let mut table = table();
        let pid = table.spawn_default().expect("a");
        assert!(table.is_process_alive(pid));
        assert!(!table.is_process_alive(ProcessId::new(999, 0)));

        // A live process has no exit reason yet.
        assert_eq!(table.exit_reason(pid).expect("live"), None);
        table
            .get_mut(pid)
            .expect("live")
            .set_exit_reason(make_small(17));
        assert_eq!(
            table.exit_reason(pid).expect("live"),
            Some(make_small(17)),
            "the recorded reason is reported verbatim"
        );

        // Once the process is gone, the pid is unknown to the table.
        table.terminate(pid, make_small(17)).expect("live");
        assert!(!table.is_process_alive(pid));
        assert!(matches!(
            table.exit_reason(pid),
            Err(BeamError::TermNotFound)
        ));
        assert!(matches!(
            table.exit_reason(ProcessId::new(999, 0)),
            Err(BeamError::TermNotFound)
        ));
    }

    /// A **relocation-exempt** receiver whose nursery cannot take the apply-time
    /// `{'EXIT', …}` build still gets the signal: the message is built on a scratch arena and
    /// copied into a heap fragment the message owns, so nothing on the receiver's heap moves
    /// (no collection, no growth — a relocation would recycle the host's unpublished roots).
    #[test]
    fn an_exempt_receiver_gets_its_apply_time_signal_through_a_heap_fragment() {
        let mut table = table();
        let signaller = table.spawn_default().expect("a fresh process");
        let (receiver, filler_words) = saturated_receiver(&mut table);
        // A trapping receiver survives the exit and queues it as a message.
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_trap_exit(true);

        table
            .set_relocation_exempt(receiver, true)
            .expect("the receiver is live");
        let minor_before = table.get(receiver).expect("live").collector().minor_gcs();
        let base_before = table.get(receiver).expect("live").collector().young().base();

        // The reason is an immediate, so the enqueue-time copy allocates nothing and the signal
        // reaches the *apply* path, where the message build is the refused request.
        table
            .send_exit(signaller, receiver, make_small(42))
            .expect("the trapping receiver takes the signal");

        let r = table.get(receiver).expect("the receiver survived");
        assert_eq!(
            r.collector().minor_gcs(),
            minor_before,
            "an exempt receiver must not collect"
        );
        assert_eq!(
            r.collector().young().base(),
            base_before,
            "an exempt receiver's heap must not move"
        );
        assert_eq!(
            r.heap_used_words(),
            filler_words,
            "the fragment-built signal wrote nothing on the receiver's heap"
        );
        assert_eq!(r.mailbox().len(), 2, "the filler and the exit message");

        let filler_msg = table
            .get_mut(receiver)
            .expect("live")
            .receive(|_| true)
            .expect("the filler");
        assert!(
            filler_msg.fragments().is_none(),
            "the ordinary delivery still lives on the receiver's heap"
        );
        let exit_msg = table
            .get_mut(receiver)
            .expect("live")
            .receive(|_| true)
            .expect("the exit message");
        let fragments = exit_msg
            .fragments()
            .expect("the exempt fallback enqueued a fragment-resident message");
        assert_eq!(fragments.len(), 1, "one fragment carries the whole payload");
        assert!(
            fragments
                .iter()
                .next()
                .expect("one fragment")
                .contains(exit_msg.payload()),
            "the payload root lives inside the fragment"
        );
        // SAFETY: `payload` is the live `{'EXIT', Signaller, Reason}` tuple the fragment holds;
        // the three element slots are in bounds.
        let payload = exit_msg.payload();
        let (size, from, delivered) = unsafe {
            (
                tuple_size(payload),
                tuple_element(payload, 1),
                tuple_element(payload, 2),
            )
        };
        assert_eq!(size, 3, "the exit message is the three-tuple EXIT/From/Reason");
        assert_eq!(from, signaller.to_eterm(), "the signaller is the link/exit sender");
        assert_eq!(delivered, make_small(42), "the reason is delivered verbatim");
    }

    /// A gate that declines **every** relocation, recording the pids it was asked about — the
    /// minimal stand-in for the host's unscanned-root predicate.
    struct AlwaysDeclines {
        asked: Vec<ProcessId>,
    }

    impl CollectionGate for AlwaysDeclines {
        fn declines_relocation(&mut self, receiver: &Process) -> bool {
            self.asked.push(receiver.pid());
            true
        }
    }

    /// Spawns a receiver and drains its nursery to exactly **three free words** with a delivered
    /// filler message, returning the receiver and the filler's word count.
    ///
    /// Three free words is one short of the four-word `{'EXIT', From, Reason}` tuple (`1` header +
    /// `3` elements) — and of the `{'DOWN', …}` build's *first* allocation, its boxed four-word
    /// monitor reference — so an **apply-time build** is refused by capacity and, a refused request
    /// writing nothing, leaves the receiver's heap exactly as the fixture found it.
    fn saturated_receiver(table: &mut ProcessTable) -> (ProcessId, usize) {
        let receiver = table.spawn_default().expect("a fresh process");
        let capacity = table
            .get(receiver)
            .expect("the receiver is live")
            .collector()
            .young()
            .capacity_words();
        let filler_words = capacity - 3;
        let mut src = Heap::new(filler_words + 8).expect("the source arena allocates");
        let elems: Vec<Eterm> = (0..filler_words - 1)
            .map(|i| make_small(i as i64))
            .collect();
        let filler = src.tuple(&elems).expect("the filler fits the source arena");
        table
            .deliver_simple(receiver, ProcessId::new(999, 0).to_eterm(), filler)
            .expect("the filler fits the receiver's nursery");
        assert_eq!(
            table.get(receiver).expect("live").heap_used_words(),
            filler_words,
            "the fixture left exactly three words free"
        );
        (receiver, filler_words)
    }

    /// The gate seam's own surface: `has_collection_gate`/`take_collection_gate`, and
    /// `declines_signal_collection` answering `false` with no gate and for a pid the table does not
    /// hold (a receiver that died earlier in the propagation has no heap left to move).
    #[test]
    fn the_collection_gate_seam_installs_takes_and_answers_only_for_live_pids() {
        let mut table = table();
        let live = table.spawn_default().expect("a fresh process");
        let unknown = ProcessId::new(4095, 0);

        assert!(!table.has_collection_gate(), "a fresh table has no gate");
        assert!(
            !table.declines_signal_collection(live),
            "with no gate installed nothing declines"
        );

        table.set_collection_gate(Box::new(AlwaysDeclines { asked: Vec::new() }));
        assert!(table.has_collection_gate());
        assert!(
            table.declines_signal_collection(live),
            "the installed gate is consulted for a live receiver"
        );
        assert!(
            !table.declines_signal_collection(unknown),
            "the gate is never asked about a pid the table does not hold"
        );

        let taken = table.take_collection_gate().expect("the gate comes back out");
        assert!(!table.has_collection_gate());
        assert!(
            !table.declines_signal_collection(live),
            "taking the gate restores the no-gate behaviour"
        );
        drop(taken);
    }

    /// A **declining gate** — on a receiver that is neither relocation-exempt nor `Running` — routes
    /// the apply-time `{'EXIT', …}` build through a heap fragment: the message is delivered whole,
    /// with the receiver's nursery neither collected nor moved.
    ///
    /// This is the same controlled experiment
    /// [`an_exempt_receiver_gets_its_apply_time_signal_through_a_heap_fragment`] runs, with the
    /// **host gate** in place of the exemption flag: the crate's own gate answers `false` for this
    /// receiver, so the decline can only have come from the host.
    #[test]
    fn a_declining_gate_routes_the_apply_time_signal_through_a_fragment() {
        let mut table = table();
        let signaller = table.spawn_default().expect("a fresh process");
        let (receiver, filler_words) = saturated_receiver(&mut table);
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_trap_exit(true);
        let receiver_state = table.get(receiver).expect("live").state();
        assert!(
            !table.get(receiver).expect("live").is_relocation_exempt(),
            "the fixture's receiver is not exempt"
        );
        assert_ne!(
            receiver_state,
            ProcessState::Running,
            "and not `Running`, so the crate's own gate is false for it"
        );

        table.set_collection_gate(Box::new(AlwaysDeclines { asked: Vec::new() }));
        let minor_before = table.get(receiver).expect("live").collector().minor_gcs();
        let base_before = table.get(receiver).expect("live").collector().young().base();

        table
            .send_exit(signaller, receiver, make_small(42))
            .expect("the trapping receiver takes the signal");

        let r = table.get(receiver).expect("the receiver survived");
        assert_eq!(
            r.collector().minor_gcs(),
            minor_before,
            "a declined relocation must not collect"
        );
        assert_eq!(
            r.collector().young().base(),
            base_before,
            "a declined relocation must not move or grow the nursery"
        );
        assert_eq!(
            r.heap_used_words(),
            filler_words,
            "the fragment-built signal wrote nothing on the receiver's heap"
        );
        assert_eq!(
            r.mailbox().len(),
            2,
            "the notification is NOT lost: the filler and the exit message"
        );
        let exit_msg = {
            table
                .get_mut(receiver)
                .expect("live")
                .receive(|_| true)
                .expect("the filler");
            table
                .get_mut(receiver)
                .expect("live")
                .receive(|_| true)
                .expect("the exit message behind the filler")
        };
        let fragments = exit_msg
            .fragments()
            .expect("the declining gate's fallback enqueued a fragment-resident message");
        assert!(
            fragments
                .iter()
                .next()
                .expect("one fragment")
                .contains(exit_msg.payload()),
            "the payload root lives inside the fragment"
        );
        // SAFETY: `payload` is the live `{'EXIT', Signaller, Reason}` tuple the fragment holds;
        // elements 0..=2 are in bounds.
        let payload = exit_msg.payload();
        let (size, head, from, delivered) = unsafe {
            (
                tuple_size(payload),
                tuple_element(payload, 0),
                tuple_element(payload, 1),
                tuple_element(payload, 2),
            )
        };
        assert_eq!(size, 3, "the exit message is the three-tuple EXIT/From/Reason");
        assert_eq!(head, make_atom(table.atoms.exit()), "the head is the `'EXIT'` atom");
        assert_eq!(from, signaller.to_eterm(), "the signaller is the exit sender");
        assert_eq!(delivered, make_small(42), "the reason is delivered verbatim");
    }

    /// The **enqueue-time** half: with the gate declining, a reason *larger than the receiver's
    /// whole nursery* cannot be copied onto that heap, so the signal never reaches the signal queue
    /// and the `{'EXIT', …}` is built into a heap fragment straight from the enqueue site — with
    /// the nursery untouched and no queued signal left behind.
    #[test]
    fn a_declining_gate_refuses_the_enqueue_time_reason_copy_and_delivers_a_fragment() {
        let mut table = table();
        let signaller = table.spawn_default().expect("a fresh process");
        let (receiver, filler_words) = saturated_receiver(&mut table);
        table
            .get_mut(receiver)
            .expect("the receiver is live")
            .set_trap_exit(true);

        let capacity = table
            .get(receiver)
            .expect("live")
            .collector()
            .young()
            .capacity_words();
        let mut src = Heap::new(capacity + 8).expect("the source arena allocates");
        let elems: Vec<Eterm> = (0..capacity).map(|i| make_small(i as i64)).collect();
        let reason = src.tuple(&elems).expect("the oversized reason fits the arena");
        assert!(
            size_object(reason) > filler_words,
            "the fixture's premise: the reason cannot fit the receiver's free space"
        );

        table.set_collection_gate(Box::new(AlwaysDeclines { asked: Vec::new() }));
        let minor_before = table.get(receiver).expect("live").collector().minor_gcs();
        let base_before = table.get(receiver).expect("live").collector().young().base();

        table
            .send_exit(signaller, receiver, reason)
            .expect("the trapping receiver takes the signal");

        let r = table.get(receiver).expect("the receiver survived");
        assert_eq!(
            r.collector().minor_gcs(),
            minor_before,
            "a declined relocation must not collect"
        );
        assert_eq!(
            r.collector().young().base(),
            base_before,
            "a declined relocation must not move or grow the nursery"
        );
        assert_eq!(
            r.heap_used_words(),
            filler_words,
            "the refused reason copy and the fragment build wrote nothing on the receiver's heap"
        );
        assert_eq!(
            table.pending_signals(receiver).expect("live"),
            0,
            "the refused reason copy left no queued signal behind"
        );
        assert_eq!(r.mailbox().len(), 2, "the notification is NOT lost");
        let exit_msg = {
            table
                .get_mut(receiver)
                .expect("live")
                .receive(|_| true)
                .expect("the filler");
            table
                .get_mut(receiver)
                .expect("live")
                .receive(|_| true)
                .expect("the exit message behind the filler")
        };
        let fragments = exit_msg
            .fragments()
            .expect("the enqueue-time fallback enqueued a fragment-resident message");
        assert!(
            fragments
                .iter()
                .next()
                .expect("one fragment")
                .contains(exit_msg.payload()),
            "the payload root lives inside the fragment"
        );
        // SAFETY: `payload` is the live `{'EXIT', Signaller, Reason}` tuple the fragment holds;
        // elements 0..=2 are in bounds.
        let payload = exit_msg.payload();
        let (size, head, from, delivered) = unsafe {
            (
                tuple_size(payload),
                tuple_element(payload, 0),
                tuple_element(payload, 1),
                tuple_element(payload, 2),
            )
        };
        assert_eq!(size, 3, "the exit message is the three-tuple EXIT/From/Reason");
        assert_eq!(head, make_atom(table.atoms.exit()), "the head is the `'EXIT'` atom");
        assert_eq!(from, signaller.to_eterm(), "the signaller is the exit sender");
        assert!(
            compare::eq(delivered, reason, &AtomTable::new()),
            "the oversized reason is delivered whole (compared structurally: the copy lives in \
             the fragment, not on the receiver's heap)"
        );
    }

    /// The **delivery** half: [`ProcessTable::deliver`] consults the gate too. When it declines, the
    /// table marks the receiver relocation-exempt for the duration of exactly one
    /// [`Process::deliver`](crate::process::Process::deliver) call, so the copy engine's **raw**
    /// `AllocKind::Heap`/`AllocKind::Message` refusal (the caller's heap-fragment key) reaches the
    /// caller with the nursery untouched — and the receiver's own exemption flag is restored, so an
    /// ordinary receiver is left exactly as it was. Taking the gate away restores the ordinary
    /// collect-and-grow arm.
    #[test]
    fn a_declining_gate_makes_the_table_delivery_surface_the_raw_refusal() {
        let mut table = table();
        let (receiver, filler_words) = saturated_receiver(&mut table);
        // One word more than the three free words the fixture left.
        let mut src = Heap::new(16).expect("the source arena allocates");
        let payload = src
            .tuple(&[make_small(100), make_small(101), make_small(102)])
            .expect("the payload fits the arena");
        assert_eq!(size_object(payload), 4, "the payload is the refused four-word tier");
        let from = ProcessId::new(998, 0).to_eterm();

        table.set_collection_gate(Box::new(AlwaysDeclines { asked: Vec::new() }));
        let minor_before = table.get(receiver).expect("live").collector().minor_gcs();
        let base_before = table.get(receiver).expect("live").collector().young().base();

        match table.deliver_simple(receiver, from, payload) {
            Err(BeamError::Alloc(failure)) => assert!(
                matches!(failure.kind, AllocKind::Heap | AllocKind::Message),
                "the raw refusal keeps its Heap/Message kind, got {:?}",
                failure.kind
            ),
            other => panic!("expected the raw allocation refusal, got {other:?}"),
        }

        let r = table.get(receiver).expect("live");
        assert_eq!(
            r.collector().minor_gcs(),
            minor_before,
            "a declined delivery must not collect"
        );
        assert_eq!(
            r.collector().young().base(),
            base_before,
            "a declined delivery must not move or grow the nursery"
        );
        assert_eq!(
            r.heap_used_words(),
            filler_words,
            "the refused delivery wrote nothing on the receiver's heap"
        );
        assert_eq!(r.mailbox().len(), 1, "the refused delivery enqueued nothing");
        assert!(
            !r.is_relocation_exempt(),
            "the temporary exemption the gate implies is restored afterwards"
        );

        // The control: with the gate taken away the identical delivery takes the ordinary
        // collect-and-grow arm and lands on the receiver's (new) nursery.
        assert!(table.take_collection_gate().is_some());
        let src_off_heap = OffHeapTable::new();
        table
            .deliver(receiver, from, payload, &src_off_heap)
            .expect("the ordinary arm collects, grows and retries");
        let delivered = {
            let process = table.get_mut(receiver).expect("live");
            process.receive(|_| true).expect("the filler");
            let message = process.receive(|_| true).expect("the payload behind the filler");
            assert!(
                message.fragments().is_none(),
                "the non-declined arm builds on the receiver's own heap, never in a fragment"
            );
            message.payload()
        };
        let r = table.get(receiver).expect("live");
        assert!(
            r.collector().minor_gcs() > minor_before,
            "the non-declined arm collected (the relocation was not declined)"
        );
        assert!(
            r.collector().young().contains_term(delivered),
            "the retried build landed on the receiver's own (new) nursery"
        );
    }
}
