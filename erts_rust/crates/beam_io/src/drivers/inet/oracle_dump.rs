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

//! The **length-framed** transfer format the `erl`-oracle dumps use, and its Rust decoder.
//!
//! An oracle dump must be framed — either by a length prefix (this module: a 4-byte big-endian
//! byte count before every reply) or as ASCII hex/decimal **text**. What it must never be is a
//! raw-byte stream split on a delimiter: **any** byte can occur inside a reply. A `bind` reply
//! carries the kernel-assigned port (`[INET_REP_OK, port_hi, port_lo]`), an errno reply carries
//! a lowercase POSIX name, an option reply carries a stored value — so a reply whose payload
//! happens to contain the delimiter byte is silently split into the wrong parts. That is not
//! hypothetical: the `udp_inet` suite split on `|` (`0x7C`) and flaked roughly once in 256 runs,
//! because `port_lo == 0x7C` is a perfectly legal ephemeral port byte
//! (`/proc/sys/net/ipv4/ip_local_port_range` is `32768 60999`, so `port_hi` never reaches it but
//! `port_lo` does). The split produced the right **number** of parts and a two-byte middle part,
//! i.e. a `left: 3 / right: 2` panic with a correct Rust reply.
//!
//! The Erlang half is [`ERL_FRAME_ELEMENT`], spliced into an oracle expression as
//! `Out = iolist_to_binary([<<(byte_size(X)):32/big, X/binary>> || X <- R])`; [`decode`] is its
//! inverse and [`encode`] builds dumps (for the adversarial cases below, and for any other suite
//! that needs a synthetic dump).

/// The Erlang list-comprehension element that frames one reply: a 4-byte big-endian byte count
/// followed by the reply's bytes. The Rust counterpart is [`decode`] — keep the two in step, which
/// is why the Erlang side is spliced from this constant rather than written out again per suite.
pub(super) const ERL_FRAME_ELEMENT: &str = "<<(byte_size(X)):32/big, X/binary>>";

/// Decodes an oracle dump framed by [`ERL_FRAME_ELEMENT`] into one byte vector per reply.
///
/// An empty dump decodes to an empty vector — a legitimate "the oracle printed nothing", which
/// the caller's own reply-count assertion then reports. A **truncated** dump (fewer than four
/// bytes where a length word belongs, or a body shorter than its own length word claims) is
/// `None`: that dump cannot be attributed to replies, and guessing would hide a broken oracle
/// run.
pub(super) fn decode(dump: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut replies = Vec::new();
    let mut rest = dump;
    while !rest.is_empty() {
        let header = rest.get(..4)?;
        let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let body = rest.get(4..)?;
        if body.len() < len {
            return None;
        }
        replies.push(body[..len].to_vec());
        rest = &body[len..];
    }
    Some(replies)
}

/// Encodes `replies` into a dump framed the way [`ERL_FRAME_ELEMENT`] frames it — the inverse of
/// [`decode`], used to build the adversarial dumps the suite below pins.
pub(super) fn encode(replies: &[&[u8]]) -> Vec<u8> {
    let mut dump = Vec::new();
    for reply in replies {
        let len = u32::try_from(reply.len()).expect("a reply fits a 32-bit length word");
        dump.extend_from_slice(&len.to_be_bytes());
        dump.extend_from_slice(reply);
    }
    dump
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, ERL_FRAME_ELEMENT};

    /// The delimiter the `udp_inet` suite used to split on, and the byte that made it flake.
    const OLD_DELIMITER: u8 = b'|';

    #[test]
    fn a_round_trip_preserves_every_reply() {
        let cases: [&[&[u8]]; 4] = [
            &[],
            &[&[1]],
            &[&[1, 0, 0], &[], &[0], &[255, 254]],
            &[&[1, 0x7D, 0x7C], &[1]],
        ];
        for replies in cases {
            let dump = encode(replies);
            let decoded = decode(&dump).expect("a self-built dump always decodes");
            assert_eq!(decoded.len(), replies.len(), "one reply per frame: {dump:02x?}");
            for (index, (decoded_reply, expected)) in decoded.iter().zip(replies.iter()).enumerate()
            {
                assert_eq!(
                    decoded_reply.as_slice(),
                    *expected,
                    "reply {index} of {dump:02x?}",
                );
            }
        }
    }

    #[test]
    fn an_empty_dump_decodes_to_no_replies() {
        assert_eq!(encode(&[]), Vec::<u8>::new());
        assert_eq!(decode(&[]), Some(Vec::new()));
    }

    #[test]
    fn the_frame_is_a_big_endian_length_word_then_the_reply() {
        // The frame is exactly the Erlang element's shape: four big-endian length bytes, then
        // the reply. Pin it literally so the Erlang string above cannot drift from this decoder.
        assert_eq!(ERL_FRAME_ELEMENT, "<<(byte_size(X)):32/big, X/binary>>");
        assert_eq!(
            encode(&[&[0xAB, 0xCD]]),
            vec![0, 0, 0, 2, 0xAB, 0xCD],
            "a two-byte reply is framed by a big-endian 2",
        );
        assert_eq!(
            encode(&[&[], &[1]]),
            vec![0, 0, 0, 0, 0, 0, 0, 1, 1],
            "an empty reply is a zero-length frame, not a skipped one",
        );
    }

    #[test]
    fn a_reply_containing_the_old_delimiter_decodes_intact() {
        let dump = encode(&[&[1, 0x7D, OLD_DELIMITER], &[1]]);
        let decoded = decode(&dump).expect("decodes");
        assert_eq!(
            decoded,
            vec![vec![1, 0x7D, OLD_DELIMITER], vec![1]],
            "the delimiter byte is payload, never a separator: {dump:02x?}",
        );
    }

    #[test]
    fn a_reply_that_is_exactly_the_old_delimiter_decodes_intact() {
        let dump = encode(&[&[OLD_DELIMITER]]);
        assert_eq!(
            dump,
            vec![0, 0, 0, 1, OLD_DELIMITER],
            "a one-byte reply of the delimiter",
        );
        assert_eq!(decode(&dump), Some(vec![vec![OLD_DELIMITER]]));
    }

    #[test]
    fn empty_replies_are_frames_not_gaps() {
        // An empty reply between two non-empty ones — the shape that a `.filter(|p|
        // !p.is_empty())` on a delimiter split silently drops.
        let dump = encode(&[&[1], &[], &[1]]);
        assert_eq!(
            decode(&dump),
            Some(vec![vec![1], vec![], vec![1]]),
            "three frames, the middle one empty: {dump:02x?}",
        );
        // Trailing empties too.
        let dump = encode(&[&[1], &[], &[]]);
        assert_eq!(decode(&dump), Some(vec![vec![1], vec![], vec![]]));
    }

    #[test]
    fn a_reply_containing_a_nul_byte_decodes_intact() {
        // `0x00` is as legal a reply byte as `0x7C`: a `flush`-style reply or a zero-valued
        // option record is dense with it.
        let reply = [0u8, 0, 0, 0, 0, 0, 1, 0];
        let dump = encode(&[&reply]);
        assert_eq!(decode(&dump), Some(vec![reply.to_vec()]));
    }

    #[test]
    fn a_body_that_starts_like_a_length_word_is_not_misread() {
        // The body begins with `00 00 00 01`, i.e. it looks exactly like the frame header of a
        // one-byte reply. Only the outer length word decides, so both parts survive whole.
        let first = [0u8, 0, 0, 1, 0x41];
        let second = [0x42u8];
        let dump = encode(&[&first, &second]);
        assert_eq!(
            decode(&dump),
            Some(vec![first.to_vec(), second.to_vec()]),
            "{dump:02x?}",
        );
        // And a body claiming the largest possible length is still just four payload bytes.
        let huge = [0xFFu8, 0xFF, 0xFF, 0xFF];
        assert_eq!(decode(&encode(&[&huge])), Some(vec![huge.to_vec()]));
    }

    #[test]
    fn a_truncated_dump_is_refused() {
        // A trailing partial header: one complete frame plus three stray bytes.
        let mut dump = encode(&[&[1, 2, 3]]);
        assert!(decode(&dump).is_some());
        dump.extend_from_slice(&[0, 0, 0]);
        assert_eq!(decode(&dump), None, "a three-byte tail is not a header");

        // A lone partial header, nothing else.
        assert_eq!(decode(&[0, 0]), None);
        // A header claiming more bytes than the dump holds.
        assert_eq!(decode(&[0, 0, 0, 5, 1, 2]), None, "the body is short");
        // A zero-length frame followed by a short header.
        assert_eq!(decode(&[0, 0, 0, 0, 0]), None);
    }

    #[test]
    fn the_original_failing_shape_decodes_to_three_replies() {
        // The exact observed pre-fix dump for `open` / `bind` / `close` with an explicit port
        // `0x7D7C`: the oracle printed the replies joined by `0x7C`, giving the 8 bytes below.
        // Split on `0x7C` that yields `[01]`, `[01,7D]`, `[]`, `[01]`, `[]` — three replies
        // after the empty parts are dropped, with a **two**-byte `bind` where the driver answers
        // three (`left: 3 / right: 2`).
        let delimiter_split = [
            &[1u8][..],
            &[1, 0x7D][..],
            &[][..],
            &[1][..],
            &[][..],
        ];
        assert_eq!(
            delimiter_split.len(),
            5,
            "the delimiter split yields five parts, two of them empty",
        );
        let old_shape: Vec<Vec<u8>> = delimiter_split
            .iter()
            .filter(|part| !part.is_empty())
            .map(|part| part.to_vec())
            .collect();
        assert_eq!(old_shape.len(), 3, "so the reply **count** assertion passed");
        assert_eq!(
            old_shape[1].len(),
            2,
            "while the bind reply lost its final byte to the delimiter",
        );

        // The framed dump keeps all three, the middle one three bytes wide.
        let dump = encode(&[&[1], &[1, 0x7D, 0x7C], &[1]]);
        let decoded = decode(&dump).expect("decodes");
        assert_eq!(decoded.len(), 3, "three frames: {dump:02x?}");
        assert_eq!(decoded[0], vec![1]);
        assert_eq!(
            decoded[1],
            vec![1, 0x7D, 0x7C],
            "the bind reply's port low byte is payload: {dump:02x?}",
        );
        assert_eq!(decoded[1].len(), 3);
        assert_eq!(decoded[2], vec![1]);
    }
}
