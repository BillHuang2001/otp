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
//! a runtime with no local process registry never *mis*-delivers a message. An
//! opcode this crate recognises but does not classify for delivery (e.g. the
//! distributed-spawn family) is reported as [`Delivery::Other`], never silently
//! treated as a send.
//!
//! Ground truth (read-only, never modified): `erts/emulator/beam/dist.c` —
//! `erts_dist_command` and the per-opcode arity/argument checks cited below.

use beam_common::{BeamError, Result};
use beam_term::Eterm;

use crate::control::ControlMessage;
use crate::flags::Dop;

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
    /// A `DOP_*` this crate recognises on the wire but does not classify for
    /// local delivery (the distributed-spawn family, the payload-exit forms, the
    /// alternative-activation signal, `DOP_UNLINK`). The runtime may still inspect
    /// it via [`crate::control::ControlMessage`].
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
            Delivery::Other(dop) => *dop,
        }
    }

    /// Classifies a decoded [`ControlMessage`] into a [`Delivery`].
    ///
    /// The argument order/keying follows `erts_dist_command` (the citations on each
    /// variant). [`decode_control`](crate::control::decode_control) has already
    /// validated the arity, so a missing argument here means a message-carrying
    /// opcode arrived **without** its trailing message term (the decoder tolerates
    /// that control-only form) — reported as [`BeamError::BadArg`], never a
    /// substitute term.
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
            // Everything else is recognised on the wire but not classified here.
            Dop::Unlink
            | Dop::PayloadExit
            | Dop::PayloadExitTt
            | Dop::PayloadExit2
            | Dop::PayloadExit2Tt
            | Dop::PayloadMonitorPExit
            | Dop::SpawnRequest
            | Dop::SpawnRequestTt
            | Dop::SpawnReply
            | Dop::SpawnReplyTt
            | Dop::AltactSigSend => Delivery::Other(message.dop),
        };
        Ok(delivery)
    }
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
