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

//! Fragmented distribution messages: the `'E'` fragment header, the `'F'`
//! continuation, sender splitting and receiver reassembly.
//!
//! Ground truth (read, never modified): `erts/emulator/beam/external.c`
//! (`erts_encode_ext_dist_header_setup`/`_fragment`/`_finalize`,
//! `erts_prepare_dist_ext`), `erts/emulator/beam/dist.c` (`ERTS_DSIG_SEND_PHASE_FIN`,
//! the `ERTS_PREP_DIST_EXT_FRAG_CONT` re-assembly), `erts/emulator/beam/dist.h`
//! ([`DIST_FRAGMENT_SIZE`] / [`DIST_FRAGMENT_HEADER_SIZE`]) and the authoritative
//! wire description in `erts/doc/guides/erl_ext_dist.md`
//! §"Distribution Header for fragmented messages".
//!
//! # The wire format (byte-exact, big-endian)
//!
//! Fragmentation (OTP-22+, negotiated via `DFLAG_FRAGMENTS`) splits a single
//! distribution *message* into a run of length-framed packets. The first packet
//! carries an `'E'` fragment header; each following packet carries an `'F'`
//! continuation:
//!
//! ```text
//! first frame:        VERSION_MAGIC(131)  'E'(69)  SequenceId(8)  FragmentId(8)  <normal header tail>  <payload chunk>
//! continuation frame: VERSION_MAGIC(131)  'F'(70)  SequenceId(8)  FragmentId(8)                        <payload chunk>
//! ```
//!
//! * The **8-byte** `SequenceId` and `FragmentId` fields are big-endian and are
//!   written *before* the normal header tail (see the worked example in
//!   `erl_ext_dist.md`). In ERTS the sequence id is really the sender's pid term
//!   (`external.c`: `ASSERT(is_pid(from))`); here it is treated as an opaque
//!   `u64`. **Caveat:** the real ERTS receiver treats `SequenceId == 0` as "not
//!   fragmented" (`erts_prepare_dist_ext`), so a peer that must interoperate with
//!   real ERTS has to use a non-zero sequence id.
//! * The normal header tail that follows the `'E'` prefix (`NumberOfAtomCacheRefs
//!   + Flags + AtomCacheRefs`) is **identical** to the tail of a `'D'` header; see
//!   [`crate::dist_header`]. There is no separate `dist_hdr_flags` byte on the
//!   wire — `external.c` folds it into the `Flags` half-bytes in
//!   `erts_encode_ext_dist_header_finalize`.
//! * The `FragmentId` in the `'E'` header is the **total number of fragments**.
//!   The continuation ids then *count down* to `1` (the final fragment), i.e. the
//!   `k`-th continuation (`k` = 1, 2, …) carries `count - k` —
//!   `dist.c` uses `erts_encode_ext_dist_header_fragment(…, --fid, …)` and the
//!   receiver expects `count, count-1, …, 1` in order.
//!
//! # Scope
//!
//! `erts/emulator/beam/dist.c` shows that ERTS keeps the whole atom cache *and*
//! the whole control message inside the first fragment ("it is not allowed to
//! fragment the message up until here", `erl_ext_dist.md`); only the trailing
//! message payload is cut into [`DIST_FRAGMENT_SIZE`]-byte chunks. [`split`]
//! reproduces that boundary: everything passed as `header_tail` lands unsplit in
//! the first fragment and `payload` is chunked from there — so a caller that
//! wants a byte-for-byte identical ERTS split passes the control message as part
//! of `header_tail` and only the message term as `payload`.

use beam_codec::AtomEncoding;
use beam_common::{BeamError, Result};
use beam_term::AtomTable;

use crate::dist_header::{DistHeader, DIST_FRAG_CONT, DIST_FRAG_HEADER};

/// `ERTS_DIST_FRAGMENT_SIZE` (`erts/emulator/beam/dist.h`): the payload chunk a
/// single fragment carries — `64 * 1024` bytes in a normal build (`1024` under
/// `#ifdef DEBUG`).
pub const DIST_FRAGMENT_SIZE: usize = 64 * 1024;

/// `ERTS_DIST_FRAGMENT_HEADER_SIZE` = `1 + 1 + 8 + 8`: `VERSION_MAGIC`, the
/// fragment tag byte, the 8-byte sequence id and the 8-byte fragment id.
///
/// This is the size of *both* the `'E'` prefix and the `'F'` prefix, and it is
/// the offset of the normal header tail (for `'E'`) or the payload (for `'F'`)
/// within a fragment packet body.
pub const DIST_FRAGMENT_HEADER_SIZE: usize = 1 + 1 + 8 + 8;

/// Default cap on the number of bytes a [`FragmentAssembler`] will buffer for a
/// single fragmented sequence before rejecting further fragments with
/// `BeamError::SystemLimit` (never an unbounded allocation).
pub const DEFAULT_MAX_FRAGMENTED_MESSAGE: usize = 64 * 1024 * 1024;

/// The `'E'` fragment-header prefix: `VERSION_MAGIC + 'E' + SequenceId(8) +
/// FragmentId(8)`.
///
/// It precedes the normal distribution-header tail in the first fragment of a
/// sequence; the tail itself is decoded by
/// [`crate::dist_header::decode_fragmented_header`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FragmentHeader {
    /// The 8-byte sequence id (the sender's pid on real ERTS).
    pub seq_id: u64,
    /// The 8-byte fragment id: the **total number of fragments** in the sequence.
    pub fragment_count: u64,
}

impl FragmentHeader {
    /// Writes the prefix `VERSION_MAGIC + 'E' + seq_id(8) + fragment_count(8)`.
    pub fn encode<W: crate::transport::Write>(&self, w: &mut W) -> Result<()> {
        w.write_all(&prefix_bytes(DIST_FRAG_HEADER, self.seq_id, self.fragment_count))
    }

    /// Decodes the prefix from the start of `body`.
    ///
    /// Errors: a body shorter than [`DIST_FRAGMENT_HEADER_SIZE`], a leading byte
    /// that is not `VERSION_MAGIC`, or a second byte that is not `'E'`, yields
    /// `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<FragmentHeader> {
        let (seq_id, field) = decode_prefix(body, DIST_FRAG_HEADER)?;
        Ok(FragmentHeader { seq_id, fragment_count: field })
    }
}

/// The `'F'` continuation prefix: `VERSION_MAGIC + 'F' + SequenceId(8) +
/// FragmentId(8)`.
///
/// It is followed directly by the raw payload chunk of that fragment.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FragmentCont {
    /// The 8-byte sequence id, which must match the sequence's first fragment.
    pub seq_id: u64,
    /// The 8-byte fragment id of this continuation.
    ///
    /// Following ERTS (`dist.c`: `erts_encode_ext_dist_header_fragment(…,
    /// --fid, …)`), this counts **down**: the first continuation is
    /// `fragment_count - 1` and the last one is `1` (never a 0-based index).
    pub fragment_index: u64,
}

impl FragmentCont {
    /// Writes the prefix `VERSION_MAGIC + 'F' + seq_id(8) + fragment_index(8)`.
    pub fn encode<W: crate::transport::Write>(&self, w: &mut W) -> Result<()> {
        w.write_all(&prefix_bytes(DIST_FRAG_CONT, self.seq_id, self.fragment_index))
    }

    /// Decodes the prefix from the start of `body`.
    ///
    /// Errors: a body shorter than [`DIST_FRAGMENT_HEADER_SIZE`], a leading byte
    /// that is not `VERSION_MAGIC`, or a second byte that is not `'F'`, yields
    /// `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<FragmentCont> {
        let (seq_id, field) = decode_prefix(body, DIST_FRAG_CONT)?;
        Ok(FragmentCont { seq_id, fragment_index: field })
    }
}

/// Builds an 18-byte fragment prefix: `VERSION_MAGIC, tag, seq_id(8 BE),
/// field(8 BE)`.
fn prefix_bytes(tag: u8, seq_id: u64, field: u64) -> [u8; DIST_FRAGMENT_HEADER_SIZE] {
    let mut out = [0u8; DIST_FRAGMENT_HEADER_SIZE];
    out[0] = beam_codec::VERSION_MAGIC;
    out[1] = tag;
    out[2..10].copy_from_slice(&seq_id.to_be_bytes());
    out[10..18].copy_from_slice(&field.to_be_bytes());
    out
}

/// Decodes a fragment prefix with the expected tag, returning `(seq_id, field)`.
fn decode_prefix(body: &[u8], expected_tag: u8) -> Result<(u64, u64)> {
    if body.len() < DIST_FRAGMENT_HEADER_SIZE
        || body[0] != beam_codec::VERSION_MAGIC
        || body[1] != expected_tag
    {
        return Err(BeamError::BadArg);
    }
    let seq_id = u64::from_be_bytes(body[2..10].try_into().expect("8 bytes"));
    let field = u64::from_be_bytes(body[10..18].try_into().expect("8 bytes"));
    Ok((seq_id, field))
}

/// Splits one distribution message body into fragment packet-bodies.
///
/// The first returned body is `VERSION_MAGIC + 'E' + seq_id(8) + count(8) +
/// header_tail + payload[0..k]`; every later body is `VERSION_MAGIC + 'F' +
/// seq_id(8) + (count - i)(8) + payload[i*k..(i+1)*k]`, where `k` is
/// `fragment_size` (byte-exact with ERTS; see the module docs).
///
/// `header_tail` — the distribution-header tail that a non-fragmented message
/// would carry after `VERSION_MAGIC + 'D'`, for example `NumberOfAtomCacheRefs +
/// Flags + AtomCacheRefs` from [`crate::dist_header::DistHeader`] — is placed
/// verbatim in the first fragment and is *not* cut by the chunking.
///
/// The number of fragments is `ceil(payload.len() / fragment_size)`, but at least
/// `1` (an empty payload yields a single `'E'` frame with no continuation).
/// `payload` is chunked on `fragment_size`-byte boundaries; each payload chunk
/// except the last is exactly `fragment_size` bytes.
///
/// Errors: `fragment_size == 0` yields `BeamError::BadArg`; an empty
/// `fragment_size` is otherwise the only failure (the byte counts involved all
/// fit comfortably).
///
/// Note: ERTS additionally keeps the whole control message in the first fragment.
/// Because this signature has no separate control-message argument, a caller that
/// needs a byte-for-byte identical ERTS split should pass `header_tail` = the
/// distribution-header tail **and the encoded control message**, and `payload` =
/// the encoded message term only.
pub fn split(
    seq_id: u64,
    header_tail: &[u8],
    payload: &[u8],
    fragment_size: usize,
) -> Result<Vec<Vec<u8>>> {
    if fragment_size == 0 {
        return Err(BeamError::BadArg);
    }

    // `payload.chunks` yields nothing for an empty payload, but a fragment
    // sequence always carries at least the `'E'` frame, so normalise to one chunk.
    let chunks: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(fragment_size).collect()
    };
    let count = chunks.len() as u64;

    let mut out = Vec::with_capacity(chunks.len());
    for (i, chunk) in chunks.iter().enumerate() {
        let mut body = Vec::with_capacity(DIST_FRAGMENT_HEADER_SIZE + header_tail.len() + chunk.len());
        if i == 0 {
            body.extend_from_slice(&prefix_bytes(DIST_FRAG_HEADER, seq_id, count));
            body.extend_from_slice(header_tail);
        } else {
            // Continuation ids count down: count-1, count-2, …, 1.
            body.extend_from_slice(&prefix_bytes(DIST_FRAG_CONT, seq_id, count - i as u64));
        }
        body.extend_from_slice(chunk);
        out.push(body);
    }
    Ok(out)
}

/// The outcome of feeding one packet body to a [`FragmentAssembler::push`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Assembly {
    /// The packet is not part of a fragment sequence — a normal `'D'` header or a
    /// pass-through message. The caller handles the body as-is.
    NotFragmented,
    /// A fragment was buffered; more fragments are awaited for this sequence.
    Pending,
    /// The sequence is complete: the reassembled message in non-fragmented form,
    /// `VERSION_MAGIC + 'D' + <header tail> + <full payload>`, ready to be handed
    /// to [`crate::dist_header::DistHeader::decode`].
    Complete(Vec<u8>),
}

/// The in-flight fragmented sequence buffered by a [`FragmentAssembler`].
#[derive(Clone, Debug)]
struct Sequence {
    seq_id: u64,
    /// The next fragment id expected on the wire (`count-1`, …, `1`).
    next_expected: u64,
    /// The raw header tail (`NumberOfAtomCacheRefs + Flags + AtomCacheRefs`) from
    /// the `'E'` frame, verbatim.
    header_tail: Vec<u8>,
    /// The payload accumulated so far (first chunk plus every continuation).
    payload: Vec<u8>,
}

impl Sequence {
    /// The number of bytes currently buffered for this sequence.
    fn buffered(&self) -> usize {
        self.header_tail.len() + self.payload.len()
    }
}

/// Reassembles fragmented distribution messages (`'E'` + `'F'` frames).
///
/// Feed each received packet body to [`push`](FragmentAssembler::push); the
/// returned [`Assembly`] says whether the packet was unrelated (`NotFragmented`),
/// buffered (`Pending`) or completed (`Complete`).
///
/// # Guarantees
///
/// * Fragments must arrive **in order**: the first `'F'` frame must carry
///   `count - 1`, then `count - 2`, down to `1`. Any other id — an out-of-order
///   fragment, a duplicate, or a count mismatch — is rejected with
///   `BeamError::BadArg`. This mirrors `dist.c` ("Verify that the fragments have
///   arrived in the correct order").
/// * A continuation before the matching `'E'` frame, a continuation whose
///   sequence id differs from the in-flight one, or a second `'E'` frame while a
///   sequence is still pending, is rejected with `BeamError::BadArg`.
/// * The bytes buffered for one sequence are bounded by
///   [`max_message_size`](FragmentAssembler::with_limit); exceeding it yields
///   `BeamError::SystemLimit` (never an unbounded allocation).
///
/// Only **one** sequence is tracked at a time: `erl_ext_dist.md` requires a sender
/// to finish a fragmented message before starting another on the same channel, so
/// a second concurrent `'E'` is a protocol violation. Non-fragmented messages may
/// freely interleave and are reported as [`Assembly::NotFragmented`].
#[derive(Clone, Debug)]
pub struct FragmentAssembler {
    state: Option<Sequence>,
    max_message_size: usize,
    encoding: AtomEncoding,
}

impl Default for FragmentAssembler {
    fn default() -> FragmentAssembler {
        FragmentAssembler::new()
    }
}

impl FragmentAssembler {
    /// A new assembler with the default buffering cap
    /// ([`DEFAULT_MAX_FRAGMENTED_MESSAGE`]) and UTF-8 atom text.
    pub fn new() -> FragmentAssembler {
        FragmentAssembler::with_limit(DEFAULT_MAX_FRAGMENTED_MESSAGE)
    }

    /// A new assembler that rejects a sequence once more than `max_message_size`
    /// bytes (header tail + payload) have been buffered, with
    /// `BeamError::SystemLimit`.
    pub fn with_limit(max_message_size: usize) -> FragmentAssembler {
        FragmentAssembler {
            state: None,
            max_message_size,
            encoding: AtomEncoding::Utf8,
        }
    }

    /// A new assembler with an explicit buffering cap and the [`AtomEncoding`]
    /// used to decode the atom-cache tail of an `'E'` frame (the
    /// `DFLAG_UTF8_ATOMS` negotiation).
    ///
    /// The encoding only affects how atom text is validated — the *length* of the
    /// tail, and the resulting byte offsets, are the same either way.
    pub fn with_encoding(max_message_size: usize, encoding: AtomEncoding) -> FragmentAssembler {
        FragmentAssembler { state: None, max_message_size, encoding }
    }

    /// Whether a fragmented sequence is currently buffered.
    pub fn is_pending(&self) -> bool {
        self.state.is_some()
    }

    /// Drops any buffered sequence (for example after the connection is reset).
    pub fn reset(&mut self) {
        self.state = None;
    }

    /// Feeds one packet body into the assembler.
    ///
    /// Recognises an `'E'` first fragment and `'F'` continuations from their
    /// `VERSION_MAGIC + tag` prefix; anything else (a `'D'` header or a
    /// pass-through message) is reported as [`Assembly::NotFragmented`] and is not
    /// inspected further.
    pub fn push(&mut self, packet_body: &[u8]) -> Result<Assembly> {
        if packet_body.len() >= 2 && packet_body[0] == beam_codec::VERSION_MAGIC {
            match packet_body[1] {
                DIST_FRAG_HEADER => return self.push_first(packet_body),
                DIST_FRAG_CONT => return self.push_continuation(packet_body),
                _ => {}
            }
        }
        Ok(Assembly::NotFragmented)
    }

    /// Handles an `'E'` first fragment.
    fn push_first(&mut self, body: &[u8]) -> Result<Assembly> {
        if self.state.is_some() {
            // A sequence is already in flight; starting a new one on the same
            // channel is forbidden (erl_ext_dist.md §fragments).
            return Err(BeamError::BadArg);
        }
        let fragment = FragmentHeader::decode(body)?;
        if fragment.fragment_count == 0 {
            return Err(BeamError::BadArg);
        }

        // Decode the shared header tail to find where this fragment's payload
        // begins. The tail length does not depend on the atom table contents, so
        // a throwaway table (bounded by the fragment) is enough here.
        let mut atoms = AtomTable::new();
        let (_, payload_offset) =
            DistHeader::decode_tail(body, DIST_FRAGMENT_HEADER_SIZE, &mut atoms, self.encoding)?;
        let header_tail = body[DIST_FRAGMENT_HEADER_SIZE..payload_offset].to_vec();
        let chunk = &body[payload_offset..];

        if header_tail.len() + chunk.len() > self.max_message_size {
            return Err(BeamError::SystemLimit);
        }

        if fragment.fragment_count == 1 {
            // The single fragment is also the last one.
            return Ok(Assembly::Complete(assemble(header_tail, chunk.to_vec())));
        }

        self.state = Some(Sequence {
            seq_id: fragment.seq_id,
            next_expected: fragment.fragment_count - 1,
            header_tail,
            payload: chunk.to_vec(),
        });
        Ok(Assembly::Pending)
    }

    /// Handles an `'F'` continuation.
    fn push_continuation(&mut self, body: &[u8]) -> Result<Assembly> {
        let cont = FragmentCont::decode(body)?;
        let seq = self.state.as_mut().ok_or(BeamError::BadArg)?;
        if cont.seq_id != seq.seq_id {
            return Err(BeamError::BadArg);
        }
        if cont.fragment_index != seq.next_expected {
            // Out of order, a duplicate, or a count mismatch.
            return Err(BeamError::BadArg);
        }

        let chunk = &body[DIST_FRAGMENT_HEADER_SIZE..];
        if seq.buffered() + chunk.len() > self.max_message_size {
            return Err(BeamError::SystemLimit);
        }
        seq.payload.extend_from_slice(chunk);

        if cont.fragment_index == 1 {
            // Last fragment: emit the reassembled non-fragmented message body.
            let seq = self.state.take().expect("a sequence is pending");
            return Ok(Assembly::Complete(assemble(seq.header_tail, seq.payload)));
        }
        seq.next_expected -= 1;
        Ok(Assembly::Pending)
    }
}

/// Frames the reassembled message as a non-fragmented `'D'` body:
/// `VERSION_MAGIC + 'D' + header_tail + payload`.
fn assemble(header_tail: Vec<u8>, payload: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + header_tail.len() + payload.len());
    out.push(beam_codec::VERSION_MAGIC);
    out.push(crate::dist_header::DIST_HEADER);
    out.extend_from_slice(&header_tail);
    out.extend_from_slice(&payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::Write;

    /// A `Vec<u8>`-backed [`Write`] sink for the in-memory prefix codecs.
    #[derive(Default)]
    struct Sink(Vec<u8>);

    impl Write for Sink {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            self.0.extend_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn encode_prefix<F>(f: F) -> Vec<u8>
    where
        F: FnOnce(&mut Sink) -> Result<()>,
    {
        let mut sink = Sink::default();
        f(&mut sink).expect("encoding into a Vec cannot fail");
        sink.0
    }

    fn bad_arg<T: std::fmt::Debug>(res: Result<T>) -> bool {
        matches!(res, Err(BeamError::BadArg))
    }

    // The sequence id / fragment id from the worked example in
    // `erts/doc/guides/erl_ext_dist.md` §"Distribution Header for fragmented
    // messages": `131,69,0,0,2,168,0,0,5,83,0,0,0,0,0,0,0,2`.
    const DOC_SEQ_ID: u64 = 0x0000_02A8_0000_0553;
    const DOC_SEQ_BYTES: [u8; 8] = [0, 0, 2, 168, 0, 0, 5, 83];

    #[test]
    fn fragment_header_prefix_is_byte_exact() {
        let bytes = encode_prefix(|w| {
            FragmentHeader { seq_id: DOC_SEQ_ID, fragment_count: 2 }.encode(w)
        });
        assert_eq!(bytes.len(), DIST_FRAGMENT_HEADER_SIZE);
        assert_eq!(bytes.len(), 18);
        assert_eq!(bytes[0], beam_codec::VERSION_MAGIC);
        assert_eq!(bytes[0], 131);
        assert_eq!(bytes[1], DIST_FRAG_HEADER);
        assert_eq!(bytes[1], 69);
        // Offsets 2..10 are the big-endian sequence id.
        assert_eq!(&bytes[2..10], &DOC_SEQ_BYTES);
        // Offsets 10..18 are the big-endian fragment count.
        assert_eq!(&bytes[10..18], &[0, 0, 0, 0, 0, 0, 0, 2]);
        assert_eq!(
            bytes,
            vec![131, 69, 0, 0, 2, 168, 0, 0, 5, 83, 0, 0, 0, 0, 0, 0, 0, 2]
        );
    }

    #[test]
    fn fragment_cont_prefix_is_byte_exact() {
        let bytes = encode_prefix(|w| {
            FragmentCont { seq_id: DOC_SEQ_ID, fragment_index: 1 }.encode(w)
        });
        assert_eq!(bytes.len(), 18);
        assert_eq!(bytes[0], 131);
        assert_eq!(bytes[1], DIST_FRAG_CONT);
        assert_eq!(bytes[1], 70);
        assert_eq!(&bytes[2..10], &DOC_SEQ_BYTES);
        assert_eq!(&bytes[10..18], &[0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn prefixes_round_trip_and_reject_wrong_tags() {
        let src = FragmentHeader { seq_id: 42, fragment_count: 7 };
        let bytes = encode_prefix(|w| src.encode(w));
        assert_eq!(FragmentHeader::decode(&bytes).unwrap(), src);
        // A continuation tag is not an `'E'`.
        let cont = encode_prefix(|w| FragmentCont { seq_id: 42, fragment_index: 1 }.encode(w));
        assert!(bad_arg(FragmentHeader::decode(&cont)));
        assert!(bad_arg(FragmentCont::decode(&bytes)));

        let src = FragmentCont { seq_id: 9_999, fragment_index: 3 };
        let bytes = encode_prefix(|w| src.encode(w));
        assert_eq!(FragmentCont::decode(&bytes).unwrap(), src);

        // Truncated / wrong magic.
        assert!(bad_arg(FragmentHeader::decode(&[])));
        assert!(bad_arg(FragmentHeader::decode(&[131, 69, 0, 0])));
        let mut wrong_magic = encode_prefix(|w| src.encode(w));
        wrong_magic[0] = 130;
        assert!(bad_arg(FragmentCont::decode(&wrong_magic)));
    }

    #[test]
    fn split_and_reassemble_a_small_payload() {
        // header_tail = a bare `NumberOfAtomCacheRefs = 0` tail.
        let header_tail = [0u8];
        let payload = b"hello world";
        let frags = split(7, &header_tail, payload, 4).expect("split");
        // 11 bytes / 4 = 3 fragments (4, 4, 3).
        assert_eq!(frags.len(), 3);

        // Fragment 0: `'E'` + tail + first chunk.
        assert_eq!(frags[0][0], 131);
        assert_eq!(frags[0][1], DIST_FRAG_HEADER);
        assert_eq!(&frags[0][2..10], &7u64.to_be_bytes());
        assert_eq!(&frags[0][10..18], &3u64.to_be_bytes());
        assert_eq!(frags[0][18], 0); // header_tail
        assert_eq!(&frags[0][19..], b"hell");
        // Fragment 1: `'F'` id 2 + chunk.
        assert_eq!(frags[1][1], DIST_FRAG_CONT);
        assert_eq!(&frags[1][10..18], &2u64.to_be_bytes());
        assert_eq!(&frags[1][18..], b"o wo");
        // Fragment 2: `'F'` id 1 + final chunk.
        assert_eq!(&frags[2][10..18], &1u64.to_be_bytes());
        assert_eq!(&frags[2][18..], b"rld");

        let mut asm = FragmentAssembler::new();
        assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
        assert!(asm.is_pending());
        assert_eq!(asm.push(&frags[1]).unwrap(), Assembly::Pending);
        match asm.push(&frags[2]).unwrap() {
            Assembly::Complete(body) => {
                assert_eq!(body[0], 131);
                assert_eq!(body[1], crate::dist_header::DIST_HEADER);
                assert_eq!(&body[2..], b"\x00hello world");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        assert!(!asm.is_pending());
    }

    #[test]
    fn empty_payload_yields_a_single_e_frame() {
        let frags = split(1, &[0], &[], 8).expect("split");
        assert_eq!(frags.len(), 1);
        assert_eq!(frags[0].len(), DIST_FRAGMENT_HEADER_SIZE + 1);
        assert_eq!(frags[0][1], DIST_FRAG_HEADER);
        assert_eq!(&frags[0][10..18], &1u64.to_be_bytes());

        let mut asm = FragmentAssembler::new();
        assert_eq!(
            asm.push(&frags[0]).unwrap(),
            Assembly::Complete(vec![131, crate::dist_header::DIST_HEADER, 0])
        );
    }

    #[test]
    fn assembler_rejects_out_of_order_and_duplicates() {
        let frags = split(5, &[0], &[0u8; 12], 4).expect("split"); // 3 fragments
        let mut asm = FragmentAssembler::new();
        assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
        // id 2 is expected, send id 1 (out of order / the final fragment early).
        assert!(bad_arg(asm.push(&frags[2])));
        // The expected id 2 is still accepted.
        assert_eq!(asm.push(&frags[1]).unwrap(), Assembly::Pending);
        // id 1 is expected now; a duplicate of id 1 *is* the completion, so first
        // check a repeat of id 2 is rejected.
        assert!(bad_arg(asm.push(&frags[1])));
        assert!(matches!(asm.push(&frags[2]).unwrap(), Assembly::Complete(_)));
    }

    #[test]
    fn assembler_rejects_continuation_without_first_and_mismatched_seq() {
        let frags = split(5, &[0], &[0u8; 12], 4).expect("split");
        let mut asm = FragmentAssembler::new();
        assert!(bad_arg(asm.push(&frags[1]))); // continuation with no in-flight seq

        assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
        // A continuation carrying a different sequence id.
        let other = split(6, &[0], &[0u8; 12], 4).expect("split");
        assert!(bad_arg(asm.push(&other[1])));
        // A second `'E'` while a sequence is pending.
        assert!(bad_arg(asm.push(&frags[0])));
    }

    #[test]
    fn assembler_rejects_oversize_with_system_limit() {
        let frags = split(1, &[0], &[0u8; 40], 4).expect("split"); // 10 fragments
        // Cap the buffer below the first chunk so the first frame is rejected.
        let mut asm = FragmentAssembler::with_limit(1);
        assert!(matches!(asm.push(&frags[0]), Err(BeamError::SystemLimit)));
        assert!(!asm.is_pending());
    }

    #[test]
    fn split_rejects_a_zero_fragment_size() {
        assert!(bad_arg(split(1, &[0], b"data", 0)));
    }

    #[test]
    fn non_fragment_packets_are_reported_verbatim() {
        let mut asm = FragmentAssembler::new();
        assert_eq!(asm.push(&[131, 68, 0]).unwrap(), Assembly::NotFragmented);
        assert_eq!(asm.push(&[1, 2, 3]).unwrap(), Assembly::NotFragmented);
        assert_eq!(asm.push(&[]).unwrap(), Assembly::NotFragmented);
        assert_eq!(asm.push(&[131]).unwrap(), Assembly::NotFragmented);
    }
}
