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

//! `Name`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement`/`Status`
//! encode→decode round-trips and negative (wrong tag / truncated) cases.
//!
//! Every case here is byte-exact against the ground truth in
//! `lib/erl_interface/src/connect/ei_connect.c` (`send_name`/`send_challenge`/
//! `send_challenge_reply`/`send_challenge_ack`/`recv_*`) and the Erlang peer
//! `lib/kernel/src/dist_util.erl` (`send_status`/`recv_complement`/…).

use beam_common::{BeamError, Result};

use beam_dist::flags::{
    DistFlags, DFLAG_DIST_DEFAULT, DFLAG_DIST_MANDATORY, DFLAG_DIST_MANDATORY_25,
    DFLAG_DIST_MANDATORY_26, DFLAG_MANDATORY_25_DIGEST, DFLAG_MAP_TAG, DFLAG_PUBLISHED,
};
use beam_dist::handshake::{
    check_mandatory, preferred_flags, publish_flag, publish_type, status_error, verify_digest,
    Challenge, ChallengeAck, ChallengeDigest, ChallengeReply, Complement, Name, PeerVisibility,
    PublishKind, Status, StatusKind, CHALLENGE_ACK_TAG, CHALLENGE_REPLY_TAG, CHALLENGE_TAG,
    COMPLEMENT_TAG, NAME_TAG, STATUS_TAG,
};
use beam_dist::transport::Write;

/// A trivial `Vec<u8>`-backed [`Write`] sink so the body codecs can be driven
/// without a live transport.
#[derive(Default)]
struct VecWriter(Vec<u8>);

impl Write for VecWriter {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.0.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Encodes `msg` via its `encode` method and returns the raw body bytes.
fn body<F>(encode: F) -> Vec<u8>
where
    F: FnOnce(&mut VecWriter) -> Result<()>,
{
    let mut w = VecWriter::default();
    encode(&mut w).expect("encoding a valid message must not fail");
    w.0
}

/// A 16-byte digest value used for the opaque-digest round-trip tests.
const DIGEST: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];

fn sample_flags() -> DistFlags {
    preferred_flags()
}

// ---------------------------------------------------------------------------
// Name
// ---------------------------------------------------------------------------

#[test]
fn name_roundtrip_and_exact_layout() -> Result<()> {
    let original = Name {
        flags: sample_flags(),
        creation: 0xdead_beef,
        name: b"gurka@localhost".to_vec(),
    };

    let bytes = body(|w| original.encode(w));

    // Layout: 'N' + flags(8) + creation(4) + namelen(2) + name.
    assert_eq!(bytes[0], NAME_TAG);
    assert_eq!(bytes[0], b'N');
    assert_eq!(&bytes[1..9], &original.flags.bits().to_be_bytes());
    assert_eq!(&bytes[9..13], &original.creation.to_be_bytes());
    assert_eq!(&bytes[13..15], &(original.name.len() as u16).to_be_bytes());
    assert_eq!(&bytes[15..], original.name.as_slice());
    assert_eq!(bytes.len(), 1 + 8 + 4 + 2 + original.name.len());

    assert_eq!(Name::decode(&bytes)?, original);
    Ok(())
}

#[test]
fn name_empty_name_roundtrips() -> Result<()> {
    let original = Name { flags: DistFlags::EMPTY, creation: 0, name: Vec::new() };
    let bytes = body(|w| original.encode(w));
    assert_eq!(bytes, vec![b'N', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(Name::decode(&bytes)?, original);
    Ok(())
}

// ---------------------------------------------------------------------------
// Challenge
// ---------------------------------------------------------------------------

#[test]
fn challenge_roundtrip_and_exact_layout() -> Result<()> {
    let original = Challenge {
        flags: sample_flags(),
        challenge: 0x0102_0304,
        creation: 0x0a0b_0c0d,
        name: b"gurka@localhost".to_vec(),
    };

    let bytes = body(|w| original.encode(w));

    // Layout: 'N' + flags(8) + challenge(4) + creation(4) + namelen(2) + name.
    assert_eq!(bytes[0], CHALLENGE_TAG);
    assert_eq!(bytes[0], b'N');
    assert_eq!(&bytes[1..9], &original.flags.bits().to_be_bytes());
    assert_eq!(&bytes[9..13], &original.challenge.to_be_bytes());
    assert_eq!(&bytes[13..17], &original.creation.to_be_bytes());
    assert_eq!(&bytes[17..19], &(original.name.len() as u16).to_be_bytes());
    assert_eq!(&bytes[19..], original.name.as_slice());
    assert_eq!(bytes.len(), 1 + 8 + 4 + 4 + 2 + original.name.len());

    assert_eq!(Challenge::decode(&bytes)?, original);
    Ok(())
}

// ---------------------------------------------------------------------------
// ChallengeReply
// ---------------------------------------------------------------------------

#[test]
fn challenge_reply_roundtrip_and_layout() -> Result<()> {
    let original = ChallengeReply { challenge: 0xfedc_ba98, digest: DIGEST };

    let bytes = body(|w| original.encode(w));

    // Layout: 'r' + challenge(4) + digest(16) — 21 bytes, the `rlen != 21`
    // check of `recv_challenge_reply`.
    assert_eq!(bytes[0], CHALLENGE_REPLY_TAG);
    assert_eq!(bytes[0], b'r');
    assert_eq!(&bytes[1..5], &original.challenge.to_be_bytes());
    assert_eq!(&bytes[5..21], &DIGEST);
    assert_eq!(bytes.len(), 21);

    assert_eq!(ChallengeReply::decode(&bytes)?, original);
    Ok(())
}

// ---------------------------------------------------------------------------
// ChallengeAck
// ---------------------------------------------------------------------------

#[test]
fn challenge_ack_roundtrip_and_layout() -> Result<()> {
    let original = ChallengeAck { digest: DIGEST };

    let bytes = body(|w| original.encode(w));

    // Layout: 'a' + digest(16) — 17 bytes, the `rlen != 17` check of
    // `recv_challenge_ack`.
    assert_eq!(bytes[0], CHALLENGE_ACK_TAG);
    assert_eq!(bytes[0], b'a');
    assert_eq!(&bytes[1..17], &DIGEST);
    assert_eq!(bytes.len(), 17);

    assert_eq!(ChallengeAck::decode(&bytes)?, original);
    Ok(())
}

// ---------------------------------------------------------------------------
// Complement
// ---------------------------------------------------------------------------

#[test]
fn complement_roundtrip_and_layout() -> Result<()> {
    let original = Complement { flags_high: 0x0000_00c0, creation: 0x1234_5678 };

    let bytes = body(|w| original.encode(w));

    // Layout: 'c' + flags-high(4) + creation(4) — 9 bytes, matching the Erlang
    // peer `recv_complement`'s `[$c, F7,F6,F5,F4, Cr3,Cr2,Cr1,Cr0]` match.
    assert_eq!(bytes[0], COMPLEMENT_TAG);
    assert_eq!(bytes[0], b'c');
    assert_eq!(&bytes[1..5], &original.flags_high.to_be_bytes());
    assert_eq!(&bytes[5..9], &original.creation.to_be_bytes());
    assert_eq!(bytes.len(), 9);

    assert_eq!(Complement::decode(&bytes)?, original);
    Ok(())
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[test]
fn status_ok_decodes_and_roundtrips() -> Result<()> {
    let bytes = body(|w| Status::Ok.encode(w));
    assert_eq!(bytes, b"sok");
    assert_eq!(bytes[0], STATUS_TAG);
    assert_eq!(Status::decode(&bytes)?, Status::Ok);
    Ok(())
}

#[test]
fn status_simultaneous_decodes_and_roundtrips() -> Result<()> {
    let bytes = body(|w| Status::Simultaneous.encode(w));
    assert_eq!(bytes, b"sok_simultaneous");
    assert_eq!(bytes[0], STATUS_TAG);
    assert_eq!(Status::decode(&bytes)?, Status::Simultaneous);

    // The C `recv_status` accepts any `"sok"` prefix, so also decode the short
    // `"sok"` body explicitly.
    assert_eq!(Status::decode(b"sok")?, Status::Ok);
    Ok(())
}

#[test]
fn status_named_roundtrip_and_layout() -> Result<()> {
    // `dist_util.erl` sends `[$s, "named:", <<NameLen:16, Name/binary>>,
    // <<Creation:32>>]`, which `recv_status` reads as `"snamed:"` because the
    // tag is the leading `'s'`.
    let name = b"gurka@localhost".to_vec();
    let creation = 0x0bad_f00du32;
    let original = Status::Named { name: name.clone(), creation };

    let bytes = body(|w| original.encode(w));

    assert_eq!(bytes[0], STATUS_TAG);
    assert_eq!(bytes[0], b's');
    assert_eq!(&bytes[..7], b"snamed:");
    assert_eq!(&bytes[1..7], b"named:");
    assert_eq!(&bytes[7..9], &(name.len() as u16).to_be_bytes());
    assert_eq!(&bytes[9..9 + name.len()], name.as_slice());
    assert_eq!(&bytes[9 + name.len()..], &creation.to_be_bytes());
    assert_eq!(bytes.len(), 1 + 6 + 2 + name.len() + 4);

    assert_eq!(Status::decode(&bytes)?, original);
    Ok(())
}
#[test]
fn status_named_truncated_name_is_bad_arg() {
    // namelen claims 20 bytes but only 3 follow.
    let body = [b's', b'n', b'a', b'm', b'e', b'd', b':', 0, 20, b'a', b'b', b'c', 0, 0, 0, 0];
    assert!(matches!(Status::decode(&body), Err(BeamError::BadArg)));
}

#[test]
fn status_named_missing_creation_is_bad_arg() {
    // namelen 3, name present, but the 4 creation bytes are missing.
    let body = [b's', b'n', b'a', b'm', b'e', b'd', b':', 0, 3, b'a', b'b', b'c'];
    assert!(matches!(Status::decode(&body), Err(BeamError::BadArg)));
}

#[test]
fn status_error_roundtrip() -> Result<()> {
    for text in [&b"nok"[..], b"not_allowed", b"alive", b"nok_cookie"] {
        let original = Status::Error(text.to_vec());
        let bytes = body(|w| original.encode(w));
        assert_eq!(bytes[0], STATUS_TAG);
        assert_eq!(&bytes[1..], text);
        assert_eq!(Status::decode(&bytes)?, original);
    }
    // `dist_util.erl` sends the `nok` atom as the plain text `"nok"`.
    assert_eq!(body(|w| Status::Error(b"nok".to_vec()).encode(w)), b"snok");
    Ok(())
}

// ---------------------------------------------------------------------------
// StatusKind classification.
// ---------------------------------------------------------------------------

#[test]
fn status_kind_classifies_every_documented_code() -> Result<()> {
    // The status codes the OTP distribution guide defines
    // (`erts/doc/guides/erl_dist_protocol.md:533-575`), each with the text a real
    // node sends (`lib/kernel/src/dist_util.erl` `send_status` calls).
    assert_eq!(Status::Ok.kind(), StatusKind::Ok);
    assert_eq!(Status::Simultaneous.kind(), StatusKind::Simultaneous);
    assert_eq!(
        Status::Named { name: b"gurka@localhost".to_vec(), creation: 3 }.kind(),
        StatusKind::Named
    );
    assert_eq!(Status::Error(b"nok".to_vec()).kind(), StatusKind::Nok);
    assert_eq!(Status::Error(b"not_allowed".to_vec()).kind(), StatusKind::NotAllowed);
    assert_eq!(Status::Error(b"alive".to_vec()).kind(), StatusKind::Alive);
    // The step-3B `"true"`/`"false"` replies, an unknown code, and the empty text
    // all classify as `Other` — including `no_creation`/`system_limit`, which this
    // OTP release never sends as a status (`dist_util.erl:593` raises the latter
    // locally).
    assert_eq!(Status::Error(b"true".to_vec()).kind(), StatusKind::Other);
    assert_eq!(Status::Error(b"false".to_vec()).kind(), StatusKind::Other);
    assert_eq!(Status::Error(b"system_limit".to_vec()).kind(), StatusKind::Other);
    assert_eq!(Status::Error(Vec::new()).kind(), StatusKind::Other);

    // The classification is chosen by the wire text, so it survives a
    // decode round-trip for every dedicated variant.
    let cases = [
        (Status::Ok, StatusKind::Ok),
        (Status::Simultaneous, StatusKind::Simultaneous),
        (
            Status::Named { name: b"n@h".to_vec(), creation: 9 },
            StatusKind::Named,
        ),
        (Status::Error(b"nok".to_vec()), StatusKind::Nok),
        (Status::Error(b"not_allowed".to_vec()), StatusKind::NotAllowed),
        (Status::Error(b"alive".to_vec()), StatusKind::Alive),
        (Status::Error(b"alive?".to_vec()), StatusKind::Other),
    ];
    for (status, kind) in cases {
        let bytes = body(|w| status.encode(w));
        assert_eq!(Status::decode(&bytes)?.kind(), kind, "decoded {status:?}");
    }
    Ok(())
}

#[test]
fn error_kind_is_none_only_for_the_continuing_statuses() {
    // `dist_util.erl:1081-1082` continues the handshake after each of these.
    assert_eq!(Status::Ok.error_kind(), None);
    assert_eq!(Status::Simultaneous.error_kind(), None);
    assert_eq!(Status::Named { name: Vec::new(), creation: 0 }.error_kind(), None);

    assert_eq!(Status::Error(b"nok".to_vec()).error_kind(), Some(StatusKind::Nok));
    assert_eq!(
        Status::Error(b"not_allowed".to_vec()).error_kind(),
        Some(StatusKind::NotAllowed)
    );
    assert_eq!(Status::Error(b"alive".to_vec()).error_kind(), Some(StatusKind::Alive));
    assert_eq!(Status::Error(b"bogus".to_vec()).error_kind(), Some(StatusKind::Other));
}

#[test]
fn refusal_constructors_match_the_reference_texts() -> Result<()> {
    // The two refusal texts a real accepting node sends (`dist_util.erl:286`/`:343`).
    assert_eq!(body(|w| Status::nok().encode(w)), b"snok");
    assert_eq!(body(|w| Status::not_allowed().encode(w)), b"snot_allowed");
    assert_eq!(Status::nok().kind(), StatusKind::Nok);
    assert_eq!(Status::not_allowed().kind(), StatusKind::NotAllowed);
    assert_eq!(Status::decode(b"snot_allowed")?, Status::not_allowed());
    Ok(())
}
// ---------------------------------------------------------------------------
// Negatives: wrong tag, truncation, empty body.
// ---------------------------------------------------------------------------

#[test]
fn wrong_tag_is_bad_arg() {
    // A body whose tag byte is `'Z'` must be rejected by every decoder.
    let bad = [b'Z'; 64];
    assert!(matches!(Name::decode(&bad), Err(BeamError::BadArg)));
    assert!(matches!(Challenge::decode(&bad), Err(BeamError::BadArg)));
    assert!(matches!(ChallengeReply::decode(&bad), Err(BeamError::BadArg)));
    assert!(matches!(ChallengeAck::decode(&bad), Err(BeamError::BadArg)));
    assert!(matches!(Complement::decode(&bad), Err(BeamError::BadArg)));
    assert!(matches!(Status::decode(&bad), Err(BeamError::BadArg)));
}

#[test]
fn empty_body_is_bad_arg() {
    assert!(matches!(Name::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(Challenge::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(ChallengeReply::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(ChallengeAck::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(Complement::decode(&[]), Err(BeamError::BadArg)));
    assert!(matches!(Status::decode(&[]), Err(BeamError::BadArg)));
}

#[test]
fn truncated_fixed_fields_are_bad_arg() {
    let name = body(|w| {
        Name { flags: sample_flags(), creation: 7, name: b"a@b".to_vec() }.encode(w)
    });
    // Cut a fixed field (flags/creation) or the name off the end.
    for cut in 1..=name.len() {
        assert!(
            matches!(Name::decode(&name[..name.len() - cut]), Err(BeamError::BadArg)),
            "Name decode must reject a body with {cut} byte(s) removed"
        );
    }

    let challenge = body(|w| {
        Challenge { flags: sample_flags(), challenge: 1, creation: 2, name: b"a@b".to_vec() }
            .encode(w)
    });
    for cut in 1..=challenge.len() {        let truncated = &challenge[..challenge.len() - cut];
        assert!(
            matches!(Challenge::decode(truncated), Err(BeamError::BadArg)),
            "Challenge decode must reject a body with {cut} byte(s) removed"
        );
    }
    let reply = body(|w| ChallengeReply { challenge: 1, digest: DIGEST }.encode(w));
    for cut in 1..=reply.len() {
        assert!(matches!(
            ChallengeReply::decode(&reply[..reply.len() - cut]),
            Err(BeamError::BadArg)
        ));
    }

    let ack = body(|w| ChallengeAck { digest: DIGEST }.encode(w));
    for cut in 1..=ack.len() {
        assert!(matches!(ChallengeAck::decode(&ack[..ack.len() - cut]), Err(BeamError::BadArg)));
    }

    let complement = body(|w| Complement { flags_high: 1, creation: 2 }.encode(w));
    for cut in 1..=complement.len() {
        assert!(matches!(
            Complement::decode(&complement[..complement.len() - cut]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn name_length_beyond_body_is_bad_arg() {
    // 'N' + flags(8) + creation(4) + namelen(2) declaring 4096 bytes, then none.
    let mut bad = vec![b'N'];
    bad.extend_from_slice(&sample_flags().bits().to_be_bytes());
    bad.extend_from_slice(&0u32.to_be_bytes());
    bad.extend_from_slice(&4096u16.to_be_bytes());
    assert!(matches!(Name::decode(&bad), Err(BeamError::BadArg)));

    // Same for a challenge.
    let mut bad = vec![b'N'];
    bad.extend_from_slice(&sample_flags().bits().to_be_bytes());
    bad.extend_from_slice(&1u32.to_be_bytes());
    bad.extend_from_slice(&2u32.to_be_bytes());
    bad.extend_from_slice(&4096u16.to_be_bytes());
    assert!(matches!(Challenge::decode(&bad), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// Flag negotiation helpers.
// ---------------------------------------------------------------------------

#[test]
fn preferred_flags_covers_the_mandatory_set() {
    assert_eq!(preferred_flags().bits(), DFLAG_DIST_DEFAULT);
    assert!(preferred_flags().contains(DistFlags::from_bits(DFLAG_DIST_MANDATORY)));
    assert!(preferred_flags().contains(DistFlags::from_bits(DFLAG_MANDATORY_25_DIGEST)));
}

// ---------------------------------------------------------------------------
// Node visibility (DFLAG_PUBLISHED): `publish_flag/3` / `publish_type/1`.
// ---------------------------------------------------------------------------

/// GOLDEN-VECTOR: [`publish_flag`] mirrors `dist_util.erl:publish_flag/3`
/// (`dist_util.erl:150-160`) — `DFLAG_PUBLISHED` exactly for a normal connection
/// to a peer we publish to, and nothing otherwise.
#[test]
fn publish_flag_mirrors_the_ground_truth() {
    let none = DistFlags::EMPTY;
    let published = DistFlags::from_bits(DFLAG_PUBLISHED);

    // publish_flag(_, _, OtherNode) -> publish_on_node(OtherNode): true -> set.
    assert_eq!(publish_flag(false, true), published);
    // ...false -> 0.
    assert_eq!(publish_flag(false, false), none);
    // publish_flag(hidden, _, _) -> 0, regardless of publish_on_node.
    assert_eq!(publish_flag(true, true), none);
    assert_eq!(publish_flag(true, false), none);

    // The typed form is exactly the boolean form.
    assert_eq!(PublishKind::Normal.publish_flag(true), publish_flag(false, true));
    assert_eq!(PublishKind::Normal.publish_flag(false), publish_flag(false, false));
    assert_eq!(PublishKind::Hidden.publish_flag(true), publish_flag(true, true));
    assert_eq!(PublishKind::Hidden.publish_flag(false), publish_flag(true, false));

    // Exactly one bit is ever produced, and it is DFLAG_PUBLISHED (`0x01`).
    assert_eq!(publish_flag(false, true).bits(), DFLAG_PUBLISHED);
    assert_eq!(DFLAG_PUBLISHED, 0x01);
}

/// GOLDEN-VECTOR: [`publish_type`] mirrors `dist_util.erl:publish_type/1`
/// (`dist_util.erl:917-923`) — keyed **only** on the peer's `DFLAG_PUBLISHED`.
#[test]
fn publish_type_classifies_the_peer_by_dflag_published() {
    // The default set omits the bit (the emulator's `erts_dflags.default`,
    // `dist.h:118-129`), so a peer offering it is classified hidden.
    assert!(!preferred_flags().contains(DistFlags::from_bits(DFLAG_PUBLISHED)));
    assert_eq!(publish_type(preferred_flags()), PeerVisibility::Hidden);
    assert_eq!(publish_type(DistFlags::EMPTY), PeerVisibility::Hidden);

    // A peer that ORs the bit in is normal.
    let published = preferred_flags() | DistFlags::from_bits(DFLAG_PUBLISHED);
    assert_eq!(publish_type(published), PeerVisibility::Normal);

    // Only DFLAG_PUBLISHED matters: every other capability bit is ignored.
    let others = DistFlags::from_bits(DFLAG_DIST_DEFAULT & !DFLAG_PUBLISHED);
    assert_eq!(others.bits(), DFLAG_DIST_DEFAULT);
    assert_eq!(publish_type(others), PeerVisibility::Hidden);

    // Round-trip: the flag `publish_flag` produces is exactly what flips the
    // classification from hidden to normal.
    assert_eq!(preferred_flags() | publish_flag(false, true), published);
}

#[test]
fn check_mandatory_accepts_a_full_mandatory_word() -> Result<()> {
    let flags = DistFlags::from_bits(DFLAG_DIST_MANDATORY);
    let got = check_mandatory(flags)?;
    // The digest bit is absent, so nothing is OR'd in: the word is unchanged.
    assert_eq!(got.bits(), DFLAG_DIST_MANDATORY);
    assert!(got.contains(DistFlags::from_bits(DFLAG_DIST_MANDATORY)));
    Ok(())
}

#[test]
fn check_mandatory_applies_the_25_digest_rule() -> Result<()> {
    // A peer that signals the OTP-25 digest but does not set the individual
    // OTP-25 capability bits: `recv_challenge` ORs `DFLAG_DIST_MANDATORY_25`
    // in, so the message is accepted and the returned word carries the bits.
    let flags = DFLAG_MANDATORY_25_DIGEST | DFLAG_DIST_MANDATORY_26;
    let got = check_mandatory(DistFlags::from_bits(flags))?;
    assert_eq!(got.bits(), flags | DFLAG_DIST_MANDATORY_25);
    assert_eq!(got.bits(), DFLAG_MANDATORY_25_DIGEST | DFLAG_DIST_MANDATORY);
    assert!(got.contains(DistFlags::from_bits(DFLAG_DIST_MANDATORY)));

    // Without the adjustment this word would be rejected (it lacks the whole
    // OTP-25 mandatory set).
    assert!(matches!(
        check_mandatory(DistFlags::from_bits(DFLAG_DIST_MANDATORY_26)),
        Err(BeamError::BadArg)
    ));
    Ok(())
}

#[test]
fn check_mandatory_rejects_a_missing_mandatory_bit() {
    let missing_map_tag = DFLAG_DIST_MANDATORY & !DFLAG_MAP_TAG;
    assert!(matches!(
        check_mandatory(DistFlags::from_bits(missing_map_tag)),
        Err(BeamError::BadArg)
    ));
    // An empty word is likewise missing every mandatory flag.
    assert!(matches!(check_mandatory(DistFlags::EMPTY), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// Digest hook.
// ---------------------------------------------------------------------------

/// A deterministic stand-in for the (absent) MD5 provider.
struct FixedDigest;

impl ChallengeDigest for FixedDigest {
    fn digest(&self, cookie: &[u8], challenge: u32) -> [u8; 16] {
        let mut digest = [0u8; 16];
        digest[0] = cookie.len() as u8;
        digest[4..8].copy_from_slice(&challenge.to_be_bytes());
        digest
    }
}

#[test]
fn verify_digest_matches_and_rejects() {
    let d = FixedDigest;
    let cookie = b"cookie";
    let challenge = 0x1122_3344u32;
    let good = d.digest(cookie, challenge);

    assert!(verify_digest(&good, challenge, cookie, &d));

    // A different challenge derives a different digest.
    assert!(!verify_digest(&good, challenge + 1, cookie, &d));
    // A single flipped bit is rejected.
    let mut tampered = good;
    tampered[0] ^= 0x01;
    assert!(!verify_digest(&tampered, challenge, cookie, &d));
    // The all-zero digest is only accepted when it really is the derived one.
    assert!(!verify_digest(&[0u8; 16], challenge, cookie, &d));
}

// ---------------------------------------------------------------------------
// The single status -> BeamError mapping.
//
// `status_error` is the one implementation shared by the blocking `connection`
// API and the non-blocking `driver`; these cases pin its `io::ErrorKind`
// selection and that it never loses the peer's reported text / name.
// ---------------------------------------------------------------------------

#[test]
fn status_error_selects_the_io_kind_for_every_refusal() {
    use std::io::ErrorKind;

    let cases = [
        (Status::nok(), ErrorKind::ConnectionRefused),
        (Status::not_allowed(), ErrorKind::PermissionDenied),
        (Status::Error(b"alive".to_vec()), ErrorKind::ConnectionAborted),
        (
            Status::Named { name: b"gurka@localhost".to_vec(), creation: 3 },
            ErrorKind::AlreadyExists,
        ),
        (Status::Simultaneous, ErrorKind::ConnectionAborted),
        // A status that is neither a refusal nor a continuing code, and the
        // (unreachable-in-practice) `Ok`, both fall through to `InvalidData`.
        (Status::Error(b"weird".to_vec()), ErrorKind::InvalidData),
        (Status::Error(b"true".to_vec()), ErrorKind::InvalidData),
        (Status::Ok, ErrorKind::InvalidData),
    ];

    for (status, expected) in cases {
        match status_error(&status) {
            BeamError::Io(io) => {
                assert_eq!(io.kind(), expected, "status {status:?}");
                assert!(
                    io.to_string().contains("distribution handshake"),
                    "the message names the context: {io}"
                );
            }
            other => panic!("expected an Io error for {status:?}, got {other:?}"),
        }
    }
}

#[test]
fn status_error_keeps_the_peer_text_name_and_creation() {
    match status_error(&Status::Error(b"nok_cookie".to_vec())) {
        BeamError::Io(io) => {
            let msg = io.to_string();
            assert!(msg.contains("nok_cookie"), "carries the peer's status text: {msg}");
            // `nok_cookie` is not the exact `"nok"` code, so it classifies `Other`.
            assert!(msg.contains("Other"), "names the kind: {msg}");
        }
        other => panic!("expected Io, got {other:?}"),
    }

    match status_error(&Status::nok()) {
        BeamError::Io(io) => {
            let msg = io.to_string();
            assert!(msg.contains("nok"), "carries the peer's status text: {msg}");
            assert!(msg.contains("Nok"), "names the kind: {msg}");
        }
        other => panic!("expected Io, got {other:?}"),
    }

    match status_error(&Status::Named { name: b"gurka@localhost".to_vec(), creation: 7 }) {
        BeamError::Io(io) => {
            let msg = io.to_string();
            assert!(msg.contains("gurka@localhost"), "names the already-assigned node: {msg}");
            assert!(msg.contains('7'), "names its creation: {msg}");
            assert!(msg.contains("Named"), "names the kind: {msg}");
        }
        other => panic!("expected Io, got {other:?}"),
    }
}
