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

//! The send/deliver family: the runtime's own copy-into-the-receiver-heap path.
//!
//! Every delivery resolves its **source off-heap table** through one builder
//! ([`RuntimeContext::send_source`]) and routes a copy the receiver's heap cannot afford to
//! the heap-fragment fallback, so a genuine exhaustion is the documented `error:system_limit`
//! rather than a lost message.

use beam_common::{AllocKind, BeamError, Result};
use beam_message::{CopyContext, Message, OffHeapTable, Queue};
use beam_process::{MonitorRef, PortId, ProcessId, ProcessTable, RemotePid};
use beam_term::{atom_val, is_atom, Eterm};

use crate::error;
use crate::mailbox::MailboxState;
use crate::runtime::Runtime;

use super::alias;
use super::bridges::two_atom_tuple;
use super::gc_walk::process_roots_are_unscanned;
use super::runtime_context::RuntimeContext;

/// A `send` destination, classified by [`classify_send_destination`].
///
/// The one classification every `send` route shares — the compiled `Dest ! Msg`
/// instruction, the `-eval` evaluator's `!` operator and the `erlang:send/2`/`'!'/2`
/// BIF ([`SendBridge`](super::bridges::SendBridge)) — so the three can never drift.
/// The distinction is needed because the two *remote* arms must be transmitted through
/// [`Runtime::dist_send_message_pid`]/[`Runtime::dist_send_reg_send`], which take
/// `&mut Runtime`, so classifying (`&self`) and transmitting (`&mut self`) are separate
/// steps.
pub(super) enum SendDestination {
    /// A destination on **this** node: the internal pid term to deliver to, plus the alias
    /// reference to consume once the message has been delivered (`None` for every other
    /// destination).
    Local {
        /// The internal pid the delivery addresses.
        target: ProcessId,
        /// The `erlang:alias/1`/`monitor/3` reference to consume after a successful delivery.
        reference: Option<MonitorRef>,
    },
    /// An external (remote-node) pid: `{DOP_SEND_SENDER|DOP_SEND, From, To} -- Msg`.
    RemotePid {
        /// The external pid term as it was passed to `send` (it decodes with
        /// [`RemotePid::from_eterm`], which is how this arm was selected).
        to: Eterm,
    },
    /// A `{Name, Node}` atom pair: `{DOP_REG_SEND, From, Cookie, ToName} -- Msg`.
    RemoteName {
        /// The registered name on the peer.
        name: String,
        /// The peer's node name, as the caller spelled it (short or full).
        node: String,
    },
    /// A `{Name, Node}` pair whose `Node` names **this** node but under which **no** process is
    /// registered: the destination is accepted and the message is **dropped**, which is the C's
    /// `return 0` for exactly this case (`bif.c:2654` — the `dep == erts_this_dist_entry` arm
    /// looks `Name` up locally and reports success when the lookup finds nothing, without ever
    /// reaching `SEND_BADARG`). The send therefore reports the message term, after the ordinary
    /// `send` trace event.
    Dropped,
}
/// The C's `send` destination classification (`bif.c:2469-2674`), shared by every send route.
///
/// The order is the C's: an internal pid, a registered name, an alias reference, an external
/// pid, a port, then a `{Name, Node}` pair — and finally `badarg`. An **external pid is
/// boxed**, so none of the three local cases can match it, which is why their order relative
/// to it does not matter; what does matter is that the port and `{Name, Node}` arms come last
/// (their shapes are boxed too).
///
/// `table` is the process table local destinations are resolved against: the scheduler pass's
/// own table inside a slice, else [`Runtime`]'s own (the same choice
/// [`RuntimeContext::process_table`] and [`SendBridge`](super::bridges::SendBridge) make).
///
/// # The `{Name, Node}` pair: same node vs another node
///
/// A 2-tuple of atoms is the C's tuple arm (`bif.c:2614-2667`), and the C splits it by *which*
/// node it names — `erts_find_dist_entry(tp[2]) == erts_this_dist_entry`:
///
/// * `Node` naming **this** node (`node()`, and on a runtime with no `-name`/`-sname` that is
///   the fixed `nonode@nohost`, so the comparison is meaningful there too) is a **local named
///   send**: the C runs the very same `erts_whereis_name_to_id` the atom-destination arm uses
///   (`bif.c:2636`), delivers to the process it finds, and — when it finds none — reports
///   success without delivering (`bif.c:2654`'s `return 0`), *not* `SEND_BADARG`. Oracle
///   measurements on the system `erl` (OTP 29.1.1 / erts-17.1) pin both halves:
///   `register(foo, self()), {foo, node()} ! hi` delivers `hi` on a non-distributed node (and on
///   a `-sname` node), while `{noreg, node()} ! hi` and `erlang:send({noreg, node()}, hi)` both
///   answer `hi` with nothing delivered.
/// * Any other node is the remote arm: `RemoteName` when this runtime **is** a node, and
///   otherwise the typed capability gap it has always been (never a silent drop). The oracle
///   accepts the tuple whatever the node atom is — `{foo, 'xyz@abc'} ! hi` answers `hi`,
///   dropping the message — so the gap is this runtime's honest signal, not the oracle's error.
///
/// # Errors
///
/// [`BeamError::BadArg`] for the destinations the oracle rejects with `badarg`; a typed
/// capability gap ([`crate::error::unsupported`]) for the ones it accepts but this runtime
/// cannot serve yet — a port destination, and a `{Name, Node}` pair naming **another** node on
/// a runtime that is **not** a distributed node (`dist_client()` is `None`, which is the
/// message the non-distributed corpus has always pinned).
pub(super) fn classify_send_destination(
    runtime: &Runtime,
    table: &ProcessTable,
    to: Eterm,
) -> Result<SendDestination> {
    if ProcessId::is_process(to) {
        return Ok(SendDestination::Local {
            target: ProcessId::from_eterm(to)?,
            reference: None,
        });
    }
    if is_atom(to) {
        return match table.whereis(atom_val(to)) {
            Some(pid) => Ok(SendDestination::Local {
                target: pid,
                reference: None,
            }),
            // An atom name no process is registered under: the oracle's only failing
            // (correctly typed) destination (`bif.c:2536`).
            None => Err(BeamError::BadArg),
        };
    }
    if let Some((owner, reference)) = alias::alias_destination(runtime, table, to) {
        // A live **alias**: the message goes to its owner and the reference is consumed after a
        // successful delivery (the C's `send_altact_message` arm, `bif.c:2478-2492`).
        return Ok(SendDestination::Local {
            target: owner,
            reference: Some(reference),
        });
    }
    if RemotePid::from_eterm(to).is_ok() {
        // A pid on another node (`bif.c:2614-2667`'s sibling arm): the message travels as a
        // `DOP_SEND[_SENDER]` control message on the link to that node.
        return Ok(SendDestination::RemotePid { to });
    }
    if PortId::from_eterm(to).is_ok() {
        return Err(error::unsupported(
            "erlang:send/2 to a port: the port write path is not wired",
        ));
    }
    if let Some((name_atom, node_atom, name, node)) = two_atom_tuple(runtime.code_server.atoms(), to)
    {
        if node_atom == runtime.local_node() {
            // `Node` names this runtime's own node: the C's `dep == erts_this_dist_entry` arm
            // (`bif.c:2628-2655`) — a plain local `whereis(Name)`, no distribution involved.
            // `node()` is this atom by construction (`erlang:node/0` is served from
            // `Runtime::local_node_term`), and the identity is the *full* `node@host` name the
            // node advertises, so the comparison is the C's own by-name `erts_find_dist_entry`.
            return match table.whereis(name_atom) {
                Some(pid) => Ok(SendDestination::Local {
                    target: pid,
                    reference: None,
                }),
                // Nothing registered under that name on this node: the C reports success and
                // drops the message (`bif.c:2654`).
                None => Ok(SendDestination::Dropped),
            };
        }
        if runtime.dist_client().is_none() {
            // Another node, and not a distributed node: no link table exists, so this stays the
            // exact capability gap it has always been -- which keeps a non-node VM's observable
            // behaviour (and the differential corpus) byte-identical.
            return Err(error::unsupported(&format!(
                "erlang:send/2 to {name}@{node}: distributed send is not implemented"
            )));
        }
        return Ok(SendDestination::RemoteName { name, node });
    }
    Err(BeamError::BadArg)
}

impl<'a> RuntimeContext<'a> {
    /// The **source** off-heap table a delivery resolves `message`'s refc binaries against.
    ///
    /// A refc binary is resolved **by its payload data pointer** (`OffHeapTable::binary_by_data_ptr`)
    /// through this table, so a payload whose handle is missing makes the copy **flatten** the
    /// window onto the destination heap (`beam_message`'s `copy_refc_binary` last resort) instead
    /// of **sharing** it. The handles one delivery may be asked to resolve come from **three**
    /// families, and only a union of all of them is correct:
    ///
    /// * `extra` — the sending **machine's** own handles
    ///   ([`Machine::refc_binaries`](beam_engine::Machine::refc_binaries)), handed over by the
    ///   engine's `send` seam;
    /// * the runtime's **escaped-payload** store (`Runtime::eval_off_heap`), where a payload built
    ///   by a nested run whose machine has since been dropped was adopted
    ///   (`Runtime::adopt_escaped_binaries`);
    /// * when `sender` is named, the off-heap families that process's **mailbox scan retained**
    ///   from the messages it received and removed ([`MailboxState`]) — so a term a process
    ///   **received** and then **forwards** (`erlang:send/2`) still resolves its payload, which no
    ///   machine of the forwarding process owns.
    ///
    /// `Runtime::source_payloads` already unions all three families; `sender = None` is the
    /// pass-boundary form (see below). The **node** family is carried by `SourcePayloads` but
    /// deliberately **not** pushed: the copier registers a fresh `NodeEntry` for every external
    /// thing it copies and never reads a source node list, while `OffHeapTable`'s `Drop` releases
    /// every entry it holds — pushing the retained handles would zero the objects' own counts (the
    /// same rule `Runtime::stabilize_term_with` documents).
    ///
    /// `None` — no handle in any family (a plain on-heap message) — is the documented empty-source
    /// fast path, so the overwhelmingly common delivery builds no table at all.
    ///
    /// # Why `sender` is an `Option`
    ///
    /// [`Self::deliver_message`] is the **no-off-heap** delivery every runtime-internal delivery
    /// uses (`trace.rs`, `native/process/exit.rs`) *and* the one [`Self::deliver_in_table`] drives
    /// at a scheduler-pass boundary — where the throwaway context is built `for_process` the
    /// **receiver**, so `self.mailbox_pid` names the receiver, never a sender. A delivery with no
    /// sending process (`None`) must therefore resolve against the runtime's escaped store and
    /// `extra` alone (`Runtime::off_heap_source`), never against the *receiver's* retained
    /// families. The union only applies where a genuine sender is named
    /// ([`Self::send_message_with_off_heap`]).
    ///
    /// # The sending machine's in-flight handles are **not** reachable from this node
    ///
    /// The engine's `send` seam passes its machine's handles as `extra`
    /// (`ExecutionContext::send_message_with_off_heap`), but the **`erlang:send/2`/`'!'/2` BIF**
    /// route (`SendBridge`) reaches this method through `ExecutionContext::send_message`, whose
    /// signature carries **no** handle parameter — and it cannot carry one: the in-flight
    /// `Machine` is removed from `RunnerState.engines` for the whole slice
    /// (`runner/run.rs:169`), `BudgetContext` forwards only `&mut RuntimeContext`
    /// (`runner/budget.rs:169-174`), and `Machine::refc_binaries` is a plain `Vec` field
    /// (`beam_engine/src/machine/mod.rs:241`) no context can observe mid-run. A compiled `A ! B`
    /// therefore carries its machine's handles, while the BIF route can only resolve what
    /// `source_payloads` holds: a term the *running* machine just built with an off-heap payload
    /// and forwards from a BIF callee still **flattens**. Closing that needs a sibling change (a
    /// thread of the sender's handles into `ExecutionContext::call_bif`/`SendHook::send`), not a
    /// beam_rt-only one — see the node `CONTEXT.md`.
    ///
    /// The returned table is **owned** by the caller and must outlive the copy the delivery runs
    /// (the [`CopyContext`] borrows it as its source; the **destination** side is always the
    /// receiver's own table, never this one).
    pub(super) fn send_source(
        &self,
        sender: Option<ProcessId>,
        extra: &[beam_alloc::RefcBinary],
    ) -> Option<OffHeapTable> {
        // No sending process: the pass-boundary/runtime-internal form resolves against the
        // runtime's escaped store and `extra` alone — exactly the table this path always built.
        let Some(pid) = sender else {
            return self.runtime.off_heap_source(extra);
        };
        let payloads = self.runtime.source_payloads(pid, extra);
        if payloads.is_empty() {
            return None;
        }
        // One clone (refcount++) per handle into the copy's source table; the table drops at the
        // end of the delivery, giving every reference back. The MSO handles are pushed **in
        // order**, so each lands on the slot its container term names.
        let mut source = OffHeapTable::new();
        for handle in &payloads.binaries {
            source.push_binary(handle.clone());
        }
        for handle in &payloads.off_heap {
            source.push(handle.clone());
        }
        Some(source)
    }

    /// Delivers `message` from `from` into the mailbox of the internal pid `to`, with **no**
    /// trace event emitted — the delivery half of [`ExecutionContext::send_message`], and the
    /// delivery a trace event itself uses (so emitting an event never emits an event).
    ///
    /// This is the **no-off-heap** form: it hands [`Self::deliver_message_with_off_heap`] the
    /// runtime-escaped-store source table ([`Self::send_source`] with **no** sending process), so
    /// every runtime-internal delivery (`trace.rs`, `native/process/exit.rs`,
    /// [`Self::deliver_in_table`]) keeps its documented semantics. The engine's `send` seam carries
    /// the sender machine's refc-binary handles and goes through the `_with_off_heap` form
    /// instead.
    ///
    /// A delivery pushes a woken recipient onto the table's wake list, so the wake survives for
    /// the scheduler pass; the outcome itself needs no action here.
    pub(crate) fn deliver_message(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<()> {
        // The sender passed no handles of its own, but `message` may still carry an off-heap
        // payload that **escaped** the nested run which built it: that payload's handle lives
        // in the runtime's escaped-payload store, and the receiver-side copy resolves a refc
        // binary by its data pointer through the source table it is handed. The context's own pid
        // is the **receiver** on the pass-boundary path ([`Self::deliver_in_table`]), so the
        // receiver's retained families are deliberately left out (`None`); `None` (no handle in
        // any family) is the documented empty-source fast path.
        let source = self.send_source(None, &[]);
        match &source {
            Some(source) => self.deliver_message_with_off_heap(to, from, message, source),
            None => {
                let empty = OffHeapTable::new();
                self.deliver_message_with_off_heap(to, from, message, &empty)
            }
        }
    }

    /// [`Self::deliver_message`] for a sender that holds the term's **source off-heap table**
    /// — the payload handles the receiver-side copy resolves a refc binary by
    /// (`beam_message`'s `copy_refc_binary` looks a payload up by its data pointer through the
    /// copy's source table).
    ///
    /// `src` is the sender's own table: for the engine's `send` seam it is the in-flight
    /// machine's refc-binary handles, so `self() ! <<0:(1200)/integer>>` — an engine-built
    /// off-heap binary — is copied into the receiver instead of being refused with
    /// [`BeamError::BadArg`]. A sender with no off-heap payload (every runtime-internal
    /// delivery, and a plain on-heap message) passes an **empty** table, exactly as
    /// [`Self::deliver_message`] always did.
    ///
    /// # Ownership
    ///
    /// `src` is **borrowed for the whole call** — the [`CopyContext`] the receiver's copy runs
    /// under resolves payloads through it for the copy's duration — so the caller must keep it
    /// alive until this returns. The **destination** side is the receiver's own off-heap table
    /// (`ProcessTable::deliver` → `Process::deliver` → `beam_message::send` drains it into the
    /// queued [`Message`]), or the fragment's own table on the fallback, so — unlike the
    /// process-return path — **no** runtime-level destination table
    /// ([`Runtime::eval_off_heap`](crate::runtime::Runtime)) is needed. A handle must never be
    /// reconstructed from a raw payload data pointer.
    ///
    /// # Ordering (the oversized-message retry)
    ///
    /// The retry is triggered by the **raw** [`AllocKind::Message`] failure `deliver` returns:
    /// it means *the payload did not fit the receiver's heap* and is *not* fatal. The
    /// translation to `error:system_limit` is therefore applied only to what remains **after**
    /// the retry has run (the retry's own failure, and the non-`Message` arm) — translating the
    /// first result would turn the retry trigger into a hard error and break every oversized
    /// delivery.
    pub(crate) fn deliver_message_with_off_heap(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
        src: &OffHeapTable,
    ) -> Result<()> {
        // A delivery may **collect on the receiver's heap** — `Process::deliver`'s
        // `ensure_capacity` refusal path drives a rooted collection — and that collection
        // relocates against the receiver's *published* root snapshot
        // ([`Process::registers`](beam_process::Process::registers)), not against the live
        // machine. For the process this context belongs to that snapshot is **stale by up to
        // one slice of instructions**, because a delivery is driven from the scheduler pump
        // *while* the owning machine is mid-slice: the machine's x registers/the Erlang stack
        // hold young terms the snapshot cannot see, so collecting here frees young words the
        // running machine still points at (the `Target::Foreign` relocation hazard
        // `crate::context::gc` documents).
        //
        // Exempting that process for the duration of the delivery makes `Process::deliver`
        // surface its raw refusal instead of collecting (see
        // [`Process::set_relocation_exempt`](beam_process::Process::set_relocation_exempt)), so
        // no heap word moves under a term the unpublished machine still holds. The message
        // still arrives: the refusal is exactly the signal the heap-fragment fallback below is
        // reached on.
        //
        // The **receiver** gets the same treatment when *its* own collection would be unsound —
        // see [`Self::deliver_message_with_off_heap_inner`] and
        // [`Self::delivery_collection_is_unsound`], which the body below consults and then marks
        // the receiver through [`Self::with_pid_relocation_exempt`].
        self.with_relocation_exempt(|this| {
            this.deliver_message_with_off_heap_inner(to, from, message, src)
        })
    }

    /// The delivery body of [`Self::deliver_message_with_off_heap`], run with the sending
    /// context's own process marked relocation-exempt (see that method for why).
    ///
    /// # The receiver's own collection, and the runtime's unscanned-root valve
    ///
    /// Exempting the **sender** covers only the sender. [`Process::deliver`] can *also* collect
    /// the **receiver's** heap: a receiver that is neither `Running` nor relocation-exempt
    /// passes `has_unpublished_roots`, so `deliver` runs a rooted collection (and an on-demand
    /// growth) over *its* root set — the `Runnable` shape, a process woken by a delivery or
    /// pre-empted at a yield. (A `Waiting` receiver is exempt for its whole parked window — the
    /// runner's Wait park arm sets the mark and the resume clears it — so it refuses the move.)
    /// The runtime's four admission doors
    /// (`gc::{ensure_heap, ensure_heap_full, alloc_words_rooted}` and `Runtime::serve_gc_request`)
    /// refuse exactly that move when the receiver's roots reach a container the collector does
    /// not scan ([`crate::context::gc_walk`]); the delivery-driven site inside
    /// `beam_process::Process::deliver` consults none of them, so a delivery into such a receiver
    /// used to move its heap against a root set the runtime itself calls unsound — a live term
    /// reachable only through a container outside the collector's from-set is left behind, the
    /// old mature block is freed and recycled in place, and the process reads a corrupted word at
    /// a stable address.
    ///
    /// [`Self::delivery_collection_is_unsound`] asks that same question for the receiver, and
    /// when the answer is `true` this method marks the **receiver**
    /// ([`Self::with_pid_relocation_exempt`]) for the duration of the delivery. `Process::deliver`
    /// then returns its raw [`AllocKind::Message`] refusal **unchanged** — no collection, no
    /// growth, no word moved — and the ordinary host fragment fallback below
    /// ([`Self::send_fragmented_message`]) delivers the payload with no heap word moved, exactly
    /// as it already does for a `Running` receiver. This is the runtime applying its *own* policy
    /// consistently, not a special case: "do not relocate against a root set that reaches an
    /// unscannable container".
    ///
    /// The receiver's mark is restored on every path, so a receiver the scheduler (or an outer
    /// scope) already had exempt keeps its mark.
    pub(super) fn deliver_message_with_off_heap_inner(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
        src: &OffHeapTable,
    ) -> Result<()> {
        if !self.delivery_collection_is_unsound(to) {
            return self.deliver_into_mailbox(to, from, message, src);
        }
        // The receiver's own heap must not be relocated for this delivery: exempt it, so
        // `deliver` refuses the move and the fragment fallback delivers the message.
        self.with_pid_relocation_exempt(to, |this| {
            this.deliver_into_mailbox(to, from, message, src)
        })
    }

    /// One delivery attempt into `to`'s mailbox, with the heap-fragment fallback for the
    /// copier's raw [`AllocKind::Message`] refusal and the `error:system_limit` translation —
    /// the shared body of [`Self::deliver_message_with_off_heap_inner`].
    pub(super) fn deliver_into_mailbox(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
        src: &OffHeapTable,
    ) -> Result<()> {
        match self.process_table_mut().deliver(to, from, message, src) {
            Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => self
                .send_fragmented_message(to, from, message, src)
                .map_err(error::enforce_term_heap_limit),
            other => other.map(|_| ()).map_err(error::enforce_term_heap_limit),
        }
    }

    /// Whether a delivery into `to` could drive an **unsound** collection of `to`'s own heap —
    /// i.e. whether the delivery must run with `to` marked relocation-exempt (see
    /// [`Self::deliver_message_with_off_heap_inner`]).
    ///
    /// The answer is the runtime's **unscanned-root valve**, asked about the *receiver*:
    ///
    /// * A receiver that already refuses the delivery-driven collection needs no mark:
    ///   [`Process::has_unpublished_roots`](beam_process::Process::has_unpublished_roots) is true
    ///   for a relocation-exempt process and for a `Running` one (whose `regs` snapshot is stale
    ///   mid-slice), and `deliver` returns its raw refusal for both — which is the very signal the
    ///   fragment fallback keys on. Answering `false` here therefore changes nothing.
    /// * Otherwise the receiver is a parked process whose roots `deliver` would publish — the
    ///   shape the four admission doors decline. The pid's **descent latch**
    ///   ([`Runtime::foreign_reachable`](crate::runtime::Runtime)) is the `O(1)` fast path: when it
    ///   is open the process's reachable graph may hold a container from a region its collector
    ///   does not scan at depth ≥ 2, and declining is the conservative (and cheap) answer — the
    ///   `O(live objects)` descent is deliberately **not** paid on the per-send hot path.
    /// * When the latch is closed the descent would be skipped anyway, so the walk's always-run
    ///   per-word phase is the whole valve — the same classification the four doors run on the
    ///   same condition ([`crate::context::gc::root_is_unscanned`]). Run it (it is `O(roots)`) and,
    ///   on a hit, open the latch permanently, exactly as those doors do: a foreign container
    ///   really did reach a live root, which is the event the latch records.
    ///
    /// A pid with no process answers `false`: there is no heap to move, and `deliver` reports the
    /// typed not-found itself.
    ///
    /// Nothing here can *allow* a collection the current behaviour would decline — a `false`
    /// answer is returned only for a receiver that is already refusing, one whose roots are all
    /// in scanned regions, or one no table holds — so the change is strictly conservative.
    pub(super) fn delivery_collection_is_unsound(&mut self, to: ProcessId) -> bool {
        match self.process_table().get(to) {
            // Already refusing the collection: `deliver` will not move a word, so no mark is
            // needed (and the fragment fallback already covers the refusal).
            Some(process) if process.has_unpublished_roots() => return false,
            Some(_) => {}
            None => return false,
        }
        // The `O(1)` fast path: an open descent latch means the graph may hold a foreign
        // container at depth ≥ 2, so decline rather than pay the `O(live objects)` walk.
        if self.runtime.foreign_reachable(to) {
            return true;
        }
        // The latch is closed, so the descent is skipped: the always-run per-word phase *is* the
        // valve here. The scratch is moved out for the walk (`O(1)`) because the receiver is
        // reached through a table that may be the runtime's own.
        let mut scratch = self.runtime.take_walk_scratch();
        let hit = match self.process_table().get(to) {
            Some(process) => process_roots_are_unscanned(&mut scratch, process, false),
            None => false,
        };
        self.runtime.put_walk_scratch(scratch);
        if hit {
            self.runtime.note_foreign_reachable(to);
        }
        hit
    }

    /// [`Self::deliver_message`] for a caller that already holds the runtime and the
    /// destination's process table **separately** — the runtime's own delivery steps
    /// ([`Runtime::service_file_requests`](crate::runtime::Runtime),
    /// [`Runtime::deliver_port_messages`](crate::runtime::Runtime),
    /// [`Runtime::fire_due_timers`](crate::runtime::Runtime),
    /// [`Runtime::drain_ets_owner_exits`](crate::runtime::Runtime)), which run at a
    /// scheduler-pass boundary where the table has been moved out of the runtime.
    ///
    /// It carries the **same** delivery semantics — the plain receiver-heap copy, the
    /// heap-fragment fallback on the copier's raw [`AllocKind::Message`] refusal, and the
    /// `error:system_limit` translation — so every message a process receives arrives through
    /// one implementation. It matters most for a **relocation-exempt** receiver
    /// ([`Self::with_relocation_exempt`]): such a process refuses the collection an exhausted
    /// delivery would otherwise run, so without the fallback that refusal would fail the whole
    /// scheduler pass instead of delivering the message.
    ///
    /// The throwaway context exists only to reach that one method: `to` is the destination, so
    /// it addresses exactly the mailbox being delivered to and never pumps the scheduler
    /// ([`RuntimeContext::for_process`]).
    ///
    /// # Errors
    ///
    /// As [`Self::deliver_message`]: the table's typed `TermNotFound` for a pid no process
    /// exists for, and the fragment fallback's own failure translated to `error:system_limit`.
    pub(crate) fn deliver_in_table(
        runtime: &'a mut Runtime,
        table: &'a mut ProcessTable,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<()> {
        RuntimeContext::for_process(runtime, table, to).deliver_message(to, from, message)
    }

    /// Delivers `message` into `to`'s mailbox through the **heap-fragment** model — the
    /// sender half of the C `erts_try_alloc_message_on_heap`/`new_message_buffer` path
    /// (`erl_message.c:622-691`), reached when the receiver's own heap cannot hold the
    /// payload (`../../erts/emulator/beam/erl_message.c`).
    ///
    /// The work is [`beam_message::send_fragmented`]`(from, message, heap, mailbox, ctx, 0)`
    /// driven straight into the receiver's own [`Process`](beam_process::Process) through
    /// [`heap_and_mailbox_mut`](beam_process::Process::heap_and_mailbox_mut) — the split
    /// borrow that hands out the receiver's young **heap** and its **mailbox** at the same
    /// time. The payload is copied into a fresh
    /// [`HeapFragment`](beam_message::HeapFragment) sized to hold all of it (the `0`
    /// capacity hint means "at least the whole payload"), the fragment is pushed onto a
    /// [`Fragments`](beam_message::Fragments) chain, and the enqueued
    /// [`Message`](beam_message::Message) owns that chain, so its payload has a stable home
    /// the receiver's heap is not involved in. The off-heap objects the payload references
    /// are refcounted into the **fragment's** table through the preflight's **source** table
    /// `src` — the sender's own handles
    /// ([`Self::deliver_message_with_off_heap`]'s) — so an engine-built off-heap binary in an
    /// oversized payload is resolved exactly as on the ordinary path.
    ///
    /// # The redundant preflight, and why it is harmless
    ///
    /// `send_fragmented` first retries the ordinary receiver-heap copy and falls back to a
    /// fragment only when *that* reports [`AllocKind::Message`]. This method is reached
    /// from exactly that failure, so the retry fails identically — and the retry is
    /// **side-effect free**: [`beam_message::copy_struct`] preflights the destination's
    /// capacity and refuses a graph it cannot hold *before* writing a word, so the
    /// receiver's heap is left byte for byte as it was (the composed body this replaced
    /// skipped the retry and went straight to a fragment).
    ///
    /// The recipient is woken exactly as the ordinary path wakes it, except that the wake
    /// cannot go through the process table's wake list (that list is filled by
    /// `ProcessTable::deliver`, which this path deliberately bypasses): it is recorded on
    /// [`Runtime::note_fragment_wake`] instead, and
    /// [`Runtime::scheduler_pass`](crate::runtime::Runtime) absorbs it into the real run
    /// queue at the same points it absorbs the table's own wake list.
    ///
    /// # Errors
    ///
    /// The failures are **raw**: the fragment's host-allocation failure and the copy
    /// engine's exhaustion ([`AllocKind::Message`]). The single caller
    /// ([`Self::deliver_message_with_off_heap`]) translates a genuine exhaustion to
    /// `error:system_limit` with [`error::enforce_term_heap_limit`]; that translation lives
    /// at the call site because the `Message` kind must stay recognizable as the retry
    /// trigger on the first `deliver` attempt.
    pub(super) fn send_fragmented_message(
        &mut self,
        to: ProcessId,
        from: Eterm,
        message: Eterm,
        src: &OffHeapTable,
    ) -> Result<()> {
        // `src` is the sender's own source off-heap table (the engine seam's machine handles
        // on the `send` path), borrowed for the whole call: the preflight resolves a refc
        // binary's payload through it exactly as the ordinary delivery did.
        // A fresh destination off-heap table: the fragment path gives the payload to the
        // **fragment's** own table (see `beam_message::send_fragmented`), so this one is
        // only the preflight's — which cannot succeed here — and stays empty.
        let mut dst = OffHeapTable::new();
        let mut ctx = CopyContext::new(src, &mut dst);
        let process = self
            .process_table_mut()
            .get_mut(to)
            .ok_or(BeamError::TermNotFound)?;
        let (heap, mailbox) = process.heap_and_mailbox_mut();
        beam_message::send_fragmented(from, message, heap, mailbox, &mut ctx, 0)?;
        let woken = process.wake();
        // The payload now lives in a [`HeapFragment`](beam_message::HeapFragment) — a region the
        // receiver's collector does not scan — so the receiver's **descent gate** opens here:
        // `receive` hands that payload to the running code as a register value, and the code can
        // nest it inside a container built on the receiver's *own* scanned heap, which is the
        // shape only the gated descent finds. Recorded before the wake so a receiver that was
        // already runnable (the wake's `false` arm) is gated too. A delivery that did **not**
        // need a fragment — the ordinary receiver-heap copy — lands on the receiver's own heap
        // and deliberately does not open the gate (see [`crate::context::gc_walk`]).
        self.runtime.note_foreign_reachable(to);
        if woken {
            self.runtime.note_fragment_wake(to);
        }
        Ok(())
    }

    /// The running process's mailbox **message queue** and **scan state**, borrowed
    /// together.
    ///
    /// The queue lives in the `Process` (in the caller-supplied table or the runtime's
    /// own); the scan state lives in [`Runtime::mailboxes`](crate::runtime::Runtime).
    /// They are disjoint, so both are handed out mutably at once. A pid with no process
    /// is the typed [`crate::error::no_mailbox`] capability gap.
    pub(super) fn mailbox_parts(&mut self) -> Result<(&mut Queue, &mut MailboxState)> {
        let pid = self.mailbox_pid;
        let Runtime {
            processes,
            mailboxes,
            ..
        } = &mut *self.runtime;
        // `table` and `mailboxes`/`processes` are disjoint places, so the borrows below
        // do not conflict.
        let table: &mut ProcessTable = match self.mailbox_table {
            Some(ref mut table) => &mut **table,
            None => processes,
        };
        // A pid with no process is the capability gap *before* any scan state is
        // created for it, so a ghost pid never accumulates a stale entry.
        let process = table.get_mut(pid).ok_or_else(|| error::no_mailbox(pid))?;
        let state = mailboxes.entry(pid).or_default();
        Ok((process.mailbox_mut(), state))
    }

    /// The payloads of the running process's mailbox, in FIFO order.
    ///
    /// This is the receive-seam **scan of the whole mailbox** the `-eval` evaluator needs
    /// (the engine's compiled `receive` walks the mailbox itself through
    /// `receive_next`/`receive_skip`, so it never needs this). The messages are *not*
    /// removed: [`RuntimeContext::remove_mailbox_message`] does that with the BEAM's
    /// `remove_message` ordering.
    ///
    /// A pid with no process is the typed [`crate::error::no_mailbox`] capability gap.
    pub(crate) fn mailbox_messages(&mut self) -> Result<Vec<Eterm>> {
        let (queue, _state) = self.mailbox_parts()?;
        Ok(queue.iter().map(Message::payload).collect())
    }

    /// Removes the message at `index` from the running process's mailbox and repositions
    /// the scan at the first message.
    ///
    /// `index` is a position in the FIFO order [`RuntimeContext::mailbox_messages`]
    /// reports; an index past the end removes nothing (the same safe no-op the queue's own
    /// `remove_at` implements for a past-the-end cursor).
    ///
    /// The removal goes through [`MailboxState::remove_at`], so a message whose payload
    /// lives in a heap fragment keeps it alive (`MailboxState::remove` explains why).
    pub(crate) fn remove_mailbox_message(&mut self, index: usize) -> Result<()> {
        let (queue, state) = self.mailbox_parts()?;
        let cursor = queue.cursor_at(index);
        state.remove_at(queue, cursor);
        state.reset(queue);
        Ok(())
    }

    /// The internal pid a `send` destination names, plus the **alias reference to consume**
    /// once the message has been delivered.
    ///
    /// A pid is passed through; anything else is tried as a reference **alias**
    /// ([`alias::alias_destination`], the C's `send_altact_message` arm,
    /// `../../erts/emulator/beam/bif.c:2478-2492`), which resolves to the alias **owner** and
    /// returns the reference so the caller can consume it after a successful delivery —
    /// `gen_server:reply/2`'s `Alias ! {Tag, Reply}` (`../lib/stdlib/src/gen_server.erl:549-553`)
    /// is exactly that compiled `send`, and without this a reply could never reach an alias.
    ///
    /// The lookup is read-only (a `&self` borrow of the process table the context resolves
    /// against — the pass table in a scheduler slice, else the runtime's own), so the caller
    /// can deliver with `&mut self` afterwards.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for everything that is neither an internal pid nor a live alias
    /// reference — the destinations the oracle rejects with `badarg` (see the [`alias`] module's
    /// divergence note for a reference that is not a live alias).
    ///
    /// `#[cfg(test)]`: the four in-module `tests.rs` cases are this form's only callers. The
    /// three send seams all need the **remote** arms too, so they resolve through
    /// [`RuntimeContext::resolve_send_destination`] and transmit with
    /// [`RuntimeContext::deliver_remote`].
    #[cfg(test)]
    pub(super) fn resolve_send_target(&self, to: Eterm) -> Result<(ProcessId, Option<MonitorRef>)> {
        match self.resolve_send_destination(to)? {
            SendDestination::Local { target, reference } => Ok((target, reference)),
            // A remote (or dropped) destination has no local `ProcessId`, which is all this form
            // can express: the in-module callers pin exactly the local contract. The send seams
            // reach the remote arms through [`RuntimeContext::resolve_send_destination`]
            // directly.
            SendDestination::RemotePid { .. }
            | SendDestination::RemoteName { .. }
            | SendDestination::Dropped => Err(BeamError::BadArg),
        }
    }

    /// The full [`SendDestination`] a `send` destination term names — the shared
    /// classification ([`classify_send_destination`]) every send route runs.
    ///
    /// Unlike the (test-only) `resolve_send_target` form this keeps the two **remote** arms, so
    /// the compiled `Dest ! Msg`, the `-eval` `!` and the `erlang:send/2` BIF can all transmit a
    /// remote destination through [`RuntimeContext::deliver_remote`]. The lookup is read-only (a
    /// `&self` borrow of the table the context resolves against), so the caller can transmit with
    /// `&mut self` afterwards.
    ///
    /// # Errors
    ///
    /// As [`classify_send_destination`]: [`BeamError::BadArg`] for a destination the oracle
    /// rejects, and a typed capability gap for the accepted-but-unsupported ones.
    pub(super) fn resolve_send_destination(&self, to: Eterm) -> Result<SendDestination> {
        classify_send_destination(&*self.runtime, self.process_table(), to)
    }

    /// Transmits `message` to a **remote** destination (contract A.5, Phase 2).
    ///
    /// The two remote arms mirror C's `remote_send` (`bif.c:2381-2456`): an external pid goes on
    /// the link to *its* node as `DOP_SEND[_SENDER]`, a `{Name, Node}` pair as `DOP_REG_SEND`
    /// addressed to the peer's registered name. This is the same pair of
    /// [`Runtime::dist_send_message_pid`]/[`Runtime::dist_send_reg_send`] calls the
    /// `erlang:send/2` BIF route makes, so both routes reach a peer identically.
    ///
    /// `to` is the destination term **verbatim** — it is what the ordinary `send` trace event
    /// carries (the C's `trace_send(p, full_to, msg)`), and the remote transmission is the only
    /// side effect, so there is no `receive` event (no local receiver exists).
    ///
    /// The link must already be established ([`Runtime::dist_connect`]); an unconnected peer is
    /// the connector's typed error, propagated rather than turned into a silent drop, so a real
    /// peer is never mis-served. `Local` is unreachable here and is rejected outright.
    ///
    /// This is also the seam's arm for [`SendDestination::Dropped`] — a `{Name, Node}` pair
    /// naming this node under which nothing is registered (`bif.c:2654`'s `return 0`): there is
    /// no local receiver to copy into and nothing to transmit, so the destination is spent
    /// exactly like the C spends it, leaving only the `send` trace event (which the C also
    /// emits, `bif.c:2631-2632`). The send therefore reports the message term.
    ///
    /// # Errors
    ///
    /// As [`Runtime::dist_send_message_pid`]/[`Runtime::dist_send_reg_send`] (not a node, no
    /// established link, the encoding/socket failures), plus [`BeamError::BadArg`] for a
    /// `Local` destination; and whatever the trace writers report.
    pub(super) fn deliver_remote(
        &mut self,
        destination: SendDestination,
        to: Eterm,
        message: Eterm,
    ) -> Result<()> {
        let from = self.mailbox_pid;
        match destination {
            SendDestination::RemotePid { to: remote } => {
                self.runtime.dist_send_message_pid(remote, from, message)?;
            }
            SendDestination::RemoteName { name, node } => {
                self.runtime.dist_send_reg_send(&node, &name, from, message)?;
            }
            // Accepted and dropped: nothing to deliver and nothing to transmit.
            SendDestination::Dropped => {}
            SendDestination::Local { .. } => return Err(BeamError::BadArg),
        }
        self.trace_send_events(to, message)
    }
}
