//! Exact-value pins for the distribution vocabulary: representative (and the
//! derived) `DFLAG_*`/`DOP_*` constants, the `Dop` behaviour, the
//! mandatory/default/rejectable sets, and the handshake/`epmd` tag bytes.
//!
//! Every literal here is copied verbatim from the wire-frozen ground truth
//! (`erts/emulator/beam/dist.h`, `dist.c`, `ei_connect_int.h`, `epmd_int.h`)
//! so that any accidental renumbering of a capability bit or opcode fails the
//! build loudly. No decoding happens in this file, so there is nothing to
//! `unwrap()`. (Implementation phase.)

use beam_dist::flags::*;
use beam_dist::{DIST_VERSION, DIST_VERSION_5, EPMD_PORT, VERSION_MAGIC, Dop};

/// Every `Dop` variant, so the exhaustive behaviours below cannot silently skip
/// a newly added opcode (a new variant forces a compile error in the arity test).
const ALL_DOPS: &[Dop] = &[
    Dop::Link,
    Dop::Send,
    Dop::Exit,
    Dop::Unlink,
    Dop::RegSend,
    Dop::GroupLeader,
    Dop::Exit2,
    Dop::SendTt,
    Dop::ExitTt,
    Dop::RegSendTt,
    Dop::Exit2Tt,
    Dop::MonitorP,
    Dop::DemonitorP,
    Dop::MonitorPExit,
    Dop::SendSender,
    Dop::SendSenderTt,
    Dop::PayloadExit,
    Dop::PayloadExitTt,
    Dop::PayloadExit2,
    Dop::PayloadExit2Tt,
    Dop::PayloadMonitorPExit,
    Dop::SpawnRequest,
    Dop::SpawnRequestTt,
    Dop::SpawnReply,
    Dop::SpawnReplyTt,
    Dop::AliasSend,
    Dop::AliasSendTt,
    Dop::UnlinkId,
    Dop::UnlinkIdAck,
    Dop::AltactSigSend,
];

/// The ten trace-token (`_TT`) opcodes.
const TT_DOPS: &[Dop] = &[
    Dop::SendTt,
    Dop::ExitTt,
    Dop::RegSendTt,
    Dop::Exit2Tt,
    Dop::SendSenderTt,
    Dop::PayloadExitTt,
    Dop::PayloadExit2Tt,
    Dop::SpawnRequestTt,
    Dop::SpawnReplyTt,
    Dop::AliasSendTt,
];

// ---------------------------------------------------------------------------
// 1. Capability bits (`DFLAG_*`).
// ---------------------------------------------------------------------------

#[test]
fn dflag_representative_values() {
    assert_eq!(DFLAG_PUBLISHED, 0x01);
    assert_eq!(DFLAG_ATOM_CACHE, 0x02);
    assert_eq!(DFLAG_EXTENDED_REFERENCES, 0x04);
    assert_eq!(DFLAG_DIST_MONITOR, 0x08);
    assert_eq!(DFLAG_FUN_TAGS, 0x10);
    assert_eq!(DFLAG_DIST_MONITOR_NAME, 0x20);
    assert_eq!(DFLAG_HIDDEN_ATOM_CACHE, 0x40);
    assert_eq!(DFLAG_NEW_FUN_TAGS, 0x80);
    assert_eq!(DFLAG_EXTENDED_PIDS_PORTS, 0x100);
    assert_eq!(DFLAG_EXPORT_PTR_TAG, 0x200);
    assert_eq!(DFLAG_BIT_BINARIES, 0x400);
    assert_eq!(DFLAG_NEW_FLOATS, 0x800);
    assert_eq!(DFLAG_UNICODE_IO, 0x1000);
    assert_eq!(DFLAG_DIST_HDR_ATOM_CACHE, 0x2000);
    assert_eq!(DFLAG_SMALL_ATOM_TAGS, 0x4000);
    assert_eq!(DFLAG_MAP_TAG, 0x20000);
    assert_eq!(DFLAG_BIG_CREATION, 0x40000);
    assert_eq!(DFLAG_SEND_SENDER, 0x80000);
    assert_eq!(DFLAG_FRAGMENTS, 0x800000);
    assert_eq!(DFLAG_HANDSHAKE_23, 0x1000000);
    assert_eq!(DFLAG_MANDATORY_25_DIGEST, 0x4000000);
    assert_eq!(DFLAG_SPAWN, 0x1_0000_0000);
    assert_eq!(DFLAG_ALIAS, 0x8_0000_0000);
}

#[test]
fn dflag_deterministic_is_atom_cache() {
    assert_eq!(DFLAG_DETERMINISTIC, DFLAG_ATOM_CACHE);
    assert_eq!(DFLAG_DETERMINISTIC, 0x02);
}

// ---------------------------------------------------------------------------
// 2. Control opcodes (`DOP_*`).
// ---------------------------------------------------------------------------

#[test]
fn dop_representative_values() {
    assert_eq!(DOP_LINK, 1);
    assert_eq!(DOP_SEND, 2);
    assert_eq!(DOP_EXIT, 3);
    assert_eq!(DOP_UNLINK, 4);
    assert_eq!(DOP_REG_SEND, 6);
    assert_eq!(DOP_GROUP_LEADER, 7);
    assert_eq!(DOP_EXIT2, 8);
    assert_eq!(DOP_SEND_TT, 12);
    assert_eq!(DOP_EXIT_TT, 13);
    assert_eq!(DOP_REG_SEND_TT, 16);
    assert_eq!(DOP_EXIT2_TT, 18);
    assert_eq!(DOP_MONITOR_P, 19);
    assert_eq!(DOP_DEMONITOR_P, 20);
    assert_eq!(DOP_MONITOR_P_EXIT, 21);
    assert_eq!(DOP_SEND_SENDER, 22);
    assert_eq!(DOP_UNLINK_ID, 35);
    assert_eq!(DOP_UNLINK_ID_ACK, 36);
    assert_eq!(DOP_ALTACT_SIG_SEND, 37);
}

// ---------------------------------------------------------------------------
// 3. Derived flag sets.
// ---------------------------------------------------------------------------

#[test]
fn derived_mandatory_sets() {
    assert_eq!(
        DFLAG_DIST_MANDATORY_25,
        DFLAG_EXTENDED_REFERENCES
            | DFLAG_FUN_TAGS
            | DFLAG_EXTENDED_PIDS_PORTS
            | DFLAG_UTF8_ATOMS
            | DFLAG_NEW_FUN_TAGS
            | DFLAG_BIG_CREATION
            | DFLAG_NEW_FLOATS
            | DFLAG_MAP_TAG
            | DFLAG_EXPORT_PTR_TAG
            | DFLAG_BIT_BINARIES
            | DFLAG_HANDSHAKE_23
    );
    assert_eq!(DFLAG_DIST_MANDATORY_26, DFLAG_V4_NC | DFLAG_UNLINK_ID);
    assert_eq!(
        DFLAG_DIST_MANDATORY,
        DFLAG_DIST_MANDATORY_25 | DFLAG_DIST_MANDATORY_26
    );
    assert_eq!(MANDATORY_DIST_FLAGS, DFLAG_DIST_MANDATORY);
}

#[test]
fn derived_rejectable_and_strict_order() {
    assert_eq!(
        DFLAG_DIST_REJECTABLE,
        DFLAG_DIST_HDR_ATOM_CACHE | DFLAG_HIDDEN_ATOM_CACHE | DFLAG_FRAGMENTS | DFLAG_ATOM_CACHE
    );
    assert_eq!(DFLAG_DIST_STRICT_ORDER, DFLAG_DIST_HDR_ATOM_CACHE);
    assert_eq!(TERM_TO_BINARY_DFLAGS, DFLAG_NEW_FLOATS | DFLAG_UTF8_ATOMS);
}

#[test]
fn default_set_contains_mandatory_bits() {
    let default = DistFlags::from_bits(DFLAG_DIST_DEFAULT);
    for bit in [
        DFLAG_EXTENDED_REFERENCES,
        DFLAG_FUN_TAGS,
        DFLAG_EXTENDED_PIDS_PORTS,
        DFLAG_UTF8_ATOMS,
        DFLAG_NEW_FUN_TAGS,
        DFLAG_BIG_CREATION,
        DFLAG_NEW_FLOATS,
        DFLAG_MAP_TAG,
        DFLAG_EXPORT_PTR_TAG,
        DFLAG_BIT_BINARIES,
        DFLAG_HANDSHAKE_23,
        DFLAG_V4_NC,
        DFLAG_UNLINK_ID,
    ] {
        assert!(
            default.contains(DistFlags::from_bits(bit)),
            "DFLAG_DIST_DEFAULT missing mandatory bit {bit:#x}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. `Dop` behaviour.
// ---------------------------------------------------------------------------

#[test]
fn dop_from_u8_round_trips() {
    for &dop in ALL_DOPS {
        assert_eq!(
            Dop::from_u8(dop.as_u8()),
            Some(dop),
            "from_u8/as_u8 round-trip failed for {dop:?}"
        );
    }
}

#[test]
fn dop_from_u8_rejects_unknown() {
    assert_eq!(Dop::from_u8(0), None);
    assert_eq!(Dop::from_u8(5), None);
    assert_eq!(Dop::from_u8(38), None);
    assert_eq!(Dop::from_u8(255), None);
}

#[test]
fn dop_names() {
    assert_eq!(Dop::Link.name(), "LINK");
    assert_eq!(Dop::RegSend.name(), "REG_SEND");
    assert_eq!(Dop::AltactSigSend.name(), "DOP_ALTACT_SIG_SEND");
}

#[test]
fn dop_is_tt() {
    for &dop in TT_DOPS {
        assert!(dop.is_tt(), "{dop:?} should be a trace-token opcode");
    }
    for &dop in &[
        Dop::Link,
        Dop::Send,
        Dop::Exit,
        Dop::MonitorP,
        Dop::MonitorPExit,
        Dop::UnlinkId,
        Dop::SpawnRequest,
        Dop::SpawnReply,
        Dop::AliasSend,
        Dop::AltactSigSend,
    ] {
        assert!(!dop.is_tt(), "{dop:?} should not be a trace-token opcode");
    }
}

#[test]
fn dop_arity_table() {
    assert_eq!(Dop::Link.arity(), 2);
    assert_eq!(Dop::Send.arity(), 2);
    assert_eq!(Dop::Exit.arity(), 3);
    assert_eq!(Dop::Unlink.arity(), 2);
    assert_eq!(Dop::RegSend.arity(), 3);
    assert_eq!(Dop::GroupLeader.arity(), 2);
    assert_eq!(Dop::Exit2.arity(), 3);
    assert_eq!(Dop::SendTt.arity(), 3);
    assert_eq!(Dop::ExitTt.arity(), 4);
    assert_eq!(Dop::RegSendTt.arity(), 4);
    assert_eq!(Dop::Exit2Tt.arity(), 4);
    assert_eq!(Dop::MonitorP.arity(), 3);
    assert_eq!(Dop::DemonitorP.arity(), 3);
    assert_eq!(Dop::MonitorPExit.arity(), 4);
    assert_eq!(Dop::SendSender.arity(), 2);
    assert_eq!(Dop::SendSenderTt.arity(), 3);
    assert_eq!(Dop::PayloadExit.arity(), 2);
    assert_eq!(Dop::PayloadExitTt.arity(), 3);
    assert_eq!(Dop::PayloadExit2.arity(), 2);
    assert_eq!(Dop::PayloadExit2Tt.arity(), 3);
    assert_eq!(Dop::PayloadMonitorPExit.arity(), 3);
    assert_eq!(Dop::SpawnRequest.arity(), 5);
    assert_eq!(Dop::SpawnRequestTt.arity(), 6);
    assert_eq!(Dop::SpawnReply.arity(), 4);
    assert_eq!(Dop::SpawnReplyTt.arity(), 5);
    assert_eq!(Dop::AliasSend.arity(), 2);
    assert_eq!(Dop::AliasSendTt.arity(), 3);
    assert_eq!(Dop::UnlinkId.arity(), 3);
    assert_eq!(Dop::UnlinkIdAck.arity(), 3);
    assert_eq!(Dop::AltactSigSend.arity(), 3);
}

// ---------------------------------------------------------------------------
// 5. Handshake / `epmd` tag bytes + crate constants.
// ---------------------------------------------------------------------------

#[test]
fn handshake_tags_and_versions() {
    assert_eq!(NAME_TAG, b'N');
    assert_eq!(CHALLENGE_TAG, b'N');
    assert_eq!(STATUS_TAG, b's');
    assert_eq!(CHALLENGE_REPLY_TAG, b'r');
    assert_eq!(CHALLENGE_ACK_TAG, b'a');
    assert_eq!(COMPLEMENT_TAG, b'c');
    assert_eq!(DIST_VER_HIGH, 6);
    assert_eq!(DIST_VER_LOW, 6);
}

#[test]
fn epmd_tags_and_node_types() {
    assert_eq!(EPMD_ALIVE2_REQ, b'x');
    assert_eq!(EPMD_ALIVE2_RESP, b'y');
    assert_eq!(EPMD_ALIVE2_X_RESP, b'v');
    assert_eq!(EPMD_PORT2_REQ, b'z');
    assert_eq!(EPMD_PORT2_RESP, b'w');
    assert_eq!(EPMD_NAMES_REQ, b'n');
    assert_eq!(EPMD_NODE_NORMAL, b'M');
    assert_eq!(EPMD_NODE_HIDDEN, b'H');
}

#[test]
fn crate_constants() {
    assert_eq!(DIST_VERSION, 6);
    assert_eq!(DIST_VERSION_5, 5);
    assert_eq!(EPMD_PORT, 4369);
    assert_eq!(VERSION_MAGIC, 131);
}
