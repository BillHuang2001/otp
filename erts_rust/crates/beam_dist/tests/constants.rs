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

// ---------------------------------------------------------------------------
// 6. In-message flag fields (`ERTS_DIST_SPAWN_FLAG_*`, `ERTS_DOP_ALTACT_SIG_FLG_*`).
// ---------------------------------------------------------------------------

#[test]
fn in_message_flag_field_values() {
    // dist.h:185-192, verbatim.
    assert_eq!(DIST_SPAWN_FLAG_LINK, 1 << 0);
    assert_eq!(DIST_SPAWN_FLAG_MONITOR, 1 << 1);

    assert_eq!(DOP_ALTACT_SIG_FLG_PRIO, 1 << 0);
    assert_eq!(DOP_ALTACT_SIG_FLG_TOKEN, 1 << 1);
    assert_eq!(DOP_ALTACT_SIG_FLG_ALIAS, 1 << 2);
    assert_eq!(DOP_ALTACT_SIG_FLG_NAME, 1 << 3);
    assert_eq!(DOP_ALTACT_SIG_FLG_EXIT, 1 << 4);
}

#[test]
fn prio_and_link_share_bit_zero_but_are_distinct_namespaces() {
    // Both `ERTS_DOP_ALTACT_SIG_FLG_PRIO` and `ERTS_DIST_SPAWN_FLAG_LINK` are
    // `1 << 0`: the same numeric value means different things depending on the
    // enclosing control opcode, so the two newtypes must not be interchangeable.
    assert_eq!(DOP_ALTACT_SIG_FLG_PRIO, DIST_SPAWN_FLAG_LINK);
    assert_eq!(AltactSigFlags::PRIO.bits(), 1 << 0);
    assert_eq!(DistSpawnFlags::LINK.bits(), 1 << 0);
    assert_eq!(AltactSigFlags::PRIO.bits(), DistSpawnFlags::LINK.bits());
}

#[test]
fn altact_flags_cover_exactly_five_bits() {
    // The five altact bits are contiguous `1 << 0 ..= 1 << 4` (`dist.h:188-192`).
    assert_eq!(DOP_ALTACT_SIG_FLG_EXIT, 1 << 4);
    let all = DOP_ALTACT_SIG_FLG_PRIO
        | DOP_ALTACT_SIG_FLG_TOKEN
        | DOP_ALTACT_SIG_FLG_ALIAS
        | DOP_ALTACT_SIG_FLG_NAME
        | DOP_ALTACT_SIG_FLG_EXIT;
    assert_eq!(all, 0b1_1111);
    assert!(all < (1 << 5), "altact flag field must fit a 5-bit width");
}

#[test]
fn spawn_flags_set_operations() {
    let mut flags = DistSpawnFlags::EMPTY;
    assert_eq!(flags.bits(), 0);
    assert!(!flags.contains(DistSpawnFlags::LINK));
    // `EMPTY` is vacuously a subset of every flag word (same rule as `DistFlags`).
    assert!(flags.contains(DistSpawnFlags::EMPTY));

    flags.insert(DistSpawnFlags::LINK);
    assert!(flags.contains(DistSpawnFlags::LINK));
    assert!(!flags.contains(DistSpawnFlags::MONITOR));
    assert_eq!(DistSpawnFlags::from_bits(flags.bits()), flags);

    flags.insert(DistSpawnFlags::MONITOR);
    assert!(flags.contains(DistSpawnFlags::LINK));
    assert!(flags.contains(DistSpawnFlags::MONITOR));
    assert_eq!(flags.bits(), DIST_SPAWN_FLAG_LINK | DIST_SPAWN_FLAG_MONITOR);

    flags.remove(DistSpawnFlags::LINK);
    assert!(!flags.contains(DistSpawnFlags::LINK));
    assert!(flags.contains(DistSpawnFlags::MONITOR));

    flags.remove(DistSpawnFlags::MONITOR);
    assert_eq!(flags, DistSpawnFlags::EMPTY);
    assert_eq!(flags.bits(), 0);
}

#[test]
fn altact_flags_set_operations_and_independence() {
    // `EXIT|TOKEN` contains both bits; neither bit alone contains the other.
    let both = AltactSigFlags::from_bits(DOP_ALTACT_SIG_FLG_EXIT | DOP_ALTACT_SIG_FLG_TOKEN);
    assert!(both.contains(AltactSigFlags::EXIT));
    assert!(both.contains(AltactSigFlags::TOKEN));
    assert!(!AltactSigFlags::EXIT.contains(AltactSigFlags::TOKEN));
    assert!(!AltactSigFlags::TOKEN.contains(AltactSigFlags::EXIT));
    assert_eq!(both.bits(), 0b1_0010);
    assert_eq!(AltactSigFlags::from_bits(both.bits()), both);

    // Incremental build-up and tear-down round-trips every bit.
    let mut flags = AltactSigFlags::EMPTY;
    assert_eq!(flags.bits(), 0);
    flags.insert(AltactSigFlags::PRIO);
    flags.insert(AltactSigFlags::ALIAS);
    flags.insert(AltactSigFlags::NAME);
    assert_eq!(flags.bits(), 1 | (1 << 2) | (1 << 3));
    assert!(flags.contains(AltactSigFlags::PRIO));
    assert!(!flags.contains(AltactSigFlags::EXIT));

    flags.remove(AltactSigFlags::ALIAS);
    assert!(!flags.contains(AltactSigFlags::ALIAS));
    assert!(flags.contains(AltactSigFlags::NAME));
    assert_eq!(AltactSigFlags::from_bits(flags.bits()), flags);
}

#[test]
fn altact_flag_consts_are_independent_single_bits() {
    let all = [
        AltactSigFlags::PRIO,
        AltactSigFlags::TOKEN,
        AltactSigFlags::ALIAS,
        AltactSigFlags::NAME,
        AltactSigFlags::EXIT,
    ];
    for (i, a) in all.iter().enumerate() {
        assert_eq!(a.bits().count_ones(), 1, "{a:?} must be a single bit");
        for (j, b) in all.iter().enumerate() {
            if i == j {
                assert_eq!(*a, *b);
            } else {
                assert_ne!(a.bits(), b.bits(), "{a:?} and {b:?} must differ");
                assert!(!a.contains(*b), "{a:?} must not contain {b:?}");
            }
        }
    }
    assert_eq!(AltactSigFlags::EMPTY.bits(), 0);
    assert_eq!(DistSpawnFlags::EMPTY.bits(), 0);
}
