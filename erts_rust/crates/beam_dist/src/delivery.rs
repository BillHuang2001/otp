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

//! Inbound `DOP_*` **delivery**: classify a decoded control message into a
//! semantic [`Delivery`] and route it through the caller-supplied [`InboundSink`]
//! seam.
//!
//! The blocking [`crate::connection::Connection`] and the non-blocking
//! [`crate::driver::ConnectionDriver`] decode an inbound distribution control
//! message into a [`ControlMessage`] — the opcode plus its raw term arguments.
//! That is the *protocol* view. A runtime that wants the *semantics* ("deliver
//! this message to the local process `to`", "`from` links with `to`", "a monitor
//! fired") needs the message classified: which term is the target, which is the
//! sender, which is the payload.
//!
//! This module owns that classification ([`Delivery`]) and the seam through which
//! a runtime routes it ([`InboundSink`]).
//!
//! # Where the routing lives (and where it must not)
//!
//! This crate is the *value/protocol* layer (contract A.5, `beam_dist`); it must
//! **not** depend on the message/mailbox/process crates (`beam_message`,
//! `beam_process`) — that would break the crate-layer dependency DAG
//! (`../CONTEXT.md` §"Layering & dependency waves", where `beam_dist` is wave 2 and
//! `beam_process` is wave 3). Delivery is therefore a **defaulted trait on this
//! side**: [`InboundSink`] receives a classified [`Delivery`] and reports what it
//! did. The production implementation belongs to `beam_rt`, which supplies it over
//! the process table + mailbox seam (`beam_process::ProcessTable::deliver` /
//! `beam_engine::Mailbox`). See the crate `CONTEXT.md`.
//!
//! # Honesty
//!
//! The default [`InboundSink::deliver`] reports [`DeliveryOutcome::Unsupported`] —
//! a runtime with no local process registry never *mis*-delivers a message. Every
//! one of the 30 v6 opcodes is classified into a typed [`Delivery`] *except*
//! `DOP_UNLINK`, which the C never sends and rejects outright and which is
//! therefore reported as [`Delivery::Other`] rather than guessed at.
//!
//! Ground truth (read-only, never modified): `erts/emulator/beam/dist.c` —
//! `erts_dist_command` and the per-opcode arity/argument checks cited below.
//!
//! # What is *not* a control opcode in v6
//!
//! `enum dop` (`erts/emulator/beam/dist.h:140-183`) holds exactly the 30 opcodes
//! [`Dop`] models — the surface classified here is therefore *complete*.
//!
//! * **There is no `DOP_NODEUP`/`DOP_NODEDOWN`.** Node up/down are *local emulator
//!   events*: `send_nodes_mon_msgs` (`dist.c:6931`) queues them into local
//!   processes from the connection-teardown path (`dist.c:864`) and the `nodeup`
//!   path (`dist.c:5063`, `dist.c:5475`), and `net_kernel:monitor_nodes/1,2` is
//!   served by the local emulator (a `nodedown` is also synthesised locally for a
//!   failed auto-connect, `dist.c:6593`). Nothing on the wire carries them.
//! * **There is no `DOP_DEMONITOR_ID`, `DOP_UNLINK_MONITOR_ID` or
//!   `DOP_DEMONITOR_MONITOR_ID`.** Monitors are cancelled with `DOP_DEMONITOR_P`
//!   (`dist.c:2415`) and the OTP-26+ link protocol uses `DOP_UNLINK_ID` /
//!   `DOP_UNLINK_ID_ACK` (`dist.c:2284`, `dist.c:2315`); no other opcode numbers
//!   are assigned.
//! * **`DOP_UNLINK` (`4`) is never sent.** The new (OTP 26+) link protocol is
//!   mandatory, so the C *rejects* it outright — `goto invalid_message`
//!   (`dist.c:2277-2282`). It stays recognised on the wire and classified as
//!   [`Delivery::Other`] precisely because this crate must not guess at a
//!   semantics the C refuses to give it.
//! * Byte `5` is the pre-OTP ancient `DOP_NODE_LINK`, explicitly reusable
//!   (`dist.h:147`); [`Dop::from_u8`](crate::flags::Dop::from_u8) rejects it and
//!   every other unassigned byte (`0`, `9`-`11`, `14`-`15`, `17`, `>= 38`).
//!
//! `beam_codec`'s `LOCAL_EXT` (`121`) is likewise *not* a control message: it is
//! a `dist.c`-level same-node delivery short-circuit wrapper around an ETF body,
//! not a `DOP_*` opcode.

use beam_common::{BeamError, Result};
use beam_term::prelude::{is_small, signed_value};
use beam_term::Eterm;

use crate::control::ControlMessage;
use crate::flags::{AltactSigFlags, DistSpawnFlags, Dop};

/// A decoded inbound distribution control message, classified for local delivery.
///
/// Every field is the *raw* term image the peer sent (allocated on the caller's
/// heap); this crate does not resolve a pid to a local process or a registered
/// name to a mailbox — that is the [`InboundSink`]'s job. The field names follow
/// the argument order documented in `dist.c` (see each variant).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Delivery {
    /// `DOP_SEND` / `DOP_SEND_TT` / `DOP_SEND_SENDER` / `DOP_SEND_SENDER_TT`:
    /// deliver `message` to the local process `to`.
    ///
    /// `from` is the sending pid when the peer negotiated `DFLAG_SEND_SENDER`
    /// (`DOP_SEND_SENDER*`); for the plain `DOP_SEND*` forms it is the empty atom
    /// `''` (`dist.c:2535-2542`: *"If DOP_SEND_SENDER … element 2 contains the
    /// sender pid … otherwise, the atom ''"*). `token` is the trace token of a
    /// `_TT` opcode.
    Send {
        /// The sending process (a pid, or the empty atom when not sent).
        from: Eterm,
        /// The destination local process.
        to: Eterm,
        /// The message term.
        message: Eterm,
        /// The trace token for the `_TT` forms.
        token: Option<Eterm>,
    },
    /// `DOP_REG_SEND` / `DOP_REG_SEND_TT`: deliver `message` to the locally
    /// registered name `name` (`dist.c:2468`: `{DOP_REG_SEND, From, Cookie,
    /// ToName} -- Message`). `cookie` is always the empty atom since R9B
    /// (`dist.c:2472-2478`).
    RegSend {
        /// The sending process.
        from: Eterm,
        /// The (always-empty) cookie atom.
        cookie: Eterm,
        /// The registered name the message is addressed to.
        name: Eterm,
        /// The message term.
        message: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_ALIAS_SEND` / `DOP_ALIAS_SEND_TT`: deliver `message` to the local
    /// process alias `to` (`dist.c:2563`).
    AliasSend {
        /// The sending process.
        from: Eterm,
        /// The destination process alias (a reference).
        to: Eterm,
        /// The message term.
        message: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_LINK` (`dist.c:2224`: `{DOP_LINK, From, To}`): link the local process
    /// `to` with the remote `from`.
    Link {
        /// The remote process.
        from: Eterm,
        /// The local process.
        to: Eterm,
    },
    /// `DOP_EXIT` / `DOP_EXIT_TT` (`dist.c:2760`/`2773`): an (trappable) exit
    /// signal from the remote `from` to the local `to`.
    Exit {
        /// The remote process whose exit is signalled.
        from: Eterm,
        /// The local destination process.
        to: Eterm,
        /// The exit reason.
        reason: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_EXIT2` / `DOP_EXIT2_TT` (`dist.c:2824`/`2837`): an **untrappable**
    /// exit signal (`exit/2`) from the remote `from` to the local `to`.
    Exit2 {
        /// The remote process sending the signal.
        from: Eterm,
        /// The local destination process.
        to: Eterm,
        /// The exit reason.
        reason: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_UNLINK_ID` (`dist.c:2284`: `{DOP_UNLINK_ID, Id, From, To}`): the
    /// unlink id handshake (OTP 26+ link protocol).
    UnlinkId {
        /// The unlink id (`Id`).
        id: Eterm,
        /// The remote process.
        from: Eterm,
        /// The local process.
        to: Eterm,
    },
    /// `DOP_UNLINK_ID_ACK` (`dist.c:2315`: `{DOP_UNLINK_ID_ACK, Id, From, To}`).
    UnlinkIdAck {
        /// The unlink id (`Id`).
        id: Eterm,
        /// The remote process.
        from: Eterm,
        /// The local process.
        to: Eterm,
    },
    /// `DOP_MONITOR_P` (`dist.c:2341`: `{DOP_MONITOR_P, Remote, Local, Ref}`): the
    /// remote `watcher` monitors the local `watched` (a pid or a registered name).
    Monitor {
        /// The remote monitoring process.
        watcher: Eterm,
        /// The local process (pid) or registered name being monitored.
        watched: Eterm,
        /// The monitor reference.
        reference: Eterm,
    },
    /// `DOP_DEMONITOR_P` (`dist.c:2415`): cancels the monitor `watcher` (remote)
    /// holds on `watched` (local).
    Demonitor {
        /// The remote monitoring process.
        watcher: Eterm,
        /// The local process or name.
        watched: Eterm,
        /// The monitor reference.
        reference: Eterm,
    },
    /// `DOP_MONITOR_P_EXIT` (`dist.c:2700`: `{DOP_MONITOR_P_EXIT, Remote, Local,
    /// Ref, Reason}`): a process the local `watcher` monitors on the remote node
    /// died.
    MonitorExit {
        /// The remote process (or name) that died.
        watched: Eterm,
        /// The local monitoring process.
        watcher: Eterm,
        /// The monitor reference.
        reference: Eterm,
        /// The exit reason.
        reason: Eterm,
    },
    /// `DOP_GROUP_LEADER` (`dist.c:2889`: `{DOP_GROUP_LEADER, From, To}`): `from`
    /// becomes the group leader of the local `to`.
    GroupLeader {
        /// The (remote) new group leader.
        from: Eterm,
        /// The local process to re-parent.
        to: Eterm,
    },
    /// `DOP_PAYLOAD_EXIT` / `DOP_PAYLOAD_EXIT_TT` (`dist.c:2757-2791`): like
    /// [`Delivery::Exit`], but the exit reason travels as a **payload** term after
    /// the control tuple because it is too large to inline.
    ///
    /// This is the `DFLAG_EXIT_PAYLOAD` form: the sender puts the reason in
    /// `ctx->msg` and builds `{DOP_PAYLOAD_EXIT, From, To}` (or
    /// `{DOP_PAYLOAD_EXIT_TT, From, To, Token}`) as the control tuple
    /// (`dist.c:1744-1745`). `payload` is that trailing term.
    PayloadExit {
        /// The remote process whose exit is signalled.
        from: Eterm,
        /// The local destination process.
        to: Eterm,
        /// The exit reason, carried as the trailing payload term.
        payload: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_PAYLOAD_EXIT2` / `DOP_PAYLOAD_EXIT2_TT` (`dist.c:2821-2856`): the
    /// **untrappable** (`exit/2`) payload-exit form — [`Delivery::PayloadExit`]
    /// with the `DOP_EXIT2` semantics.
    PayloadExit2 {
        /// The remote process sending the signal.
        from: Eterm,
        /// The local destination process.
        to: Eterm,
        /// The exit reason, carried as the trailing payload term.
        payload: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_PAYLOAD_MONITOR_P_EXIT` (`dist.c:2700-2714`): like
    /// [`Delivery::MonitorExit`], but the down reason travels as a **payload**
    /// term after the control tuple (`{DOP_PAYLOAD_MONITOR_P_EXIT, Remote, Local,
    /// Ref}`; the sender sets `ctx->msg = reason`, `dist.c:1511-1514`).
    PayloadMonitorExit {
        /// The remote process (or name) that died.
        watched: Eterm,
        /// The local monitoring process.
        watcher: Eterm,
        /// The monitor reference.
        reference: Eterm,
        /// The exit reason, carried as the trailing payload term.
        payload: Eterm,
    },
    /// `DOP_SPAWN_REQUEST` / `DOP_SPAWN_REQUEST_TT` (`dist.c:2902-2931`): a remote
    /// node asks this node to spawn a process.
    ///
    /// The control tuple is `{op, Ref, From, GL, MFA, Opts}` — `Ref` is the
    /// external spawn reference (or the already-known `pid`), `From` the requesting
    /// process, `GL` its group leader, `MFA` a `{Module, Function, Arity}` tuple and
    /// `Opts` the spawn options — and the **argument list** rides as the trailing
    /// term (`ctx->msg = alist`, `dist.c:1877`). `token` is present only for the
    /// `_TT` form. Note that this opcode carries **no** flag field: the
    /// link/monitor intent is answered in [`Delivery::SpawnReply`].
    SpawnRequest {
        /// The external spawn reference.
        reference: Eterm,
        /// The requesting (remote) process.
        from: Eterm,
        /// The requester's group leader.
        group_leader: Eterm,
        /// The `{Module, Function, Arity}` to spawn.
        mfa: Eterm,
        /// The spawn options list.
        opts: Eterm,
        /// The argument list, carried as the trailing term.
        args: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_SPAWN_REPLY` / `DOP_SPAWN_REPLY_TT` (`dist.c:3033-3093`): the answer to
    /// a [`Delivery::SpawnRequest`].
    ///
    /// The control tuple is `{op, Ref, To, Flags, From}`: `To` is the *parent* the
    /// reply is addressed to (`parent` in the C) and `From` is the *result* — the
    /// newly spawned remote pid, or an error atom (`result` in the C). `flags` is
    /// the `ERTS_DIST_SPAWN_FLAG_*` word; the C rejects a non-small integer and any
    /// value `>= 1 << 27` (`dist.c:3088-3093`), and so does this classifier. There
    /// is **no** trailing term.
    SpawnReply {
        /// The external spawn reference from the request.
        reference: Eterm,
        /// The local parent process the reply is addressed to.
        to: Eterm,
        /// The `ERTS_DIST_SPAWN_FLAG_*` word.
        flags: DistSpawnFlags,
        /// The spawned (remote) pid, or an error atom.
        result: Eterm,
        /// The trace token for the `_TT` form.
        token: Option<Eterm>,
    },
    /// `DOP_ALTACT_SIG_SEND` (`dist.c:2590-2697`): an *alternative activation*
    /// signal. Which of the two signals it is, and how `to` is addressed, is
    /// carried in `flags`:
    ///
    /// * `ERTS_DOP_ALTACT_SIG_FLG_EXIT` — an **exit** signal, so the trailing term
    ///   is the exit reason (`erts_proc_sig_send_dist_exit`, `dist.c:2689-2692`);
    ///   otherwise it is a **message** signal (`dist.c:2693-2696`).
    /// * `ERTS_DOP_ALTACT_SIG_FLG_ALIAS` — `to` is a reference (an alias);
    ///   `..._NAME` — `to` is an atom (a registered name); neither — `to` is a pid
    ///   (`dist.c:2662-2685`).
    /// * `ERTS_DOP_ALTACT_SIG_FLG_PRIO` — a priority signal.
    /// * `ERTS_DOP_ALTACT_SIG_FLG_TOKEN` — the control tuple grows one element
    ///   (the token): `{op, Flags, SenderPid, To, Token}` instead of
    ///   `{op, Flags, SenderPid, To}` (`dist.c:2651-2658`). There is **no** separate
    ///   `DOP_ALTACT_SIG_SEND_TT` opcode in v6 — the flag *is* the `_TT` marker —
    ///   so the token is reported here rather than as a different
    ///   [`Delivery::dop`].
    ///
    /// The flags themselves are *not* interpreted here beyond reading the token
    /// bit: this crate resolves neither names nor aliases (that is the
    /// [`InboundSink`]'s job).
    AltactSigSend {
        /// The `ERTS_DOP_ALTACT_SIG_FLG_*` word.
        flags: AltactSigFlags,
        /// The sending pid (`SenderPid`).
        from: Eterm,
        /// The target: an alias reference, a registered name, or a pid.
        to: Eterm,
        /// The trace token, present iff [`AltactSigFlags::TOKEN`] is set.
        token: Option<Eterm>,
        /// The trailing signal term: the message, or the exit reason when
        /// [`AltactSigFlags::EXIT`] is set.
        message: Eterm,
    },
    /// A `DOP_*` this crate recognises on the wire but does not classify for
    /// local delivery. Today that is **only** [`Dop::Unlink`], which the C never
    /// sends and rejects outright (see the module docs). The runtime may still
    /// inspect it via [`crate::control::ControlMessage`].
    Other(Dop),
}

impl Delivery {
    /// The `DOP_*` opcode this delivery was decoded from.
    pub fn dop(&self) -> Dop {
        match self {
            Delivery::Send { .. } => Dop::Send,
            Delivery::RegSend { .. } => Dop::RegSend,
            Delivery::AliasSend { .. } => Dop::AliasSend,
            Delivery::Link { .. } => Dop::Link,
            Delivery::Exit { .. } => Dop::Exit,
            Delivery::Exit2 { .. } => Dop::Exit2,
            Delivery::UnlinkId { .. } => Dop::UnlinkId,
            Delivery::UnlinkIdAck { .. } => Dop::UnlinkIdAck,
            Delivery::Monitor { .. } => Dop::MonitorP,
            Delivery::Demonitor { .. } => Dop::DemonitorP,
            Delivery::MonitorExit { .. } => Dop::MonitorPExit,
            Delivery::GroupLeader { .. } => Dop::GroupLeader,
            Delivery::PayloadExit { .. } => Dop::PayloadExit,
            Delivery::PayloadExit2 { .. } => Dop::PayloadExit2,
            Delivery::PayloadMonitorExit { .. } => Dop::PayloadMonitorPExit,
            Delivery::SpawnRequest { .. } => Dop::SpawnRequest,
            Delivery::SpawnReply { .. } => Dop::SpawnReply,
            Delivery::AltactSigSend { .. } => Dop::AltactSigSend,
            Delivery::Other(dop) => *dop,
        }
    }

    /// Classifies a decoded [`ControlMessage`] into a [`Delivery`].
    ///
    /// The argument order/keying follows `erts_dist_command` (the citations on each
    /// variant). [`decode_control`](crate::control::decode_control) has already
    /// validated the arity, so a missing argument here means an opcode that
    /// [`carries_message`](crate::control::carries_message) arrived **without** its
    /// trailing term (the decoder tolerates that control-only form) — reported as
    /// [`BeamError::BadArg`], never a substituted term.
    pub fn classify(message: &ControlMessage) -> Result<Delivery> {
        let terms = &message.terms;
        // A defensive accessor: arity was validated by `decode_control`, so a miss
        // is the control-only `send` form (no trailing message term).
        let term = |index: usize| terms.get(index).copied().ok_or(BeamError::BadArg);

        let delivery = match message.dop {
            // `/* {DOP_SEND, From, To} -- Message */` (`dist.c:2527`).
            Dop::Send | Dop::SendSender => Delivery::Send {
                from: term(0)?,
                to: term(1)?,
                message: term(2)?,
                token: None,
            },
            // `/* {DOP_SEND_TT, From, To, TraceToken} -- Message */` (`dist.c:2513`).
            Dop::SendTt | Dop::SendSenderTt => Delivery::Send {
                from: term(0)?,
                to: term(1)?,
                token: Some(term(2)?),
                message: term(3)?,
            },
            // `/* {DOP_REG_SEND, From, Cookie, ToName} -- Message */` (`dist.c:2468`).
            Dop::RegSend => Delivery::RegSend {
                from: term(0)?,
                cookie: term(1)?,
                name: term(2)?,
                message: term(3)?,
                token: None,
            },
            // `/* {DOP_REG_SEND_TT, From, Cookie, ToName, TraceToken} -- Message */`
            // (`dist.c:2470`).
            Dop::RegSendTt => Delivery::RegSend {
                from: term(0)?,
                cookie: term(1)?,
                name: term(2)?,
                token: Some(term(3)?),
                message: term(4)?,
            },
            // `/* {DOP_ALIAS_SEND, From, To} -- Message */` (`dist.c:2563`).
            Dop::AliasSend => Delivery::AliasSend {
                from: term(0)?,
                to: term(1)?,
                message: term(2)?,
                token: None,
            },
            Dop::AliasSendTt => Delivery::AliasSend {
                from: term(0)?,
                to: term(1)?,
                token: Some(term(2)?),
                message: term(3)?,
            },
            // `{DOP_LINK, From, To}` (`dist.c:2224`).
            Dop::Link => Delivery::Link {
                from: term(0)?,
                to: term(1)?,
            },
            // `{DOP_EXIT, From, To, Reason}` (`dist.c:2760`).
            Dop::Exit => Delivery::Exit {
                from: term(0)?,
                to: term(1)?,
                reason: term(2)?,
                token: None,
            },
            // `{DOP_EXIT_TT, From, To, TraceToken, Reason}` (`dist.c:2773-2779`).
            Dop::ExitTt => Delivery::Exit {
                from: term(0)?,
                to: term(1)?,
                token: Some(term(2)?),
                reason: term(3)?,
            },
            // `{DOP_EXIT2, From, To, Reason}` (`dist.c:2824`).
            Dop::Exit2 => Delivery::Exit2 {
                from: term(0)?,
                to: term(1)?,
                reason: term(2)?,
                token: None,
            },
            // `{DOP_EXIT2_TT, From, To, TraceToken, Reason}` (`dist.c:2837`).
            Dop::Exit2Tt => Delivery::Exit2 {
                from: term(0)?,
                to: term(1)?,
                token: Some(term(2)?),
                reason: term(3)?,
            },
            // `{DOP_UNLINK_ID, Id, From, To}` (`dist.c:2284`).
            Dop::UnlinkId => Delivery::UnlinkId {
                id: term(0)?,
                from: term(1)?,
                to: term(2)?,
            },
            // `{DOP_UNLINK_ID_ACK, Id, From, To}` (`dist.c:2315`).
            Dop::UnlinkIdAck => Delivery::UnlinkIdAck {
                id: term(0)?,
                from: term(1)?,
                to: term(2)?,
            },
            // `{DOP_MONITOR_P, Remote, Local, Ref}` (`dist.c:2341`).
            Dop::MonitorP => Delivery::Monitor {
                watcher: term(0)?,
                watched: term(1)?,
                reference: term(2)?,
            },
            // `{DOP_DEMONITOR_P, Remote, Local, Ref}` (`dist.c:2415`).
            Dop::DemonitorP => Delivery::Demonitor {
                watcher: term(0)?,
                watched: term(1)?,
                reference: term(2)?,
            },
            // `{DOP_MONITOR_P_EXIT, Remote, Local, Ref, Reason}` (`dist.c:2700`).
            Dop::MonitorPExit => Delivery::MonitorExit {
                watched: term(0)?,
                watcher: term(1)?,
                reference: term(2)?,
                reason: term(3)?,
            },
            // `{DOP_GROUP_LEADER, From, To}` (`dist.c:2889`).
            Dop::GroupLeader => Delivery::GroupLeader {
                from: term(0)?,
                to: term(1)?,
            },
            // `{DOP_PAYLOAD_EXIT, From, To} -- Reason` (`dist.c:2757-2785`).
            Dop::PayloadExit => Delivery::PayloadExit {
                from: term(0)?,
                to: term(1)?,
                payload: term(2)?,
                token: None,
            },
            // `{DOP_PAYLOAD_EXIT_TT, From, To, TraceToken} -- Reason`
            // (`dist.c:2786-2791`).
            Dop::PayloadExitTt => Delivery::PayloadExit {
                from: term(0)?,
                to: term(1)?,
                token: Some(term(2)?),
                payload: term(3)?,
            },
            // `{DOP_PAYLOAD_EXIT2, From, To} -- Reason` (`dist.c:2821-2849`).
            Dop::PayloadExit2 => Delivery::PayloadExit2 {
                from: term(0)?,
                to: term(1)?,
                payload: term(2)?,
                token: None,
            },
            // `{DOP_PAYLOAD_EXIT2_TT, From, To, TraceToken} -- Reason`
            // (`dist.c:2850-2856`).
            Dop::PayloadExit2Tt => Delivery::PayloadExit2 {
                from: term(0)?,
                to: term(1)?,
                token: Some(term(2)?),
                payload: term(3)?,
            },
            // `{DOP_PAYLOAD_MONITOR_P_EXIT, Remote, Local, Ref} -- Reason`
            // (`dist.c:2700-2714`).
            Dop::PayloadMonitorPExit => Delivery::PayloadMonitorExit {
                watched: term(0)?,
                watcher: term(1)?,
                reference: term(2)?,
                payload: term(3)?,
            },
            // `{DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts} -- Args`
            // (`dist.c:2919-2931`).
            Dop::SpawnRequest => Delivery::SpawnRequest {
                reference: term(0)?,
                from: term(1)?,
                group_leader: term(2)?,
                mfa: term(3)?,
                opts: term(4)?,
                args: term(5)?,
                token: None,
            },
            // `{DOP_SPAWN_REQUEST_TT, Ref, From, GL, MFA, Opts, TraceToken} -- Args`
            // (`dist.c:2902-2916`).
            Dop::SpawnRequestTt => Delivery::SpawnRequest {
                reference: term(0)?,
                from: term(1)?,
                group_leader: term(2)?,
                mfa: term(3)?,
                opts: term(4)?,
                args: term(6)?,
                token: Some(term(5)?),
            },
            // `{DOP_SPAWN_REPLY, Ref, To, Flags, From}` (`dist.c:3051-3068`).
            Dop::SpawnReply => Delivery::SpawnReply {
                reference: term(0)?,
                to: term(1)?,
                flags: spawn_flags(term(2)?)?,
                result: term(3)?,
                token: None,
            },
            // `{DOP_SPAWN_REPLY_TT, Ref, To, Flags, From, TraceToken}`
            // (`dist.c:3033-3045`).
            Dop::SpawnReplyTt => Delivery::SpawnReply {
                reference: term(0)?,
                to: term(1)?,
                flags: spawn_flags(term(2)?)?,
                result: term(3)?,
                token: Some(term(4)?),
            },
            // `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To[, TraceToken]} -- Signal`
            // (`dist.c:2590-2658`). The token element is present iff its flag bit
            // is set, so the offsets shift by one between the two forms.
            Dop::AltactSigSend => {
                let flags = altact_flags(term(0)?)?;
                let has_token = flags.contains(AltactSigFlags::TOKEN);
                let expected = if has_token { 5 } else { 4 };
                if terms.len() != expected {
                    // Either a token-less tuple that nonetheless grew an element, or
                    // a `TOKEN`-flagged tuple that is too short (`dist.c:2654-2656`).
                    return Err(BeamError::BadArg);
                }
                let (token, message) =
                    if has_token { (Some(term(3)?), term(4)?) } else { (None, term(3)?) };
                Delivery::AltactSigSend {
                    flags,
                    from: term(1)?,
                    to: term(2)?,
                    token,
                    message,
                }
            }
            // `DOP_UNLINK` is the only v6 opcode with no delivery semantics: the C
            // rejects it outright because the new link protocol is mandatory
            // (`dist.c:2277-2282`). Recognised, never guessed at.
            Dop::Unlink => Delivery::Other(Dop::Unlink),
        };
        Ok(delivery)
    }
}

/// Reads the `Flags` field of `DOP_SPAWN_REPLY` as an [`DistSpawnFlags`] word.
///
/// The C requires a **small integer** and rejects any value `>= 1 << 27`
/// (`dist.c:3088-3093`); those two conditions are enforced here as
/// [`BeamError::BadArg`], so a malformed flags field can never be mistaken for a
/// legitimately empty one.
fn spawn_flags(flags: Eterm) -> Result<DistSpawnFlags> {
    if !is_small(flags) {
        return Err(BeamError::BadArg);
    }
    let value = signed_value(flags);
    if !(0..(1i64 << 27)).contains(&value) {
        return Err(BeamError::BadArg);
    }
    Ok(DistSpawnFlags::from_bits(value as u32))
}

/// Reads the `Flags` field of `DOP_ALTACT_SIG_SEND` as an [`AltactSigFlags`] word.
///
/// The C accepts a small integer or a bignum and rejects a negative one
/// (`dist.c:2628-2644`), keeping only the **low byte** of the value
/// (`flags = (byte) (val & 0xff)`, `dist.c:2632`/`dist.c:2639`). Only the five low
/// bits are defined (`ERTS_DOP_ALTACT_SIG_FLG_*`, `dist.h:188-192`), so the mask
/// loses nothing the protocol assigns. A **bignum** flags field — which the C
/// tolerates by reading that same low byte — is reported as [`BeamError::BadArg`]
/// rather than guessed at: a typed error is honest, a mis-read flag word would not
/// be.
fn altact_flags(flags: Eterm) -> Result<AltactSigFlags> {
    if !is_small(flags) {
        return Err(BeamError::BadArg);
    }
    let value = signed_value(flags);
    if value < 0 {
        return Err(BeamError::BadArg);
    }
    Ok(AltactSigFlags::from_bits((value as u64 & 0xff) as u32))
}

/// What an [`InboundSink`] did with a [`Delivery`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeliveryOutcome {
    /// The sink routed the delivery to its local recipient (a mailbox enqueue, a
    /// link install, a monitor fire, …).
    Delivered,
    /// The addressed local recipient does not exist; the runtime should answer the
    /// peer with the appropriate signal (e.g. `noproc`/`badarg`).
    NoSuchProcess,
    /// The sink recognised the delivery but cannot route it (a typed, honest
    /// "unsupported" — e.g. no process registry, or an opcode it does not handle).
    Unsupported,
    /// The delivery was deliberately dropped (e.g. addressed to an old incarnation
    /// of this node, or a monitor for a process that has already exited).
    Dropped,
}

/// One dispatched inbound control message: the classified [`Delivery`] and the
/// [`DeliveryOutcome`] the sink reported for it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dispatched {
    /// The classified inbound delivery.
    pub delivery: Delivery,
    /// What the sink did with it.
    pub outcome: DeliveryOutcome,
}

/// The runtime's **inbound-distribution seam**: routes a classified [`Delivery`]
/// to its local recipient.
///
/// This crate (the value/protocol layer) cannot deliver to a mailbox itself — it
/// must not depend on the process/message crates. Instead the runtime supplies an
/// implementation: `beam_rt` implements this over its process table and mailbox
/// (`beam_process::ProcessTable::deliver` for a pid/alias target;
/// `beam_process::ProcessTable::whereis` + `deliver` for a registered name; the
/// link/monitor/exit signal paths for the signal opcodes).
///
/// The default implementation reports [`DeliveryOutcome::Unsupported`] for every
/// delivery, so a caller that has not wired a runtime never gets a *wrong* result
/// — it gets an honest "not routed".
pub trait InboundSink {
    /// Routes one classified inbound delivery.
    ///
    /// # Errors
    ///
    /// A sink may return [`Err`] for an internal failure; the driver/connection
    /// dispatch propagates it to the caller without altering the link state.
    fn deliver(&mut self, delivery: &Delivery) -> Result<DeliveryOutcome> {
        let _ = delivery;
        Ok(DeliveryOutcome::Unsupported)
    }
}

/// The trivial [`InboundSink`] that does nothing (every delivery is
/// [`DeliveryOutcome::Unsupported`]).
///
/// Useful as a default/placeholder and in tests: it exercises classification
/// without pretending to route.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedSink;

impl InboundSink for UnsupportedSink {}
