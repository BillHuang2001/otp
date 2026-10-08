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

//! The wire-frozen distribution vocabulary: capability bits (`DFLAG_*`), control
//! opcodes (`DOP_*`), the handshake/`epmd` tag bytes and the version constants.
//!
//! Every value here is taken verbatim from the legacy C ground truth and is
//! **wire-frozen** (contract A.5): the capability word is negotiated bit-for-bit
//! in the connect handshake and the control opcodes are the first byte of every
//! `DOP_*` control message. Never renumber or re-order them.
//!
//! Ground truth (read-only):
//! - `erts/emulator/beam/dist.h` — `DFLAG_*` capability bits, the derived
//!   mandatory/hopefully/default/addable/rejectable sets and `enum dop`.
//! - `lib/erl_interface/src/connect/ei_connect_int.h` — the OTP 23+ handshake
//!   version (`EI_DIST_6`) and the handshake tags.
//! - `lib/erl_interface/src/epmd/ei_epmd.h` + `erts/epmd/src/epmd.h` +
//!   `erts/epmd/src/epmd_int.h` — the `epmd` tags, node-type bytes (`77 = 'M'`
//!   normal Erlang node, `72 = 'H'` hidden/c-node) and `EI_MYPROTO` (`0` =
//!   tcp/ip).

// ---------------------------------------------------------------------------
// Capability bits (`DFLAG_*`, dist.h:30-70).
// ---------------------------------------------------------------------------

/// `DFLAG_PUBLISHED` — a published (registered) node.
pub const DFLAG_PUBLISHED: u64 = 0x01;
/// `DFLAG_ATOM_CACHE` — the distribution atom cache is enabled.
pub const DFLAG_ATOM_CACHE: u64 = 0x02;
/// `DFLAG_EXTENDED_REFERENCES` — `NEWER_REFERENCE_EXT` references.
pub const DFLAG_EXTENDED_REFERENCES: u64 = 0x04;
/// `DFLAG_DIST_MONITOR` — distributed `MONITOR_P`/`DEMONITOR_P`.
pub const DFLAG_DIST_MONITOR: u64 = 0x08;
/// `DFLAG_FUN_TAGS` — `NEW_FUN_EXT`/`EXPORT_EXT` funs.
pub const DFLAG_FUN_TAGS: u64 = 0x10;
/// `DFLAG_DIST_MONITOR_NAME` — monitor by registered name.
pub const DFLAG_DIST_MONITOR_NAME: u64 = 0x20;
/// `DFLAG_HIDDEN_ATOM_CACHE` — the (legacy) hidden atom cache.
pub const DFLAG_HIDDEN_ATOM_CACHE: u64 = 0x40;
/// `DFLAG_NEW_FUN_TAGS` — the newer fun tags.
pub const DFLAG_NEW_FUN_TAGS: u64 = 0x80;
/// `DFLAG_EXTENDED_PIDS_PORTS` — full 32-bit pid/port fields.
pub const DFLAG_EXTENDED_PIDS_PORTS: u64 = 0x100;
/// `DFLAG_EXPORT_PTR_TAG` — `EXPORT_EXT` export tags.
pub const DFLAG_EXPORT_PTR_TAG: u64 = 0x200;
/// `DFLAG_BIT_BINARIES` — bit-level binaries.
pub const DFLAG_BIT_BINARIES: u64 = 0x400;
/// `DFLAG_NEW_FLOATS` — `NEW_FLOAT_EXT`.
pub const DFLAG_NEW_FLOATS: u64 = 0x800;
/// `DFLAG_UNICODE_IO` — Unicode-aware `io` (a.k.a. `UTF8_IO`).
pub const DFLAG_UNICODE_IO: u64 = 0x1000;
/// `DFLAG_DIST_HDR_ATOM_CACHE` — the distribution-header atom cache.
pub const DFLAG_DIST_HDR_ATOM_CACHE: u64 = 0x2000;
/// `DFLAG_SMALL_ATOM_TAGS` — `SMALL_ATOM_*` tags.
pub const DFLAG_SMALL_ATOM_TAGS: u64 = 0x4000;
/// `DFLAG_ETS_COMPRESSED` — compressed ETS (internal).
pub const DFLAG_ETS_COMPRESSED: u64 = 0x8000;
/// `DFLAG_UTF8_ATOMS` — UTF-8 (rather than Latin-1) atom names.
pub const DFLAG_UTF8_ATOMS: u64 = 0x10000;
/// `DFLAG_MAP_TAG` — `MAP_EXT`.
pub const DFLAG_MAP_TAG: u64 = 0x20000;
/// `DFLAG_BIG_CREATION` — 32-bit node creation.
pub const DFLAG_BIG_CREATION: u64 = 0x40000;
/// `DFLAG_SEND_SENDER` — `SEND_SENDER` control messages.
pub const DFLAG_SEND_SENDER: u64 = 0x80000;
/// `DFLAG_BIG_SEQTRACE_LABELS` — 64-bit seq-trace labels.
pub const DFLAG_BIG_SEQTRACE_LABELS: u64 = 0x100000;
/// `DFLAG_PENDING_CONNECT` — pending (not yet established) connection (internal).
pub const DFLAG_PENDING_CONNECT: u64 = 0x200000;
/// `DFLAG_EXIT_PAYLOAD` — exit with payload.
pub const DFLAG_EXIT_PAYLOAD: u64 = 0x400000;
/// `DFLAG_FRAGMENTS` — fragmented distribution messages.
pub const DFLAG_FRAGMENTS: u64 = 0x800000;
/// `DFLAG_HANDSHAKE_23` — the OTP 23+ connect handshake.
pub const DFLAG_HANDSHAKE_23: u64 = 0x1000000;
/// `DFLAG_UNLINK_ID` — `UNLINK_ID`/`UNLINK_ID_ACK`.
pub const DFLAG_UNLINK_ID: u64 = 0x2000000;
/// `DFLAG_MANDATORY_25_DIGEST` — the OTP 25 handshake digest.
pub const DFLAG_MANDATORY_25_DIGEST: u64 = 0x4000000;
/// `DFLAG_RESERVED` — reserved bits in the low flag word.
pub const DFLAG_RESERVED: u64 = 0xf8000000;
/// `DFLAG_SPAWN` — distributed spawn.
pub const DFLAG_SPAWN: u64 = 0x1_0000_0000;
/// `DFLAG_NAME_ME` — `-name`/`-sname` "name me" support.
pub const DFLAG_NAME_ME: u64 = 0x2_0000_0000;
/// `DFLAG_V4_NC` — the "no creation" v4 node container.
pub const DFLAG_V4_NC: u64 = 0x4_0000_0000;
/// `DFLAG_ALIAS` — process aliases (`ALIAS_SEND`).
pub const DFLAG_ALIAS: u64 = 0x8_0000_0000;
/// `DFLAG_LOCAL_EXT` — local extensions (internal).
pub const DFLAG_LOCAL_EXT: u64 = 0x10_0000_0000;
/// `DFLAG_ALTACT_SIG` — alternative activation signals.
pub const DFLAG_ALTACT_SIG: u64 = 0x20_0000_0000;
/// `DFLAG_NATIVE_RECORDS` — native records.
pub const DFLAG_NATIVE_RECORDS: u64 = 0x40_0000_0000;

/// `DFLAG_DETERMINISTIC` — an alias for [`DFLAG_ATOM_CACHE`] (`dist.h:77`).
pub const DFLAG_DETERMINISTIC: u64 = DFLAG_ATOM_CACHE;

// ---------------------------------------------------------------------------
// The negotiated-flags newtype.
// ---------------------------------------------------------------------------

/// The 64-bit distribution capability word negotiated in the connect handshake.
///
/// The frozen representation is a single machine-word (`u64`); this newtype adds
/// the ergonomic set operations the flag vocabulary needs (`contains`/`insert`/
/// `remove` and the bit operators). The individual `DFLAG_*` values stay raw
/// `u64` constants so that exact-value pins remain trivial.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DistFlags(u64);

impl DistFlags {
    /// No flags set.
    pub const EMPTY: DistFlags = DistFlags(0);

    /// The raw 64-bit flag word.
    #[inline]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Wraps a raw 64-bit flag word.
    #[inline]
    pub const fn from_bits(bits: u64) -> DistFlags {
        DistFlags(bits)
    }

    /// Returns `true` when **every** bit of `other` is set in `self`.
    #[inline]
    pub const fn contains(self, other: DistFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Sets every bit of `other`.
    #[inline]
    pub fn insert(&mut self, other: DistFlags) {
        self.0 |= other.0;
    }

    /// Clears every bit of `other`.
    #[inline]
    pub fn remove(&mut self, other: DistFlags) {
        self.0 &= !other.0;
    }
}

impl core::ops::BitOr for DistFlags {
    type Output = DistFlags;
    #[inline]
    fn bitor(self, rhs: DistFlags) -> DistFlags {
        DistFlags(self.0 | rhs.0)
    }
}

impl core::ops::BitAnd for DistFlags {
    type Output = DistFlags;
    #[inline]
    fn bitand(self, rhs: DistFlags) -> DistFlags {
        DistFlags(self.0 & rhs.0)
    }
}

impl core::ops::BitXor for DistFlags {
    type Output = DistFlags;
    #[inline]
    fn bitxor(self, rhs: DistFlags) -> DistFlags {
        DistFlags(self.0 ^ rhs.0)
    }
}

impl core::ops::Not for DistFlags {
    type Output = DistFlags;
    #[inline]
    fn not(self) -> DistFlags {
        DistFlags(!self.0)
    }
}

// ---------------------------------------------------------------------------
// Derived flag sets (exact ORs, dist.h:80-139).
// ---------------------------------------------------------------------------

/// `DFLAG_DIST_MANDATORY_25` — the flags mandatory for distribution in OTP 25.
pub const DFLAG_DIST_MANDATORY_25: u64 = DFLAG_EXTENDED_REFERENCES
    | DFLAG_FUN_TAGS
    | DFLAG_EXTENDED_PIDS_PORTS
    | DFLAG_UTF8_ATOMS
    | DFLAG_NEW_FUN_TAGS
    | DFLAG_BIG_CREATION
    | DFLAG_NEW_FLOATS
    | DFLAG_MAP_TAG
    | DFLAG_EXPORT_PTR_TAG
    | DFLAG_BIT_BINARIES
    | DFLAG_HANDSHAKE_23;

/// `DFLAG_DIST_MANDATORY_26` — the new mandatory flags for OTP 26.
pub const DFLAG_DIST_MANDATORY_26: u64 = DFLAG_V4_NC | DFLAG_UNLINK_ID;

/// `DFLAG_DIST_MANDATORY` — the flags a peer *must* offer.
pub const DFLAG_DIST_MANDATORY: u64 = DFLAG_DIST_MANDATORY_25 | DFLAG_DIST_MANDATORY_26;

/// `MANDATORY_DIST_FLAGS` — an alias of [`DFLAG_DIST_MANDATORY`].
pub const MANDATORY_DIST_FLAGS: u64 = DFLAG_DIST_MANDATORY;

/// `DFLAG_DIST_HOPEFULLY` — optimistically assumed flags for pending connections.
pub const DFLAG_DIST_HOPEFULLY: u64 = DFLAG_DIST_MONITOR
    | DFLAG_DIST_MONITOR_NAME
    | DFLAG_SPAWN
    | DFLAG_ALTACT_SIG
    | DFLAG_ALIAS
    | DFLAG_NATIVE_RECORDS;

/// `DFLAG_DIST_DEFAULT` — this node's preferred flag set (offered in the handshake).
pub const DFLAG_DIST_DEFAULT: u64 = DFLAG_DIST_MANDATORY
    | DFLAG_DIST_HOPEFULLY
    | DFLAG_UNICODE_IO
    | DFLAG_DIST_HDR_ATOM_CACHE
    | DFLAG_SMALL_ATOM_TAGS
    | DFLAG_SEND_SENDER
    | DFLAG_BIG_SEQTRACE_LABELS
    | DFLAG_EXIT_PAYLOAD
    | DFLAG_FRAGMENTS
    | DFLAG_SPAWN
    | DFLAG_ALIAS
    | DFLAG_MANDATORY_25_DIGEST;

/// `DFLAG_DIST_ADDABLE` — flags a local distribution implementation may add.
pub const DFLAG_DIST_ADDABLE: u64 = DFLAG_DIST_DEFAULT;

/// `DFLAG_DIST_REJECTABLE` — flags a local distribution implementation may reject.
pub const DFLAG_DIST_REJECTABLE: u64 =
    DFLAG_DIST_HDR_ATOM_CACHE | DFLAG_HIDDEN_ATOM_CACHE | DFLAG_FRAGMENTS | DFLAG_ATOM_CACHE;

/// `DFLAG_DIST_STRICT_ORDER` — flags requiring strictly ordered delivery.
pub const DFLAG_DIST_STRICT_ORDER: u64 = DFLAG_DIST_HDR_ATOM_CACHE;

/// `TERM_TO_BINARY_DFLAGS` — the flags assumed by `term_to_binary/1`.
pub const TERM_TO_BINARY_DFLAGS: u64 = DFLAG_NEW_FLOATS | DFLAG_UTF8_ATOMS;

// ---------------------------------------------------------------------------
// Control opcodes (`DOP_*`, dist.h:142-182).
// ---------------------------------------------------------------------------

/// `DOP_LINK` — link two processes.
pub const DOP_LINK: u8 = 1;
/// `DOP_SEND` — send a message.
pub const DOP_SEND: u8 = 2;
/// `DOP_EXIT` — an exit signal.
pub const DOP_EXIT: u8 = 3;
/// `DOP_UNLINK` — unlink two processes.
pub const DOP_UNLINK: u8 = 4;
/// `DOP_REG_SEND` — send to a registered name.
pub const DOP_REG_SEND: u8 = 6;
/// `DOP_GROUP_LEADER` — set the group leader.
pub const DOP_GROUP_LEADER: u8 = 7;
/// `DOP_EXIT2` — an exit2 signal (linked exit).
pub const DOP_EXIT2: u8 = 8;
/// `DOP_SEND_TT` — send with a trace token.
pub const DOP_SEND_TT: u8 = 12;
/// `DOP_EXIT_TT` — exit with a trace token.
pub const DOP_EXIT_TT: u8 = 13;
/// `DOP_REG_SEND_TT` — registered send with a trace token.
pub const DOP_REG_SEND_TT: u8 = 16;
/// `DOP_EXIT2_TT` — exit2 with a trace token.
pub const DOP_EXIT2_TT: u8 = 18;
/// `DOP_MONITOR_P` — monitor a process.
pub const DOP_MONITOR_P: u8 = 19;
/// `DOP_DEMONITOR_P` — demonitor a process.
pub const DOP_DEMONITOR_P: u8 = 20;
/// `DOP_MONITOR_P_EXIT` — a monitor-down signal.
pub const DOP_MONITOR_P_EXIT: u8 = 21;
/// `DOP_SEND_SENDER` — send carrying the sender pid.
pub const DOP_SEND_SENDER: u8 = 22;
/// `DOP_SEND_SENDER_TT` — sender-carrying send with a trace token.
pub const DOP_SEND_SENDER_TT: u8 = 23;
/// `DOP_PAYLOAD_EXIT` — exit with a payload.
pub const DOP_PAYLOAD_EXIT: u8 = 24;
/// `DOP_PAYLOAD_EXIT_TT` — payload exit with a trace token.
pub const DOP_PAYLOAD_EXIT_TT: u8 = 25;
/// `DOP_PAYLOAD_EXIT2` — exit2 with a payload.
pub const DOP_PAYLOAD_EXIT2: u8 = 26;
/// `DOP_PAYLOAD_EXIT2_TT` — payload exit2 with a trace token.
pub const DOP_PAYLOAD_EXIT2_TT: u8 = 27;
/// `DOP_PAYLOAD_MONITOR_P_EXIT` — monitor-down with a payload.
pub const DOP_PAYLOAD_MONITOR_P_EXIT: u8 = 28;
/// `DOP_SPAWN_REQUEST` — a distributed spawn request.
pub const DOP_SPAWN_REQUEST: u8 = 29;
/// `DOP_SPAWN_REQUEST_TT` — a spawn request with a trace token.
pub const DOP_SPAWN_REQUEST_TT: u8 = 30;
/// `DOP_SPAWN_REPLY` — a distributed spawn reply.
pub const DOP_SPAWN_REPLY: u8 = 31;
/// `DOP_SPAWN_REPLY_TT` — a spawn reply with a trace token.
pub const DOP_SPAWN_REPLY_TT: u8 = 32;
/// `DOP_ALIAS_SEND` — send via a process alias.
pub const DOP_ALIAS_SEND: u8 = 33;
/// `DOP_ALIAS_SEND_TT` — alias send with a trace token.
pub const DOP_ALIAS_SEND_TT: u8 = 34;
/// `DOP_UNLINK_ID` — id-carrying unlink.
pub const DOP_UNLINK_ID: u8 = 35;
/// `DOP_UNLINK_ID_ACK` — id-carrying unlink acknowledgement.
pub const DOP_UNLINK_ID_ACK: u8 = 36;
/// `DOP_ALTACT_SIG_SEND` — an alternative activation signal.
pub const DOP_ALTACT_SIG_SEND: u8 = 37;

/// A distribution control-message opcode (`enum dop`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Dop {
    /// `DOP_LINK` (1).
    Link,
    /// `DOP_SEND` (2).
    Send,
    /// `DOP_EXIT` (3).
    Exit,
    /// `DOP_UNLINK` (4).
    Unlink,
    /// `DOP_REG_SEND` (6).
    RegSend,
    /// `DOP_GROUP_LEADER` (7).
    GroupLeader,
    /// `DOP_EXIT2` (8).
    Exit2,
    /// `DOP_SEND_TT` (12).
    SendTt,
    /// `DOP_EXIT_TT` (13).
    ExitTt,
    /// `DOP_REG_SEND_TT` (16).
    RegSendTt,
    /// `DOP_EXIT2_TT` (18).
    Exit2Tt,
    /// `DOP_MONITOR_P` (19).
    MonitorP,
    /// `DOP_DEMONITOR_P` (20).
    DemonitorP,
    /// `DOP_MONITOR_P_EXIT` (21).
    MonitorPExit,
    /// `DOP_SEND_SENDER` (22).
    SendSender,
    /// `DOP_SEND_SENDER_TT` (23).
    SendSenderTt,
    /// `DOP_PAYLOAD_EXIT` (24).
    PayloadExit,
    /// `DOP_PAYLOAD_EXIT_TT` (25).
    PayloadExitTt,
    /// `DOP_PAYLOAD_EXIT2` (26).
    PayloadExit2,
    /// `DOP_PAYLOAD_EXIT2_TT` (27).
    PayloadExit2Tt,
    /// `DOP_PAYLOAD_MONITOR_P_EXIT` (28).
    PayloadMonitorPExit,
    /// `DOP_SPAWN_REQUEST` (29).
    SpawnRequest,
    /// `DOP_SPAWN_REQUEST_TT` (30).
    SpawnRequestTt,
    /// `DOP_SPAWN_REPLY` (31).
    SpawnReply,
    /// `DOP_SPAWN_REPLY_TT` (32).
    SpawnReplyTt,
    /// `DOP_ALIAS_SEND` (33).
    AliasSend,
    /// `DOP_ALIAS_SEND_TT` (34).
    AliasSendTt,
    /// `DOP_UNLINK_ID` (35).
    UnlinkId,
    /// `DOP_UNLINK_ID_ACK` (36).
    UnlinkIdAck,
    /// `DOP_ALTACT_SIG_SEND` (37).
    AltactSigSend,
}

impl Dop {
    /// Decodes a control opcode byte; `None` for an unknown opcode.
    pub fn from_u8(value: u8) -> Option<Dop> {
        let dop = match value {
            DOP_LINK => Dop::Link,
            DOP_SEND => Dop::Send,
            DOP_EXIT => Dop::Exit,
            DOP_UNLINK => Dop::Unlink,
            DOP_REG_SEND => Dop::RegSend,
            DOP_GROUP_LEADER => Dop::GroupLeader,
            DOP_EXIT2 => Dop::Exit2,
            DOP_SEND_TT => Dop::SendTt,
            DOP_EXIT_TT => Dop::ExitTt,
            DOP_REG_SEND_TT => Dop::RegSendTt,
            DOP_EXIT2_TT => Dop::Exit2Tt,
            DOP_MONITOR_P => Dop::MonitorP,
            DOP_DEMONITOR_P => Dop::DemonitorP,
            DOP_MONITOR_P_EXIT => Dop::MonitorPExit,
            DOP_SEND_SENDER => Dop::SendSender,
            DOP_SEND_SENDER_TT => Dop::SendSenderTt,
            DOP_PAYLOAD_EXIT => Dop::PayloadExit,
            DOP_PAYLOAD_EXIT_TT => Dop::PayloadExitTt,
            DOP_PAYLOAD_EXIT2 => Dop::PayloadExit2,
            DOP_PAYLOAD_EXIT2_TT => Dop::PayloadExit2Tt,
            DOP_PAYLOAD_MONITOR_P_EXIT => Dop::PayloadMonitorPExit,
            DOP_SPAWN_REQUEST => Dop::SpawnRequest,
            DOP_SPAWN_REQUEST_TT => Dop::SpawnRequestTt,
            DOP_SPAWN_REPLY => Dop::SpawnReply,
            DOP_SPAWN_REPLY_TT => Dop::SpawnReplyTt,
            DOP_ALIAS_SEND => Dop::AliasSend,
            DOP_ALIAS_SEND_TT => Dop::AliasSendTt,
            DOP_UNLINK_ID => Dop::UnlinkId,
            DOP_UNLINK_ID_ACK => Dop::UnlinkIdAck,
            DOP_ALTACT_SIG_SEND => Dop::AltactSigSend,
            _ => return None,
        };
        Some(dop)
    }

    /// The on-wire opcode byte.
    pub fn as_u8(self) -> u8 {
        match self {
            Dop::Link => DOP_LINK,
            Dop::Send => DOP_SEND,
            Dop::Exit => DOP_EXIT,
            Dop::Unlink => DOP_UNLINK,
            Dop::RegSend => DOP_REG_SEND,
            Dop::GroupLeader => DOP_GROUP_LEADER,
            Dop::Exit2 => DOP_EXIT2,
            Dop::SendTt => DOP_SEND_TT,
            Dop::ExitTt => DOP_EXIT_TT,
            Dop::RegSendTt => DOP_REG_SEND_TT,
            Dop::Exit2Tt => DOP_EXIT2_TT,
            Dop::MonitorP => DOP_MONITOR_P,
            Dop::DemonitorP => DOP_DEMONITOR_P,
            Dop::MonitorPExit => DOP_MONITOR_P_EXIT,
            Dop::SendSender => DOP_SEND_SENDER,
            Dop::SendSenderTt => DOP_SEND_SENDER_TT,
            Dop::PayloadExit => DOP_PAYLOAD_EXIT,
            Dop::PayloadExitTt => DOP_PAYLOAD_EXIT_TT,
            Dop::PayloadExit2 => DOP_PAYLOAD_EXIT2,
            Dop::PayloadExit2Tt => DOP_PAYLOAD_EXIT2_TT,
            Dop::PayloadMonitorPExit => DOP_PAYLOAD_MONITOR_P_EXIT,
            Dop::SpawnRequest => DOP_SPAWN_REQUEST,
            Dop::SpawnRequestTt => DOP_SPAWN_REQUEST_TT,
            Dop::SpawnReply => DOP_SPAWN_REPLY,
            Dop::SpawnReplyTt => DOP_SPAWN_REPLY_TT,
            Dop::AliasSend => DOP_ALIAS_SEND,
            Dop::AliasSendTt => DOP_ALIAS_SEND_TT,
            Dop::UnlinkId => DOP_UNLINK_ID,
            Dop::UnlinkIdAck => DOP_UNLINK_ID_ACK,
            Dop::AltactSigSend => DOP_ALTACT_SIG_SEND,
        }
    }

    /// A human-readable name, mirroring `erts_dop_to_string` (`dist.c:113`).
    ///
    /// The opcodes the C helper does not name (`SPAWN_REPLY_TT`, `ALIAS_SEND`,
    /// `ALIAS_SEND_TT`) use their `enum dop` identifier.
    pub fn name(self) -> &'static str {
        match self {
            Dop::Link => "LINK",
            Dop::Send => "SEND",
            Dop::Exit => "EXIT",
            Dop::Unlink => "UNLINK",
            Dop::RegSend => "REG_SEND",
            Dop::GroupLeader => "GROUP_LEADER",
            Dop::Exit2 => "EXIT2",
            Dop::SendTt => "SEND_TT",
            Dop::ExitTt => "EXIT_TT",
            Dop::RegSendTt => "REG_SEND_TT",
            Dop::Exit2Tt => "EXIT2_TT",
            Dop::MonitorP => "MONITOR_P",
            Dop::DemonitorP => "DEMONITOR_P",
            Dop::MonitorPExit => "MONITOR_P_EXIT",
            Dop::SendSender => "SEND_SENDER",
            Dop::SendSenderTt => "SEND_SENDER_TT",
            Dop::PayloadExit => "PAYLOAD_EXIT",
            Dop::PayloadExitTt => "PAYLOAD_EXIT_TT",
            Dop::PayloadExit2 => "PAYLOAD_EXIT2",
            Dop::PayloadExit2Tt => "PAYLOAD_EXIT2_TT",
            Dop::PayloadMonitorPExit => "PAYLOAD_MONITOR_P_EXIT",
            Dop::SpawnRequest => "SPAWN_REQUEST",
            Dop::SpawnRequestTt => "SPAWN_REQUEST_TT",
            Dop::SpawnReply => "SPAWN_REPLY",
            Dop::SpawnReplyTt => "SPAWN_REPLY_TT",
            Dop::AliasSend => "ALIAS_SEND",
            Dop::AliasSendTt => "ALIAS_SEND_TT",
            Dop::UnlinkId => "UNLINK_ID",
            Dop::UnlinkIdAck => "UNLINK_ID_ACK",
            Dop::AltactSigSend => "DOP_ALTACT_SIG_SEND",
        }
    }

    /// Whether this is a *trace-token* (`_TT`) control opcode.
    pub fn is_tt(self) -> bool {
        matches!(
            self,
            Dop::SendTt
                | Dop::ExitTt
                | Dop::RegSendTt
                | Dop::Exit2Tt
                | Dop::SendSenderTt
                | Dop::PayloadExitTt
                | Dop::PayloadExit2Tt
                | Dop::SpawnRequestTt
                | Dop::SpawnReplyTt
                | Dop::AliasSendTt
        )
    }

    /// The number of term arguments that follow the opcode in a control message.
    ///
    /// This is `tuple_arity - 1`: the C receive path validates each `DOP_*` as a
    /// message *tuple* `{DOP, Arg1, …}` whose first element is the opcode, while
    /// [`encode_control`](crate::control::encode_control)/[`decode_control`](crate::control::decode_control)
    /// carry the opcode as a bare byte followed by that many term bodies. Every
    /// value below is the `tuple_arity` check in `erts/emulator/beam/dist.c`
    /// minus one (read-only ground truth; never modify it).
    ///
    /// Citation for the C `tuple_arity` checks (comment-relative line numbers):
    /// - `DOP_LINK` ~2224, `DOP_UNLINK_ID` ~2284, `DOP_UNLINK_ID_ACK` ~2315,
    ///   `DOP_MONITOR_P` ~2341, `DOP_DEMONITOR_P` ~2415,
    ///   `DOP_REG_SEND(_TT)` ~2463, `DOP_SEND(_TT)`/`DOP_SEND_SENDER(_TT)` ~2512-2531,
    ///   `DOP_ALIAS_SEND(_TT)` ~2563, `DOP_ALTACT_SIG_SEND` ~2590,
    ///   `DOP_MONITOR_P_EXIT`/`DOP_PAYLOAD_MONITOR_P_EXIT` ~2700,
    ///   `DOP_EXIT(_TT)`/`DOP_PAYLOAD_EXIT(_TT)` ~2757,
    ///   `DOP_EXIT2(_TT)`/`DOP_PAYLOAD_EXIT2(_TT)` ~2821,
    ///   `DOP_GROUP_LEADER` ~2889, `DOP_SPAWN_REQUEST(_TT)` ~2902,
    ///   `DOP_SPAWN_REPLY(_TT)` ~3033.
    ///
    /// `DOP_UNLINK` is never sent (`dist.c:2277` — the C path rejects it); its
    /// shape `{op, From, To}` yields 2. `DOP_ALTACT_SIG_SEND` is *variable*
    /// arity in C (`tuple_arity >= 4` — "the tuple might be extended with more
    /// elements in the future"); this returns the documented minimum of 3, and
    /// its variant handling is out of M1 scope.
    pub fn arity(self) -> usize {
        match self {
            Dop::Link => 2,            // {DOP_LINK, From, To}                      (dist.c:2224)
            Dop::Send => 2,            // {DOP_SEND, From, To}                      (dist.c:2527)
            Dop::Exit => 3,            // {DOP_EXIT, From, To, Reason}              (dist.c:2760)
            Dop::Unlink => 2,          // never sent; {op, From, To} shape          (dist.c:2277)
            Dop::RegSend => 3,         // {DOP_REG_SEND, From, Cookie, ToName}      (dist.c:2468)
            Dop::GroupLeader => 2,     // {DOP_GROUP_LEADER, From, To}              (dist.c:2889)
            Dop::Exit2 => 3,           // {DOP_EXIT2, From, To, Reason}             (dist.c:2824)
            Dop::SendTt => 3,          // {DOP_SEND_TT, From, To, Token}            (dist.c:2513)
            Dop::ExitTt => 4,          // {DOP_EXIT_TT, From, To, Token, Reason}    (dist.c:2759)
            Dop::RegSendTt => 4,       // {DOP_REG_SEND_TT, From, Cookie, ToName, Token} (dist.c:2463)
            Dop::Exit2Tt => 4,         // {DOP_EXIT2_TT, From, To, Token, Reason}   (dist.c:2823)
            Dop::MonitorP => 3,        // {DOP_MONITOR_P, Remote, Local, Ref}       (dist.c:2341)
            Dop::DemonitorP => 3,      // {DOP_DEMONITOR_P, Remote, Local, Ref}     (dist.c:2415)
            Dop::MonitorPExit => 4,    // {DOP_MONITOR_P_EXIT, Remote, Local, Ref, Reason} (dist.c:2701)
            Dop::SendSender => 2,      // {DOP_SEND_SENDER, Sender, To}             (dist.c:2526)
            Dop::SendSenderTt => 3,    // {DOP_SEND_SENDER_TT, Sender, To, Token}   (dist.c:2512)
            Dop::PayloadExit => 2,     // {DOP_PAYLOAD_EXIT, From, To}              (dist.c:2757)
            Dop::PayloadExitTt => 3,   // {DOP_PAYLOAD_EXIT_TT, From, To, Token}    (dist.c:2758)
            Dop::PayloadExit2 => 2,    // {DOP_PAYLOAD_EXIT2, From, To}             (dist.c:2822)
            Dop::PayloadExit2Tt => 3,  // {DOP_PAYLOAD_EXIT2_TT, From, To, Token}   (dist.c:2821)
            Dop::PayloadMonitorPExit => 3, // {DOP_PAYLOAD_MONITOR_P_EXIT, Remote, Local, Ref} (dist.c:2700)
            Dop::SpawnRequest => 5,    // {DOP_SPAWN_REQUEST, Ref, From, GL, MFA, Opts} (dist.c:2919)
            Dop::SpawnRequestTt => 6,  // {DOP_SPAWN_REQUEST_TT, Ref, From, GL, MFA, Opts, Token} (dist.c:2902)
            Dop::SpawnReply => 4,      // {DOP_SPAWN_REPLY, Ref, To, Flags, From}   (dist.c:3051)
            Dop::SpawnReplyTt => 5,    // {DOP_SPAWN_REPLY_TT, Ref, To, Flags, From, Token} (dist.c:3033)
            Dop::AliasSend => 2,       // {DOP_ALIAS_SEND, From, To}                (dist.c:2563)
            Dop::AliasSendTt => 3,     // {DOP_ALIAS_SEND_TT, From, To, Token}      (dist.c:2570)
            Dop::UnlinkId => 3,        // {DOP_UNLINK_ID, Id, From, To}             (dist.c:2284)
            Dop::UnlinkIdAck => 3,     // {DOP_UNLINK_ID_ACK, Id, From, To}         (dist.c:2315)
            Dop::AltactSigSend => 3,   // {DOP_ALTACT_SIG_SEND, Flags, SenderPid, To[, Token]} (variable, min) (dist.c:2590)
        }
    }
}

// ---------------------------------------------------------------------------
// In-message flag fields (`ERTS_DIST_SPAWN_FLAG_*`, `ERTS_DOP_ALTACT_SIG_FLG_*`,
// dist.h:185-192).
//
// These are *not* handshake capability bits — they never travel in the
// `DFLAG_*` word. They are small integer flag fields carried *inside* two
// control messages, reproduced bit-for-bit from the C macros (contract A.5):
// - the `Flags` element of `DOP_ALTACT_SIG_SEND` (`dist.c:2590-2698`): the five
//   `ERTS_DOP_ALTACT_SIG_FLG_*` bits (`dist.h:188-192`); the receiver reads the
//   low bits of the (possibly bignum) flags integer and ignores the rest
//   (`dist.c:2615-2617`);
// - the `Flags` element of `DOP_SPAWN_REPLY`/`DOP_SPAWN_REPLY_TT`
//   (`dist.c:3090-3146`): `ERTS_DIST_SPAWN_FLAG_LINK`/`_MONITOR`
//   (`dist.h:185-186`), a small integer rejected when `>= 1 << 27`
//   (`dist.c:3092-3093`).
//
// The two fields live in *distinct namespaces*: bit 0 is `PRIO` for altact and
// `LINK` for spawn, so the same numeric value means different things depending
// on the enclosing opcode.
// ---------------------------------------------------------------------------

/// `ERTS_DIST_SPAWN_FLAG_LINK` (`dist.h:185`) — a distributed spawn-link: the
/// spawning process is linked to the new child (`dist.c:3102`/`3138`,
/// `erl_process.c:13098`).
pub const DIST_SPAWN_FLAG_LINK: u32 = 1 << 0;

/// `ERTS_DIST_SPAWN_FLAG_MONITOR` (`dist.h:186`) — the spawning process
/// monitors the new child (`dist.c:3100`, `erl_process.c:13100`).
pub const DIST_SPAWN_FLAG_MONITOR: u32 = 1 << 1;

/// `ERTS_DOP_ALTACT_SIG_FLG_PRIO` (`dist.h:188`) — a priority signal
/// (`dist.c:2687`); combined with [`DOP_ALTACT_SIG_FLG_ALIAS`] it is the
/// priority alias send (`dist.c:1646-1655`).
pub const DOP_ALTACT_SIG_FLG_PRIO: u32 = 1 << 0;

/// `ERTS_DOP_ALTACT_SIG_FLG_TOKEN` (`dist.h:189`) — the control message also
/// carries a trailing trace token, i.e. a 5-tuple rather than a 4-tuple
/// (`dist.c:2651-2658`); the receiver rewrites it to a `_TT` opcode
/// (`external.c:6866`).
pub const DOP_ALTACT_SIG_FLG_TOKEN: u32 = 1 << 1;

/// `ERTS_DOP_ALTACT_SIG_FLG_ALIAS` (`dist.h:190`) — `To` is a process alias
/// (a reference; `dist.c:2662-2666`).
pub const DOP_ALTACT_SIG_FLG_ALIAS: u32 = 1 << 2;

/// `ERTS_DOP_ALTACT_SIG_FLG_NAME` (`dist.h:191`) — `To` is a registered name
/// (an atom; `dist.c:2667-2671`).
pub const DOP_ALTACT_SIG_FLG_NAME: u32 = 1 << 3;

/// `ERTS_DOP_ALTACT_SIG_FLG_EXIT` (`dist.h:192`) — the signal is an exit signal
/// rather than a message signal (`dist.c:2689`).
pub const DOP_ALTACT_SIG_FLG_EXIT: u32 = 1 << 4;

/// The `Flags` field of a `DOP_SPAWN_REPLY`/`DOP_SPAWN_REPLY_TT` control
/// message (`ERTS_DIST_SPAWN_FLAG_*`, `dist.h:185-186`).
///
/// The C receive path validates it as a small integer `< 1 << 27`
/// (`dist.c:3092`) and reads the `LINK`/`MONITOR` bits; this newtype mirrors
/// [`DistFlags`]'s set ergonomics without the handshake capability word's
/// derived-set machinery.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DistSpawnFlags(u32);

impl DistSpawnFlags {
    /// No spawn flags set.
    pub const EMPTY: DistSpawnFlags = DistSpawnFlags(0);

    /// [`DIST_SPAWN_FLAG_LINK`].
    pub const LINK: DistSpawnFlags = DistSpawnFlags(DIST_SPAWN_FLAG_LINK);

    /// [`DIST_SPAWN_FLAG_MONITOR`].
    pub const MONITOR: DistSpawnFlags = DistSpawnFlags(DIST_SPAWN_FLAG_MONITOR);

    /// The raw 32-bit flag word.
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Wraps a raw 32-bit flag word.
    #[inline]
    pub const fn from_bits(bits: u32) -> DistSpawnFlags {
        DistSpawnFlags(bits)
    }

    /// Returns `true` when **every** bit of `other` is set in `self`.
    #[inline]
    pub const fn contains(self, other: DistSpawnFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Sets every bit of `other`.
    #[inline]
    pub fn insert(&mut self, other: DistSpawnFlags) {
        self.0 |= other.0;
    }

    /// Clears every bit of `other`.
    #[inline]
    pub fn remove(&mut self, other: DistSpawnFlags) {
        self.0 &= !other.0;
    }
}

/// The `Flags` element of a `DOP_ALTACT_SIG_SEND` control message
/// (`ERTS_DOP_ALTACT_SIG_FLG_*`, `dist.h:188-192`).
///
/// Five bits select the signal kind and shape (exit vs. message signal, alias
/// vs. name vs. pid target, priority, and the presence of a trailing token);
/// the C path tolerates wider flag integers and applies the same set operations.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AltactSigFlags(u32);

impl AltactSigFlags {
    /// No altact flags set.
    pub const EMPTY: AltactSigFlags = AltactSigFlags(0);

    /// [`DOP_ALTACT_SIG_FLG_PRIO`].
    pub const PRIO: AltactSigFlags = AltactSigFlags(DOP_ALTACT_SIG_FLG_PRIO);

    /// [`DOP_ALTACT_SIG_FLG_TOKEN`].
    pub const TOKEN: AltactSigFlags = AltactSigFlags(DOP_ALTACT_SIG_FLG_TOKEN);

    /// [`DOP_ALTACT_SIG_FLG_ALIAS`].
    pub const ALIAS: AltactSigFlags = AltactSigFlags(DOP_ALTACT_SIG_FLG_ALIAS);

    /// [`DOP_ALTACT_SIG_FLG_NAME`].
    pub const NAME: AltactSigFlags = AltactSigFlags(DOP_ALTACT_SIG_FLG_NAME);

    /// [`DOP_ALTACT_SIG_FLG_EXIT`].
    pub const EXIT: AltactSigFlags = AltactSigFlags(DOP_ALTACT_SIG_FLG_EXIT);

    /// The raw 32-bit flag word.
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Wraps a raw 32-bit flag word.
    #[inline]
    pub const fn from_bits(bits: u32) -> AltactSigFlags {
        AltactSigFlags(bits)
    }

    /// Returns `true` when **every** bit of `other` is set in `self`.
    #[inline]
    pub const fn contains(self, other: AltactSigFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Sets every bit of `other`.
    #[inline]
    pub fn insert(&mut self, other: AltactSigFlags) {
        self.0 |= other.0;
    }

    /// Clears every bit of `other`.
    #[inline]
    pub fn remove(&mut self, other: AltactSigFlags) {
        self.0 &= !other.0;
    }
}

// ---------------------------------------------------------------------------
// Handshake / version tags (ei_connect_int.h, ei_connect.c).
// ---------------------------------------------------------------------------

/// `NAME_TAG` (and `CHALLENGE_TAG`): the `'N'`/`'n'` connect-name tag.
pub const NAME_TAG: u8 = b'N';
/// `CHALLENGE_TAG`: the `'N'` challenge tag (same byte as [`NAME_TAG`]).
pub const CHALLENGE_TAG: u8 = b'N';
/// `STATUS_TAG`: the `'s'` status tag.
pub const STATUS_TAG: u8 = b's';
/// `CHALLENGE_REPLY_TAG`: the `'r'` challenge-reply tag.
pub const CHALLENGE_REPLY_TAG: u8 = b'r';
/// `CHALLENGE_ACK_TAG`: the `'a'` challenge-ack tag.
pub const CHALLENGE_ACK_TAG: u8 = b'a';
/// `COMPLEMENT_TAG`: the `'c'` OTP-23 high-flags complement tag.
pub const COMPLEMENT_TAG: u8 = b'c';
/// `DIST_VER_HIGH`: the highest handshake version understood (`EI_DIST_6`).
pub const DIST_VER_HIGH: u16 = 6;
/// `DIST_VER_LOW`: the lowest handshake version understood (`EI_DIST_6`).
pub const DIST_VER_LOW: u16 = 6;

// ---------------------------------------------------------------------------
// epmd tags + node types (ei_epmd.h, epmd.h, epmd_int.h).
// ---------------------------------------------------------------------------

/// `EPMD_ALIVE2_REQ`: the `'x'` node-registration request.
pub const EPMD_ALIVE2_REQ: u8 = b'x';
/// `EPMD_ALIVE2_RESP`: the `'y'` (legacy) registration response.
pub const EPMD_ALIVE2_RESP: u8 = b'y';
/// `EPMD_ALIVE2_X_RESP`: the `'v'` extended registration response.
pub const EPMD_ALIVE2_X_RESP: u8 = b'v';
/// `EPMD_PORT2_REQ`: the `'z'` port-lookup request.
pub const EPMD_PORT2_REQ: u8 = b'z';
/// `EPMD_PORT2_RESP`: the `'w'` port-lookup response.
pub const EPMD_PORT2_RESP: u8 = b'w';
/// `EPMD_PORT_PLEASE2_REQ`: the historical (pre-rename) spelling of the `'z'`
/// port-lookup request.
///
/// It is the **same wire byte** as [`EPMD_PORT2_REQ`] (`122 = 'z'`): the OTP
/// sources renamed `n_REQ` to `PORT2_REQ` and prefixed the tags `EPMD_` (see
/// `erts/doc/notes.md`), so this is a documented alias rather than a second
/// tag. The value is *not* repeated literally — it is [`EPMD_PORT2_REQ`].
pub const EPMD_PORT_PLEASE2_REQ: u8 = EPMD_PORT2_REQ;
/// `EPMD_NAMES_REQ`: the `'n'` "list names" request.
pub const EPMD_NAMES_REQ: u8 = b'n';
/// `EPMD_DUMP_REQ`: the `'d'` interactive "dump the node table" request
/// (`epmd.h`; used by the `epmd -dump` CLI, `epmd_cli.c::epmd_call`).
pub const EPMD_DUMP_REQ: u8 = b'd';
/// `EPMD_KILL_REQ`: the `'k'` interactive "kill the daemon" request
/// (`epmd.h`; used by the `epmd -kill` CLI, `epmd_cli.c::kill_epmd`).
pub const EPMD_KILL_REQ: u8 = b'k';
/// `EPMD_STOP_REQ`: the `'s'` interactive "unregister a node" request
/// (`epmd.h`; also `EI_EPMD_STOP_REQ` in `ei_epmd.h`, used by the `epmd -stop
/// Name` CLI, `epmd_cli.c::stop_cli`). The body is the tag followed by the node
/// name.
pub const EPMD_STOP_REQ: u8 = b's';
/// `EPMD_NODE_NORMAL`: a normal Erlang node (`77`, `'M'`).
pub const EPMD_NODE_NORMAL: u8 = b'M';
/// `EPMD_NODE_HIDDEN`: a hidden node / C-node (`72`, `'H'`).
pub const EPMD_NODE_HIDDEN: u8 = b'H';
