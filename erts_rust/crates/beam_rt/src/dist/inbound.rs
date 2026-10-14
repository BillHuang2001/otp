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

//! Inbound delivery: the runtime side of `beam_dist`'s [`InboundSink`] seam.
//!
//! The node half — the listener, the `epmd` registration, the `'N'` handshake and the
//! per-link drain ([`crate::dist::DistNode::dispatch_inbound`]) — lives in the parent
//! module ([`crate::dist`]); this module is the sink those drains hand each classified
//! [`Delivery`] to.
//!
//! The split is the routed / partially-routed / not-routed one documented on
//! [`LocalDeliverySink`]: the ordinary message family and the inbound signal families
//! (link / exit / unlink / monitor / group-leader) are **routed** through the local
//! process table's own route-free receipt primitives; `DOP_UNLINK_ID`'s ack half is
//! **partially routed**; and the distributed-spawn family plus `Delivery::Other` are
//! **not routed**, reported as the honest [`DeliveryOutcome::Unsupported`].

use beam_codec::term::external_ref_parts;
use beam_common::{BeamError, Result};
use beam_dist::{AltactSigFlags, Delivery, DeliveryOutcome, InboundSink};
use beam_process::{
    MonitorKind, MonitorRef, PidTarget, ProcessId, ProcessTable, RemotePid, RemoteRef,
};
use beam_term::prelude::{
    atom_val, is_atom, is_immediate, is_small, make_atom, signed_value, Atom, Eterm,
};


/// The runtime's [`InboundSink`]: routes a classified inbound [`Delivery`] from a
/// distribution link to the addressed **local** process.
///
/// This is `beam_dist`'s "production implementation belongs to `beam_rt`" seam
/// (`beam_dist::delivery`): the value/protocol crate classifies the control
/// message, this sink resolves the addressee against *this* runtime's process
/// table and enqueues the message through `beam_process`'s own delivery path (so a
/// blocked receiver is woken on the table's wake list and resumed by the
/// scheduler, exactly as a port or timer message wakes it).
///
/// # Scope (honest)
///
/// **Routed** — the ordinary message family, which is what a peer's `!`/`erlang:send`
/// produces:
///
/// * [`Delivery::Send`] (`DOP_SEND*`) to a local pid — an internal pid immediate, or
///   the **external** pid shape the distribution decoder materialises for a pid on this
///   node (`dist.c:2512-2561`);
/// * [`Delivery::RegSend`] (`DOP_REG_SEND*`) to a registered local name
///   (`dist.c:2463-2510`);
/// * [`Delivery::AliasSend`] (`DOP_ALIAS_SEND*`) to a live local alias — a process
///   reference alias (`dist.c:2563-2588`; `erl_proc_sig_queue.c:2318-2326`,
///   `:6036-6049`), with the C's `reply_demonitor` alias consumed when the message is
///   really delivered;
/// * the same three targets when they arrive as the **message** form of
///   [`Delivery::AltactSigSend`] (`DOP_ALTACT_SIG_SEND` with neither
///   `ERTS_DOP_ALTACT_SIG_FLG_PRIO` nor `..._EXIT`, `dist.c:2590-2697`).
///
/// **Routed** — the inbound *signal* families, through the route-free **receipt**
/// primitives `beam_process` exposes (each installs/drops the local record and echoes
/// nothing, the C's `break; /* Done */`):
///
/// * [`Delivery::Link`] (`DOP_LINK`, `dist.c:2224`);
/// * [`Delivery::Exit`]/[`Delivery::Exit2`] and their payload forms
///   (`DOP_EXIT`/`DOP_EXIT2`, `dist.c:2760`/`:2824`), with `untrappable` selecting the
///   link-exit vs `erlang:exit/2` family;
/// * [`Delivery::UnlinkIdAck`] (the ack **consumption** half of the OTP-26+ handshake);
/// * [`Delivery::Monitor`]/[`Delivery::Demonitor`] (`DOP_MONITOR_P`/`DOP_DEMONITOR_P`,
///   `dist.c:2341`/`:2415`): a peer's monitor of a local pid-or-name is recorded as a
///   [`RemoteWatcher`](beam_process::RemoteWatcher) keyed by the whole **peer-minted wire
///   reference** ([`RemoteRef`](beam_process::RemoteRef)), and its demonitor removes exactly
///   that record (see [`LocalDeliverySink::monitor`]);
/// * [`Delivery::MonitorExit`]/[`Delivery::PayloadMonitorExit`]
///   (`DOP_MONITOR_P_EXIT`, `dist.c:2700-2714`) for a DOWN whose reference is one
///   **this** node minted (see [`LocalDeliverySink::wire_monitor_ref_id`]);
/// * [`Delivery::GroupLeader`] (`DOP_GROUP_LEADER`, `dist.c:2889`).
///
/// **Partially routed** — [`Delivery::UnlinkId`]: the local link-record removal IS
/// applied, but an `id != 0` additionally owes a `DOP_UNLINK_ID_ACK` this sink has no
/// channel to emit, so that case reports the typed
/// [`DeliveryOutcome::Unsupported`] for the wire half (see its arm).
///
/// **Not routed** — the distributed-spawn family and [`Delivery::Other`] are reported as
/// [`DeliveryOutcome::Unsupported`], with the still-missing sibling API named at each
/// match arm. (The `DOP_MONITOR_P_EXIT`-on-local-death echo a recorded
/// [`RemoteWatcher`](beam_process::RemoteWatcher) would owe its peer is a separate
/// **outbound** concern, still unimplemented and out of this sink's scope.)
///
/// Every unrouted arm is *structural*, not incidental: routing one through the nearest
/// *locally-initiated* `beam_process` entry point (`link_term`/`monitor_term`/`…`) would
/// emit an outbound `DOP_*` the C's inbound receipt arm never sends. Guessing at the
/// semantics — or echoing the mirrored opcode — is far worse than an honest gap.
///
/// Nothing is ever delivered to the *wrong* process: an addressed pid that is not
/// a process of this node (another node, or a stale incarnation of this one) is
/// [`DeliveryOutcome::NoSuchProcess`], never a lookup by number alone; a delivery
/// outside its opcode's wire contract (a non-pid `DOP_SEND` target, a non-atom
/// `DOP_REG_SEND` name, a non-reference `DOP_ALIAS_SEND` target) is
/// [`DeliveryOutcome::Unsupported`]; and a well-formed delivery whose recipient does
/// not exist is dropped ([`DeliveryOutcome::NoSuchProcess`] /
/// [`DeliveryOutcome::Dropped`]) exactly as the C's silent-drop arms do.
pub struct LocalDeliverySink<'a> {
    /// The runtime's process table the delivery is enqueued into.
    table: &'a mut ProcessTable,
    /// This node's full `node@host` name, as the interned atom term a decoded
    /// external pid carries in its node slot.
    local_node: Eterm,
    /// This node's creation (the incarnation the pid must belong to).
    local_creation: u32,
}

impl<'a> LocalDeliverySink<'a> {
    /// A sink over `table` for a node named by `local_node` (its interned atom) with
    /// the given `local_creation`.
    pub fn new(table: &'a mut ProcessTable, local_node: Atom, local_creation: u32) -> Self {
        LocalDeliverySink {
            table,
            local_node: make_atom(local_node),
            local_creation,
        }
    }
}

impl InboundSink for LocalDeliverySink<'_> {
    fn deliver(&mut self, delivery: &Delivery) -> Result<DeliveryOutcome> {
        match delivery {
            // --- routed: the ordinary message family (see the type docs) -----
            Delivery::Send {
                from,
                to,
                message,
                ..
            } => self.deliver_to_pid(*from, *to, *message),
            Delivery::RegSend {
                from,
                name,
                message,
                ..
            } => self.deliver_to_name(*from, *name, *message),
            Delivery::AliasSend {
                from,
                to,
                message,
                ..
            } => self.deliver_to_alias(*from, *to, *message),
            Delivery::AltactSigSend {
                flags,
                from,
                to,
                message,
                ..
            } => self.deliver_altact(*flags, *from, *to, *message),

            // --- routed: the link / exit / unlink / monitor-down signal family --
            //
            // Every family below goes through the **route-free receipt** half of the
            // distribution seam — the `ProcessTable` entry points documented under
            // "inbound (receipt) distribution primitives" (`ptab.rs`), each of which
            // installs (or drops) the local record and then follows the C's
            // `break; /* Done */` (`dist.c:2258`), never echoing the opcode it
            // received. Routing them through the nearest *locally-initiated* entry
            // point (`link_term`/`unlink_term`/`monitor_term`/`demonitor`/
            // `send_exit_term`) would instead put a `DOP_*` the C's inbound arm never
            // sends on the wire through the installed `DistSignalRouter`.
            Delivery::Link { from, to } => self.record_link(*from, *to),

            // Exit signals from a remote sender (`DOP_EXIT`/`DOP_EXIT2`, `dist.c:2760`
            // /`:2824`, and their payload forms). The C family selects the trap rule,
            // and `untrappable` carries exactly that split: `DOP_EXIT`/
            // `DOP_PAYLOAD_EXIT` are **link exits** (the recorded incoming remote link
            // is removed first, and with no such link the signal is ignored outright —
            // `erl_proc_sig_queue.c:4674-4680`), while `DOP_EXIT2`/`DOP_PAYLOAD_EXIT2`
            // are the untrappable `erlang:exit/2` family.
            //
            // The C answers a dead `to` with a `DOP_EXIT` carrying `noproc`
            // (`dist.c:2269-2275`); that reply has no channel on this sink, so a
            // non-live `to` is the typed `NoSuchProcess` instead (see the type docs).
            Delivery::Exit { from, to, reason, .. } => {
                self.exit_signal(*from, *to, *reason, false)
            }
            Delivery::Exit2 { from, to, reason, .. } => {
                self.exit_signal(*from, *to, *reason, true)
            }
            Delivery::PayloadExit { from, to, payload, .. } => {
                self.exit_signal(*from, *to, *payload, false)
            }
            Delivery::PayloadExit2 { from, to, payload, .. } => {
                self.exit_signal(*from, *to, *payload, true)
            }

            // The OTP-26+ unlink-id handshake (`DOP_UNLINK_ID`/`DOP_UNLINK_ID_ACK`,
            // `dist.c:2284`/`:2315`).
            //
            // `DOP_UNLINK_ID_ACK` **consumes** an ack: it resolves a pending unlink
            // this node initiated, so it owes no reply and is `Delivered` once `to` is
            // a live local process. (`unlink_ack_from_remote` is the route-free
            // counterpart of `unlink_term`.) The pending-op bookkeeping it would
            // resolve does not exist: `beam_rt`'s `dist_client` mints the unlink id on
            // the fly and keeps no record of it — a gap *there*, recorded in
            // `../CONTEXT.md`, not a routing gap here.
            //
            // `DOP_UNLINK_ID` is the half that **must answer**. The C removes the link
            // and then sends `DOP_UNLINK_ID_ACK` unconditionally
            // (`reply_dist_unlink_ack`, `erl_proc_sig_queue.c:2753-2760`; nothing is
            // sent only when `id == 0`). The local link-record removal IS applied
            // (`unlink_from_remote`), but the `ACK` **cannot be emitted**: `InboundSink`
            // carries no reply channel and the drains count only `Delivered` (see the
            // type docs), so this reports the typed `Unsupported` for the wire half
            // rather than claiming an ack the runtime cannot produce. The `id == 0`
            // form owes no ack and is `Delivered`.
            Delivery::UnlinkId { id, from, to } => self.unlink_id(*id, *from, *to),
            Delivery::UnlinkIdAck { id, from, to } => self.unlink_ack(*id, *from, *to),

            // A **remote watcher** monitoring a local process/name
            // (`DOP_MONITOR_P`/`DOP_DEMONITOR_P`, `dist.c:2341`/`:2415`).
            //
            // The C's inbound arms only install (respectively, remove) the **local record**:
            // `DOP_MONITOR_P` creates the monitor and queues the local monitor signal, then
            // `break; /* done */` (`dist.c:2341-2381`), and `DOP_DEMONITOR_P` sends the local
            // demonitor signal and answers nothing (`dist.c:2415-2460`). Neither echoes its own
            // opcode, so neither may go through a *locally-initiated* entry point
            // (`monitor_term`/`demonitor`, which route a `DOP_*`).
            //
            // The reference a **peer** puts on the wire is one *it* minted — a full external
            // reference whose identity is `(node, creation, ids[0..n])`. It is decomposed into a
            // `RemoteRef` and the record is keyed by the whole identity, so two distinct peer
            // monitors of one local process never collapse (a replaced watcher, or a `demonitor`
            // that cancels the wrong monitor). `beam_process::RemoteRef::from_eterm` is the safe
            // decoder — the `unsafe` block reader stays confined to `beam_process` — and a term
            // that is not a well-formed external reference is outside this opcode's wire contract:
            // `Unsupported`.
            Delivery::Monitor {
                watcher,
                watched,
                reference,
            } => self.monitor(*watcher, *watched, *reference),
            Delivery::Demonitor {
                watcher,
                watched,
                reference,
            } => self.demonitor(*watcher, *watched, *reference),

            // A monitor **DOWN** from a remote target (`DOP_MONITOR_P_EXIT` /
            // `DOP_PAYLOAD_MONITOR_P_EXIT`, `dist.c:2700-2714`). This is the **inverse**
            // of a monitor *this* node initiated: the reference on the wire is the one
            // `dist_client` put into the outgoing `DOP_MONITOR_P` (an external reference
            // on **our** node, `{our_node, our_creation, [0, id >> 32, id]}`), which the
            // peer echoes back verbatim. `wire_monitor_ref_id` therefore recovers the
            // very `MonitorRef` the outgoing record was keyed by, so the recorded
            // `RemoteMonitor` is found and the DOWN matches its monitor exactly.
            //
            // A DOWN whose `watched` is a registered **name** (the C's `dist.c:2722`
            // accepts an external pid *or* an atom) is not representable: the receipt
            // API takes the resolved `RemotePid`. That stays a typed `Unsupported`.
            Delivery::MonitorExit {
                watched,
                watcher,
                reference,
                reason,
            } => self.monitor_down(*watched, *watcher, *reference, *reason),
            Delivery::PayloadMonitorExit {
                watched,
                watcher,
                reference,
                payload,
            } => self.monitor_down(*watched, *watcher, *reference, *payload),

            // `DOP_GROUP_LEADER` (`dist.c:2889`): installs the remote `from` as the
            // local `to`'s group leader. `set_group_leader_from_remote` records it as a
            // `PidTarget::Remote` — readable back through `ProcessTable::group_leader`
            // — and routes nothing, exactly as the C's arm does.
            Delivery::GroupLeader { from, to } => self.group_leader(*from, *to),

            // The distributed-spawn family (`DOP_SPAWN_REQUEST`/`DOP_SPAWN_REPLY`,
            // `dist.c:2902-3093`): a `beam_rt` CLI process neither serves spawn
            // requests nor holds spawn reservations, so there is no responder. Needs a
            // `beam_process` spawn-responder/reservation API — a `DOP_SPAWN_REPLY`
            // sender plus the reservation bookkeeping a `DOP_SPAWN_REQUEST` would key
            // into — of which none exists.
            Delivery::SpawnRequest { .. } | Delivery::SpawnReply { .. } => {
                Ok(DeliveryOutcome::Unsupported)
            }

            // `DOP_UNLINK` is not a v6 opcode the C accepts at all: it falls into the
            // `invalid_message` arm and **kills the connection** (`dist.c:2277-2282`).
            // A `DOP_UNLINK` reaching this sink is therefore a protocol violation; the
            // caller (the link driver) owns the connection-level answer, so the sink
            // reports the honest "recognised, not routed" and never a fabricated one.
            Delivery::Other(_) => Ok(DeliveryOutcome::Unsupported),
        }
    }
}

/// The mailbox **envelope**'s sender word for a message classified off the wire.
///
/// [`ProcessTable::deliver_simple`] stores `from` in the queue entry's envelope, and
/// that slot must be an **immediate**: `beam_message` gathers only each queued
/// message's *payload* as a GC root, so a non-immediate word left there is not
/// relocated and dangles into the evacuated from-space the moment the owning process
/// collects (`beam_message/src/message.rs`'s `debug_assert_enqueue_terms`). An
/// external pid classified off the wire is exactly such a word: the distribution
/// decoder materialises it in the dispatch arena, which [`DistNode::dispatch_inbound`]
/// **resets on every drain** — and `Runtime::dispatch_inbound`'s caller-supplied arena
/// is reset by its caller. Passing the classified `from` straight through therefore
/// stored a dangling envelope.
///
/// The C's own rule is exact (`erl_message.c:307`): `ERL_MESSAGE_FROM(mp) =
/// dist_ext->dep->sysname` — the **peer node atom**, never the sender pid (the pid is
/// used only for per-sender signal ordering, `erl_message.c:324-329`). This derives
/// exactly that from an external-pid sender's node slot: an atom term, i.e. an
/// immediate, safe to hold for the message's whole queued life. The plain `DOP_SEND*`
/// forms need no derivation — the peer puts the empty atom `''` there
/// (`dist.c:2535-2542`) — and an internal pid is already an immediate.
///
/// `None` means no faithful immediate word exists: a boxed sender that is neither an
/// internal nor an external pid (outside both opcodes' wire contract) or a
/// pathological external pid whose node slot is not an atom immediate. The delivery is
/// then refused rather than enqueued with a term that would dangle.
///
/// **Typed divergence from the C:** for a plain `DOP_SEND*` the C still stores the peer's
/// *sysname*, while this keeps the wire's `''`. Both are immediates and neither is ever
/// read as a process identity by this runtime (nothing here resolves a mailbox
/// envelope), so the divergence is inert — and deriving the sysname would need the
/// peer's node name plumbed down to the sink, which this seam does not carry.
fn envelope_from(from: Eterm) -> Option<Eterm> {
    if is_immediate(from) {
        return Some(from);
    }
    match PidTarget::from_eterm(from) {
        Ok(PidTarget::Remote(remote)) => {
            let node = remote.node();
            // The pid block's node slot is an atom term (an immediate); refuse anything
            // else rather than store a word the GC walk cannot keep alive.
            (is_immediate(node) && is_atom(node)).then_some(node)
        }
        _ => None,
    }
}

/// Where an addressed pid term points (see [`LocalDeliverySink::resolve`]).
enum Target {
    /// A local process in this node's table.
    Local(ProcessId),
    /// Not a process of *this* node (another node, or a stale incarnation).
    Foreign,
    /// Not a pid at all — outside the wire contract for this opcode.
    NotAPid,
}

impl LocalDeliverySink<'_> {
    /// Routes a `DOP_SEND*` to the addressed local pid.
    ///
    /// The C's receipt (`dist.c:2512-2561`) rejects a non-pid `to` as an
    /// `invalid_message` and **silently drops** a message whose `to` is not a live local
    /// process (a dead pid, or a stale incarnation of this node) — it answers the peer
    /// with nothing at all.
    fn deliver_to_pid(
        &mut self,
        from: Eterm,
        to: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        match self.resolve(to) {
            Target::Local(pid) => self.enqueue(pid, from, message),
            Target::Foreign => Ok(DeliveryOutcome::NoSuchProcess),
            Target::NotAPid => Ok(DeliveryOutcome::Unsupported),
        }
    }

    /// Routes a `DOP_REG_SEND*` to the addressed registered local name.
    fn deliver_to_name(
        &mut self,
        from: Eterm,
        name: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        if !is_atom(name) {
            // `DOP_REG_SEND`'s `ToName` is an atom (`dist.c:2486`); anything else is
            // outside the contract for this opcode.
            return Ok(DeliveryOutcome::Unsupported);
        }
        match self.table.whereis(atom_val(name)) {
            Some(pid) => self.enqueue(pid, from, message),
            // A name nobody holds drops the message (the oracle drops it too — it
            // answers the peer with nothing, `dist.c:2506-2509`). A name registered to
            // a *port* is the same silent drop in the C, and `ProcessTable::whereis`
            // resolves process names only, so it lands here too.
            None => Ok(DeliveryOutcome::NoSuchProcess),
        }
    }

    /// Routes a `DOP_ALIAS_SEND*` through this node's **reference alias** table.
    ///
    /// The C's receipt is `erts_proc_sig_send_dist_altact_msg` (`dist.c:2563-2588`
    /// → `erl_proc_sig_queue.c:2293-2412`): `To` must be a reference (`is_not_ref(to)`
    /// ⇒ `invalid_message`), the reference's own embedded owner pid must be an
    /// **internal** pid — a reference whose owner is not local is dropped outright
    /// (`:2318-2326`) — and the alias is then resolved inside the owner's monitor tree,
    /// where a reference that is not a live alias is **silently dropped**
    /// (`:6036-6046`: *"Not an alias (never has been, not anymore, or not yet); drop
    /// message…"*). The peer is answered with nothing in every one of those cases.
    ///
    /// A message really delivered through an `ALIAS_ONCE` (`reply_demonitor`) alias
    /// **consumes** it (`erl_proc_sig_queue.c:6047-6064`), so that is what happens here —
    /// after, and only after, a successful enqueue.
    fn deliver_to_alias(
        &mut self,
        from: Eterm,
        to: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        let Ok(reference) = MonitorRef::from_eterm(to) else {
            // Not an internal reference thing: outside the opcode's contract
            // (`dist.c:2577`), so neither a delivery nor a drop is claimed for it.
            //
            // A wire-borne reference currently arrives in the **external** form — the
            // control-message decoder installs no local-node identity
            // (`beam_dist::control::decode_control`), so the codec's local-node
            // demotion never runs — and an external reference is not a readable alias
            // key here. That is a `beam_dist` handoff, recorded rather than worked
            // around: see the module's escalation notes.
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(owner) = self.table.resolve_alias(reference) else {
            // Never an alias, its rule already fired, or its owner has died: the C
            // drops the message and tells the peer nothing.
            return Ok(DeliveryOutcome::Dropped);
        };
        let outcome = self.enqueue(owner, from, message)?;
        if outcome == DeliveryOutcome::Delivered {
            // The reply-alias consumption (`erl_proc_sig_queue.c:6047-6049`); a
            // `demonitor`/`explicit_unalias` alias survives the delivery, which the
            // table's own mode check enforces.
            self.table.alias_reply_delivered(reference)?;
        }
        Ok(outcome)
    }

    /// Routes the sub-cases of `DOP_ALTACT_SIG_SEND` that land in the ordinary
    /// message paths above.
    ///
    /// The C reads the flag word and dispatches accordingly
    /// (`dist.c:2662-2696`): `..._EXIT` is an **exit signal** (the trailing term is
    /// the reason), otherwise a **message**; `..._ALIAS` says `To` is a reference,
    /// `..._NAME` that it is a registered name, and neither that it is a pid.
    /// `..._PRIO` makes the signal jump the mailbox queue.
    ///
    /// Only the plain **message** forms are routed ([`Self::deliver_to_pid`] /
    /// [`Self::deliver_to_name`] / [`Self::deliver_to_alias`]) — a priority signal
    /// needs a priority queue this mailbox has no seam for, and an exit signal needs the
    /// `apply_remote_exit_signal` named on [`Delivery::Exit`]'s arm. Both stay typed
    /// gaps rather than being delivered as ordinary messages (which would silently
    /// reorder a priority signal and turn an untrappable exit into a message).
    fn deliver_altact(
        &mut self,
        flags: AltactSigFlags,
        from: Eterm,
        to: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        if flags.contains(AltactSigFlags::EXIT) {
            // `ERTS_DOP_ALTACT_SIG_FLG_EXIT`: the trailing term is an exit *reason*
            // (`dist.c:2687-2692`), with the `DOP_EXIT2` semantics — untrappable, so it
            // must not be enqueued as a message.
            return Ok(DeliveryOutcome::Unsupported);
        }
        if flags.contains(AltactSigFlags::PRIO) {
            // `ERTS_DOP_ALTACT_SIG_FLG_PRIO`: the signal must jump the mailbox queue
            // (`dist.c:2684`/`:2693-2696`), which the ordinary enqueue here cannot do.
            return Ok(DeliveryOutcome::Unsupported);
        }
        if flags.contains(AltactSigFlags::ALIAS) {
            // `To` is a reference (the C rejects `ALIAS` together with `NAME`,
            // `dist.c:2666-2669`; the classifier has already applied that rule).
            return self.deliver_to_alias(from, to, message);
        }
        if flags.contains(AltactSigFlags::NAME) {
            // `To` is a registered name; an unregistered one is a silent drop
            // (`dist.c:2670-2680`).
            return self.deliver_to_name(from, to, message);
        }
        // Otherwise `To` is a pid (`dist.c:2682-2685`).
        self.deliver_to_pid(from, to, message)
    }

    /// Classifies an addressed pid term as a process of this node.
    ///
    /// An **internal** pid immediate is a local process by construction. The
    /// distribution decoder materialises a pid as an **external** pid thing
    /// (`beam_codec`), so a destination on this node arrives as an external pid
    /// whose node atom and creation match ours — and only then is it looked up (by
    /// number *and* serial, never number alone).
    fn resolve(&self, term: Eterm) -> Target {
        match PidTarget::from_eterm(term) {
            Ok(PidTarget::Local(pid)) => Target::Local(pid),
            Ok(PidTarget::Remote(remote))
                if remote.node() == self.local_node
                    && remote.creation() == self.local_creation =>
            {
                Target::Local(ProcessId::new(
                    u64::from(remote.number()),
                    u64::from(remote.serial()),
                ))
            }
            Ok(PidTarget::Remote(_)) => Target::Foreign,
            Err(_) => Target::NotAPid,
        }
    }

    /// Enqueues `message` (from `from`) into `pid`'s mailbox, waking a blocked
    /// receiver through the process table's own wake list.
    ///
    /// The envelope's sender word is derived by [`envelope_from`] — shared by every
    /// routed family, because the rule is the message envelope's, not the opcode's.
    fn enqueue(
        &mut self,
        pid: ProcessId,
        from: Eterm,
        message: Eterm,
    ) -> Result<DeliveryOutcome> {
        let Some(envelope) = envelope_from(from) else {
            // No immediate sender word exists for this delivery, and storing the
            // classified word would leave a dangling envelope (see [`envelope_from`]).
            // Refuse it: nothing is delivered, nothing is mis-delivered.
            return Ok(DeliveryOutcome::Unsupported);
        };
        if !self.table.contains(pid) {
            // The pid decoded to a slot this table does not hold (a dead process, or
            // a number that was never ours).
            return Ok(DeliveryOutcome::NoSuchProcess);
        }
        match self.table.deliver_simple(pid, envelope, message) {
            Ok(_) => Ok(DeliveryOutcome::Delivered),
            // The receiver's heap could not hold the message even after a
            // collection (this runtime has no inbound heap-fragment delivery). Drop
            // it rather than mis-deliver it, and keep the link up: one oversized
            // message from a peer must not tear the link down. Reported honestly as
            // `Dropped`.
            Err(BeamError::Alloc(_)) => Ok(DeliveryOutcome::Dropped),
            // Anything else is an unexpected table invariant break — surface it (the
            // caller retires the offending link) rather than swallowing it.
            Err(err) => Err(err),
        }
    }

    // --- the routed signal families (route-free receipt) ---------------------

    /// Applies an inbound `DOP_LINK`: records the remote `from` as linked to the local
    /// `to`. **Record-only** — the C's arm ends `break; /* Done */` (`dist.c:2258`) and
    /// sends no `DOP_LINK` back.
    fn record_link(&mut self, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            // `DOP_LINK`'s `From` is an external pid (`dist.c:2234`); the per-link
            // validator already refuses anything else, so this only catches the
            // unchecked call.
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.record_remote_link(pid, remote))
    }

    /// Applies an inbound `DOP_EXIT`/`DOP_EXIT2` (and their payload forms) to the local
    /// `to`, with `untrappable` selecting the C's operation family (see the `Exit` arm).
    fn exit_signal(
        &mut self,
        from: Eterm,
        to: Eterm,
        reason: Eterm,
        untrappable: bool,
    ) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(
            self.table
                .apply_remote_exit_signal(pid, remote, reason, untrappable),
        )
    }

    /// Applies an inbound `DOP_UNLINK_ID`: drops the recorded remote link and reports
    /// what the **wire** half owes (see the `UnlinkId` arm).
    ///
    /// `unlink_from_remote` hands back the id an ack is required for: `Some(id)` iff
    /// `id != 0`, which is exactly the C's split (`reply_dist_unlink_ack` sends nothing
    /// for `id == 0`, `erl_proc_sig_queue.c:2753-2760`). The link is dropped in both
    /// cases; only the `id != 0` case additionally owes a `DOP_UNLINK_ID_ACK`, which
    /// this sink has no channel to emit, so it is reported `Unsupported`.
    fn unlink_id(&mut self, id: Eterm, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(id) = small_uint(id) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        match self.table.unlink_from_remote(pid, remote, id) {
            // `id == 0`: no ack owed, the unlink is complete locally.
            Ok(None) => Ok(DeliveryOutcome::Delivered),
            // The link is dropped, but the ack this node owes cannot be emitted.
            Ok(Some(_)) => Ok(DeliveryOutcome::Unsupported),
            Err(BeamError::TermNotFound) => Ok(DeliveryOutcome::NoSuchProcess),
            Err(err) => Err(err),
        }
    }

    /// Applies an inbound `DOP_UNLINK_ID_ACK`: validates the addressed local process
    /// and lets the table consume the ack. It routes nothing (the C answers nothing).
    fn unlink_ack(&mut self, id: Eterm, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(id) = small_uint(id) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.unlink_ack_from_remote(pid, remote, id))
    }

    /// Applies an inbound `DOP_GROUP_LEADER`: the remote `from` becomes the local `to`'s
    /// group leader. Routes nothing.
    fn group_leader(&mut self, from: Eterm, to: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(to) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(from) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.set_group_leader_from_remote(pid, remote))
    }

    /// Applies an inbound `DOP_MONITOR_P` — `{DOP_MONITOR_P, Remote(watcher), Local
    /// pid-or-name(watched), Ref}` (`dist.c:2341`): records the `watcher`'s monitor of the local
    /// `watched`. Routes nothing (the C's arm ends `break; /* done */` once the local monitor
    /// signal is queued).
    ///
    /// `watched` is the C's `local pid or name` slot: an internal pid (or an external pid of
    /// *this* node, the shape the decoder materialises) resolves through
    /// [`LocalDeliverySink::resolve_live`], and a registered **name** through the table's name
    /// registry (`erts_whereis_name_to_id`, `dist.c:2353-2360`) — an unregistered name is the
    /// honest [`DeliveryOutcome::NoSuchProcess`] (the C replies `noproc`, a wire artifact this
    /// sink owes no channel for).
    ///
    /// The wire `watcher` must be an external pid and the wire `Ref` a peer-minted external
    /// reference; anything else is outside this opcode's wire contract and reported
    /// [`DeliveryOutcome::Unsupported`]. The reference is decomposed into a
    /// [`RemoteRef`](beam_process::RemoteRef) and stored whole, so the record is faithful — a
    /// later demonitor matches exactly this reference (see [`LocalDeliverySink::demonitor`]).
    ///
    /// `DOP_MONITOR_P` is a **process** monitor on the wire, so the record's kind is
    /// [`MonitorKind::Process`](beam_process::MonitorKind).
    fn monitor(&mut self, watcher: Eterm, watched: Eterm, reference: Eterm) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_watched(watched) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(watcher) = remote_pid(watcher) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        // A term that is not a well-formed external reference (`is_not_ref` in the C,
        // `dist.c:2350`) is outside the opcode's contract; the per-link validator owns the
        // connection-level answer.
        let Ok(reference) = RemoteRef::from_eterm(reference) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(
            self.table
                .monitor_from_remote_ref(pid, watcher, reference, MonitorKind::Process),
        )
    }

    /// Applies an inbound `DOP_DEMONITOR_P` — `{DOP_DEMONITOR_P, Remote(watcher), Local
    /// pid-or-name(watched), Ref}` (`dist.c:2415`): removes the `watch`er's record under
    /// `reference`. Routes nothing (the C's arm answers nothing).
    ///
    /// The same resolution as [`LocalDeliverySink::monitor`]: `watched` is a local pid / this
    /// node's external pid / a registered name; the wire `watcher` must be an external pid and the
    /// wire `Ref` a peer-minted external reference. Removal matches the reference by its **whole**
    /// decomposed identity, so it cancels exactly the monitor the matching `DOP_MONITOR_P`
    /// installed, never a different one.
    fn demonitor(
        &mut self,
        watcher: Eterm,
        watched: Eterm,
        reference: Eterm,
    ) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_watched(watched) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        // The demonitor key is `(watched, ref)` — the C still validates the `watcher` is an
        // external pid on the peer's dist entry (`dist.c:2424`), so a bogus one is outside the
        // wire contract rather than a silent removal.
        if remote_pid(watcher).is_none() {
            return Ok(DeliveryOutcome::Unsupported);
        }
        let Ok(reference) = RemoteRef::from_eterm(reference) else {
            return Ok(DeliveryOutcome::Unsupported);
        };
        receipt(self.table.demonitor_from_remote_ref(pid, reference))
    }

    /// Resolves the `watched` slot of the monitor family: a local pid (or this node's external
    /// pid) through [`LocalDeliverySink::resolve_live`], or a registered **name** through the
    /// table's process-name registry.
    ///
    /// An unregistered name, a pid of another node and a stale incarnation of this one are all
    /// [`DeliveryOutcome::NoSuchProcess`] — the C replies `noproc` for the first two
    /// (`dist.c:2353-2362`, `erl_proc_sig_queue.c`) and drops the stale-pid case. A term that is
    /// neither a pid nor an atom is outside the wire contract: [`DeliveryOutcome::Unsupported`].
    fn resolve_watched(&self, watched: Eterm) -> std::result::Result<ProcessId, DeliveryOutcome> {
        if is_atom(watched) {
            return match self.table.whereis(atom_val(watched)) {
                Some(pid) => Ok(pid),
                None => Err(DeliveryOutcome::NoSuchProcess),
            };
        }
        self.resolve_live(watched)
    }
    /// Applies an inbound `DOP_MONITOR_P_EXIT`/`DOP_PAYLOAD_MONITOR_P_EXIT`: resolves the
    /// **local** `watcher`'s outgoing remote monitor under `reference` and delivers the
    /// `{'DOWN', …}` — without routing any `DOP_DEMONITOR_P` back (the C's arm queues
    /// only the local DOWN signal).
    ///
    /// The reference is looked up *through the watcher's own record*, exactly as the C
    /// resolves the DOWN through the watcher's monitor tree and **drops** the signal when
    /// the lookup misses (`erl_proc_sig_queue.c:6355-6396`) — so an unmatched DOWN is the
    /// honest `Dropped`, never a fabricated one. Using the record's own `MonitorRef` also
    /// avoids inventing one: the wire id is only the lookup key, never a term.
    fn monitor_down(
        &mut self,
        watched: Eterm,
        watcher: Eterm,
        reference: Eterm,
        reason: Eterm,
    ) -> Result<DeliveryOutcome> {
        let pid = match self.resolve_live(watcher) {
            Ok(pid) => pid,
            Err(outcome) => return Ok(outcome),
        };
        let Some(remote) = remote_pid(watched) else {
            // A **name**-targeted DOWN (an atom `watched`) is outside what the receipt
            // API can express — it takes the resolved `RemotePid` (see the arm).
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(wanted) = self.wire_monitor_ref_id(reference) else {
            // A reference this node did not mint cannot match an outgoing record.
            return Ok(DeliveryOutcome::Unsupported);
        };
        let Some(record) = self
            .table
            .remote_monitors(pid)
            .iter()
            .find(|monitor| monitor.reference.id() == wanted)
            .copied()
        else {
            return Ok(DeliveryOutcome::Dropped);
        };
        receipt(self.table.remote_monitor_down(
            pid,
            remote,
            record.reference,
            reason,
        ))
    }

    /// Resolves an addressed pid term to a **live** local process, or the honest outcome
    /// for one that is not.
    ///
    /// A pid on another node, or a stale incarnation of this one, is never looked up by
    /// its slot number alone — [`Self::resolve`] classifies it [`Target::Foreign`], and a
    /// local slot this table does not hold is the same `NoSuchProcess`. A non-pid (for
    /// this opcode) is outside the wire contract: `Unsupported`.
    fn resolve_live(&self, to: Eterm) -> std::result::Result<ProcessId, DeliveryOutcome> {
        match self.resolve(to) {
            Target::Local(pid) if self.table.contains(pid) => Ok(pid),
            Target::Local(_) | Target::Foreign => Err(DeliveryOutcome::NoSuchProcess),
            Target::NotAPid => Err(DeliveryOutcome::Unsupported),
        }
    }

    /// Recovers the `MonitorRef` **id** of a reference that came back off the wire and
    /// was minted by *this* runtime — the exact inverse of the pack
    /// [`crate::dist_client`]'s `monitor_ref_term` puts on the wire.
    ///
    /// A monitor this node initiates on a remote process records a
    /// [`RemoteMonitor`](beam_process::RemoteMonitor) under a `MonitorRef` and routes a
    /// `DOP_MONITOR_P` whose `Ref` element is `{our_node, our_creation, [0, id >> 32,
    /// id]}` (three 32-bit words, most significant first, leading word zeroed). The peer
    /// echoes that identical reference image in the `DOP_MONITOR_P_EXIT` it sends when the
    /// monitored process dies, and the control-message decoder materialises it as an
    /// **external** reference (`beam_dist::control::decode_control` installs no
    /// local-node identity, so the codec's local-node demotion never runs).
    ///
    /// This undoes that packing exactly, and only for a reference this node minted: the
    /// node and creation must be **this** node's and the leading id word zero. Anything
    /// else — in particular a reference a **peer** minted, whose node is not ours — is
    /// `None`, never a guessed id: such an identity has no faithful `u64` image (see the
    /// `Monitor`/`Demonitor` arm).
    fn wire_monitor_ref_id(&self, term: Eterm) -> Option<u64> {
        let (node, ids, creation) = external_ref_parts(term).ok()?;
        if node != self.local_node || creation != self.local_creation {
            return None;
        }
        let (high, low) = match ids {
            [0, high, low] => (*high, *low),
            _ => return None,
        };
        Some((u64::from(high) << 32) | u64::from(low))
    }
}

/// The sink outcome of a **route-free receipt** call: `Delivered` on success, or the
/// honest `NoSuchProcess` when the table reports the addressed local party is not live
/// (every receipt primitive reports that as `BeamError::TermNotFound`).
fn receipt(result: Result<()>) -> Result<DeliveryOutcome> {
    match result {
        Ok(()) => Ok(DeliveryOutcome::Delivered),
        Err(BeamError::TermNotFound) => Ok(DeliveryOutcome::NoSuchProcess),
        Err(err) => Err(err),
    }
}

/// The remote pid an **external** pid thing names, or `None` for anything else.
fn remote_pid(term: Eterm) -> Option<RemotePid> {
    RemotePid::from_eterm(term).ok()
}

/// The `u64` value of a non-negative **small** integer term, or `None` (a bignum or a
/// non-number id is outside this safe path).
fn small_uint(term: Eterm) -> Option<u64> {
    if !is_small(term) {
        return None;
    }
    u64::try_from(signed_value(term)).ok()
}

