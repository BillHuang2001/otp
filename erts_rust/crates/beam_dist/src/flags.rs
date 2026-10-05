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
    /// Taken from the `{DOP_*, …}` tuple shapes in `dist.c`.
    pub fn arity(self) -> usize {
        todo!("M1")
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
/// `EPMD_NAMES_REQ`: the `'n'` "list names" request.
pub const EPMD_NAMES_REQ: u8 = b'n';
/// `EPMD_NODE_NORMAL`: a normal Erlang node (`77`, `'M'`).
pub const EPMD_NODE_NORMAL: u8 = b'M';
/// `EPMD_NODE_HIDDEN`: a hidden node / C-node (`72`, `'H'`).
pub const EPMD_NODE_HIDDEN: u8 = b'H';
