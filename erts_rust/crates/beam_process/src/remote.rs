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

//! Remote-node (distributed) links and monitors: [`RemotePid`], [`PidTarget`],
//! [`RemoteMonitor`], [`DistSignal`] and the [`DistSignalRouter`] seam.
//!
//! This module is the **local record** of a process's relationships with processes living on
//! *other* nodes. A local pid is fully described by [`crate::pid::ProcessId`] (a table number
//! plus a serial), but a remote process is identified by an **external pid** term: the frozen
//! 4-word thing `[header, node, creation, data]` whose `data` word holds two `u32`s (`number`,
//! `serial`) and whose `node` slot holds the sysname atom term (see `beam_term::thing`).
//! [`RemotePid`] decomposes that term into its stable parts so the table never retains a boxed
//! pointer into another heap; [`RemotePid::to_eterm`] materialises it again on demand.
//!
//! # The distribution seam is a trait (anti-cycle)
//!
//! Sending `DOP_LINK`/`DOP_MONITOR_P`/… over the wire is `beam_dist`'s job, but `beam_process`
//! is a Stage-5 crate that must not depend on it (the wire crate is a Stage-2 foundation and
//! pulling it in here would invert the layer DAG). The seam is therefore the
//! [`DistSignalRouter`] trait — exactly the pattern
//! [`ProcessRunner`](crate::scheduler::ProcessRunner) uses for the engine: the runtime
//! (`beam_rt`) supplies the concrete implementation, which forwards each [`DistSignal`] to a
//! `beam_dist::Connection`. `beam_process` only *records* the relationship locally and *asks*
//! the router to serialise the control signal.
//!
//! # The handoff: signal → distribution control message
//!
//! A downstream implementor of [`DistSignalRouter`] maps each signal onto one distributed
//! control message, exactly as follows (so the wire format stays owned by `beam_dist`). The
//! argument orders are the C's, read off the `erts_dsig_send_*` builders in the send half of
//! `dist.c`:
//!
//! ```text
//! DistSignal::Link      -> {DOP_LINK,        From(local), To(remote)}                 -> Dop::Link,       &[from.to_eterm(),  target_term]          dist.c:1456
//! DistSignal::Unlink    -> {DOP_UNLINK_ID,   Id, From(local), To(remote)}             -> Dop::UnlinkId,   &[id_term, from.to_eterm(), target_term]  dist.c:1472
//! DistSignal::Monitor   -> {DOP_MONITOR_P,   Watcher(local), Watched(remote), Ref}    -> Dop::MonitorP,   &[watcher.to_eterm(), target_term, ref_term]  dist.c:1543
//! DistSignal::Demonitor -> {DOP_DEMONITOR_P, Watcher(local), Watched(remote), Ref}    -> Dop::DemonitorP, &[watcher.to_eterm(), target_term, ref_term]  dist.c:1566
//! DistSignal::Exit      -> {DOP_EXIT,        From(local), To(remote), Reason}        -> Dop::Exit,       &[from.to_eterm(),  target_term, reason]  dist.c:1747
//! DistSignal::Exit2     -> {DOP_EXIT2,       From(local), To(remote), Reason}        -> Dop::Exit2,      &[from.to_eterm(),  target_term, reason]  dist.c:1800
//! ```
//!
//! where `target_term` is the **target** [`RemotePid`] (the `To` of the link/unlink/exit
//! family, the *watched* remote process/port of the monitor family) and `from`/`watcher` is
//! the local sender, each materialised on the router's own scratch heap with
//! [`RemotePid::to_eterm`]; `ref_term` is the [`MonitorRef`] materialised with
//! [`MonitorRef::to_eterm`]. Two details a router must not get wrong: **the watcher comes
//! first** in the monitor family (`TUPLE4(DOP_MONITOR_P, watcher, watched, ref)`,
//! `dist.c:1543-1545`), and **`DOP_UNLINK_ID` carries a leading `Id`**
//! (`TUPLE4(DOP_UNLINK_ID, id, from, to)`, `dist.c:1472`) which [`DistSignal::Unlink`] does
//! **not** carry — the router supplies it. (`DOP_UNLINK` = 4 is documented as *never sent*;
//! modern ERTS unlinks by id, hence `Dop::UnlinkId`.) `DOP_EXIT` and `DOP_EXIT2` are distinct
//! opcodes with distinct receipt semantics (read-only `dist.c:2760` vs `dist.c:2824`):
//! `DOP_EXIT` is a **link** exit ("`from`, which `to` is linked to, died" →
//! `erts_dsig_send_exit`, `dist.c:1747`) and `DOP_EXIT2` is a plain exit signal from
//! `erlang:exit/2` ("`from` sent an exit signal to `to`" → `erts_dsig_send_exit2`,
//! `dist.c:1800`), so the seam keeps them separate.
//!
//! [`DistSignalRouter::route`] must serialise the signal **synchronously**: `reason` may live
//! in the signaller's heap, which the caller is free to drop or recycle as soon as `route`
//! returns. Legacy C ground truth (read-only): `erts/emulator/beam/erl_monitor_link.c` and
//! `dist.c`.
//!
//! # Inbound (receipt) primitives — the other half of the seam
//!
//! The [`DistSignalRouter`] seam itself is **outbound only** (local → remote). The *inbound*
//! half — a `DOP_*` a peer sent us — is decoded by `beam_rt` (which owns the wire) and handed
//! to [`ProcessTable`](crate::ptab::ProcessTable) as the *already-parsed* pieces
//! (`ProcessTable::record_remote_link`, `apply_remote_exit_signal`,
//! `unlink_from_remote`/`unlink_ack_from_remote`, `monitor_from_remote`/`demonitor_from_remote`,
//! `remote_monitor_down`, `set_group_leader_from_remote`).
//!
//! Those entry points are deliberately **separate** from the locally-initiated ones
//! (`link_term`, `unlink_term`, `monitor_term`, `demonitor`, `send_exit_term`): the C's inbound
//! receipt arms never echo the opcode they received (`dist.c:2258` `break; /* Done */` after a
//! successful `DOP_LINK`), whereas every locally-initiated entry point routes a `DOP_*` through
//! [`DistSignalRouter`]. Reusing them for an inbound signal would put that echo on the wire.
//! [`RemoteWatcher`] is the record of an **incoming** monitor (a remote process watching a
//! local one), the mirror of [`RemoteMonitor`].

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    external_pid_parts, external_pid_size_words, is_external_pid, is_internal_pid,
    write_external_pid, Eterm,
};

use crate::error;
use crate::monitor::{MonitorKind, MonitorRef};
use crate::pid::ProcessId;

/// A remote (external-node) process pid, decomposed into its stable parts so no boxed
/// pointer into another heap is retained.
///
/// `node` is the sysname atom term (an immediate, safe to hold long-term); `number`, `serial`
/// and `creation` are the identity fields of the external pid thing (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemotePid {
    node: Eterm,
    number: u32,
    serial: u32,
    creation: u32,
}

impl RemotePid {
    /// Decomposes an external pid term, rejecting every other term with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `term` is not an external pid.
    pub fn from_eterm(term: Eterm) -> Result<RemotePid> {
        // SAFETY: `term` is a live term held by the runtime, so if it is boxed its header word
        // is a live, aligned term word — exactly the precondition the predicate documents.
        if !unsafe { is_external_pid(term) } {
            return Err(error::bad_pid());
        }
        // SAFETY: `is_external_pid` proved the term is the live 4-word
        // `[header, node, creation, data]` block `write_external_pid` builds, so every word
        // `external_pid_parts` reads is in bounds and initialised.
        let (node, number, serial, creation) = unsafe { external_pid_parts(term) };
        Ok(RemotePid {
            node,
            number,
            serial,
            creation,
        })
    }

    /// The sysname atom term of the remote node.
    pub fn node(&self) -> Eterm {
        self.node
    }

    /// The remote process-table number.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// The remote pid serial.
    pub fn serial(&self) -> u32 {
        self.serial
    }

    /// The remote node's creation.
    pub fn creation(&self) -> u32 {
        self.creation
    }

    /// Writes the external-pid thing on `heap` and returns the tagged term.
    ///
    /// The exact inverse of [`RemotePid::from_eterm`], so a round-trip through a heap
    /// reproduces the same parts.
    ///
    /// # Errors
    ///
    /// Propagates [`beam_common::BeamError::Alloc`] when `heap` cannot fit the four-word thing.
    pub fn to_eterm(self, heap: &mut Heap) -> Result<Eterm> {
        // SAFETY: `alloc_words` returns `external_pid_size_words()` (4) exclusively-owned,
        // writable words that nothing else references.
        let p = unsafe { heap.alloc_words(external_pid_size_words())? };
        // SAFETY: `p` addresses the four words just handed out; `write_external_pid` writes
        // every one of them before the pointer escapes this block, and `self.node` is an
        // immediate/literal term owned by the runtime rather than by `heap`.
        Ok(unsafe { write_external_pid(p, self.node, self.number, self.serial, self.creation) })
    }
}

/// Whether a pid term names a local or a remote process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PidTarget {
    /// A local process in this node's process table.
    Local(ProcessId),
    /// A process on another node.
    Remote(RemotePid),
}

impl PidTarget {
    /// Classifies a pid term: an internal pid is [`PidTarget::Local`], an external pid is
    /// [`PidTarget::Remote`] and anything else is `badarg`.
    ///
    /// # Errors
    ///
    /// Returns [`beam_common::BeamError::BadArg`] when `term` is neither an internal nor an
    /// external pid.
    pub fn from_eterm(term: Eterm) -> Result<PidTarget> {
        if is_internal_pid(term) {
            Ok(PidTarget::Local(ProcessId::from_eterm(term)?))
        // SAFETY: `term` is a live term held by the runtime, so if it is boxed its header word
        // is a live, aligned term word — the predicate's documented precondition.
        } else if unsafe { is_external_pid(term) } {
            Ok(PidTarget::Remote(RemotePid::from_eterm(term)?))
        } else {
            Err(error::bad_pid())
        }
    }
}

/// An outgoing monitor this node holds on a remote process/port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteMonitor {
    /// The reference the remote `{'DOWN', …}` will carry.
    pub reference: MonitorRef,
    /// The watched remote process.
    pub remote: RemotePid,
    /// Whether a process or a port is watched.
    pub kind: MonitorKind,
}

/// An **incoming** remote monitor: a process on another node watching a **local** process or
/// port — the mirror of [`RemoteMonitor`].
///
/// It is the record an inbound `DOP_MONITOR_P` ({`DOP_MONITOR_P`, `Remote(watcher)`,
/// `Local pid-or-name(watched)`, `Ref`}, `dist.c:2341`) installs, and an inbound
/// `DOP_DEMONITOR_P` (`dist.c:2415`) removes. The record holds no term that can be relocated:
/// [`RemotePid`] is a decomposed external pid (its `node` is an immediate) and the reference is
/// a plain value, so it is not a GC root.
///
/// It lives in a side table on
/// [`ProcessTable`](crate::ptab::ProcessTable) rather than in
/// [`MonitoredBy`](crate::monitor::MonitoredBy) because `MonitoredBy`'s
/// `watcher` field is a local `ProcessId` and its shape is frozen (built by struct literal from
/// other crates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteWatcher {
    /// The reference the watcher's `{'DOWN', …}` will carry.
    pub reference: MonitorRef,
    /// The watching remote process.
    pub watcher: RemotePid,
    /// Whether a process or a port is watched.
    pub kind: MonitorKind,
}

/// A control signal the runtime must route to a remote node over the distribution protocol.
///
/// The mapping onto `beam_dist`'s `Dop` control messages is documented on the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistSignal {
    /// `erlang:link/1` on a remote pid (`DOP_LINK`).
    Link {
        /// The local process establishing the link.
        from: ProcessId,
        /// The remote link partner.
        to: RemotePid,
    },
    /// `erlang:unlink/1` on a remote pid (`DOP_UNLINK_ID`).
    Unlink {
        /// The local process dropping the link.
        from: ProcessId,
        /// The remote link partner.
        to: RemotePid,
    },
    /// `erlang:monitor/2` on a remote process/port (`DOP_MONITOR_P`).
    Monitor {
        /// The local watching process.
        watcher: ProcessId,
        /// The watched remote process/port.
        target: RemotePid,
        /// The reference identifying the monitor.
        reference: MonitorRef,
        /// Whether a process or a port is watched.
        kind: MonitorKind,
    },
    /// `erlang:demonitor/1` of a remote monitor (`DOP_DEMONITOR_P`).
    Demonitor {
        /// The local watching process.
        watcher: ProcessId,
        /// The watched remote process/port.
        target: RemotePid,
        /// The reference identifying the monitor.
        reference: MonitorRef,
    },
    /// A **link** exit: a local process linked to `to` has died, so `from` is no longer linked
    /// to it (`DOP_EXIT` → `{DOP_EXIT, From, To, Reason}` → `Dop::Exit`).
    ///
    /// The receiver interprets this as "the process I am linked to went away". Legacy C ground
    /// truth: `erts_dsig_send_exit` (`dist.c:1747`) / the `DOP_EXIT` receipt arm (`dist.c:2760`).
    Exit {
        /// The local process that died (the link partner of `to`).
        from: ProcessId,
        /// The remote link partner.
        to: RemotePid,
        /// The exit reason term (may live in the signaller's heap — serialise it synchronously).
        reason: Eterm,
    },
    /// `erlang:exit/2` towards a remote pid: `to` is sent a plain exit signal from `from`
    /// (`DOP_EXIT2` → `{DOP_EXIT2, From, To, Reason}` → `Dop::Exit2`).
    ///
    /// Distinct from [`DistSignal::Exit`] because the receiver must not treat it as a link exit.
    /// Legacy C ground truth: `erts_dsig_send_exit2` (`dist.c:1800`) / the `DOP_EXIT2` receipt
    /// arm (`dist.c:2824`).
    Exit2 {
        /// The local signaller (`erlang:exit(To, Reason)`'s caller).
        from: ProcessId,
        /// The remote recipient.
        to: RemotePid,
        /// The exit reason term (may live in the signaller's heap — serialise it synchronously).
        reason: Eterm,
    },
}

/// The distribution seam: implemented downstream (over `beam_dist::Connection`) so this
/// crate does not depend on `beam_dist`.
///
/// `route` MUST serialise the signal synchronously — `reason` may live in the sender's heap,
/// which the caller may drop/recycle right after the call returns.
pub trait DistSignalRouter {
    /// Serialises and sends one control signal to the target's remote node.
    ///
    /// # Errors
    ///
    /// Propagates the transport's failure (an unknown/unconnected node, an I/O error).
    fn route(&mut self, signal: DistSignal) -> Result<()>;
}
