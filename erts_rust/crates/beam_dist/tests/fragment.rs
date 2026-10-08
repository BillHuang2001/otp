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

//! `'E'`/`'F'` fragment prefixes, sender splitting and receiver reassembly.
//!
//! Byte-exact against the worked example in
//! `erts/doc/guides/erl_ext_dist.md` §"Distribution Header for fragmented
//! messages" and the ground truth in `erts/emulator/beam/external.c`
//! (`erts_encode_ext_dist_header_setup`/`_fragment`/`_finalize`) +
//! `erts/emulator/beam/dist.c` (`ERTS_DSIG_SEND_PHASE_FIN`, `ERTS_PREP_DIST_EXT_FRAG_CONT`).
//!
//! Every test is hermetic: no sockets, all codecs are driven in memory.
use beam_codec::AtomEncoding;
use beam_common::{BeamError, Result};
use beam_dist::dist_header::{
    decode_fragmented_header, AtomCacheRef, DistHeader, DIST_FRAG_CONT, DIST_FRAG_HEADER,
    DIST_HEADER,
};
use beam_dist::fragment::{
    split, Assembly, FragmentAssembler, FragmentCont, FragmentHeader, DIST_FRAGMENT_HEADER_SIZE,
    DIST_FRAGMENT_SIZE,
};
use beam_dist::transport::Write;
use beam_term::AtomTable;

/// A trivial `Vec<u8>`-backed [`Write`] sink so the prefix codecs can be driven
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

fn encode_prefix<F>(f: F) -> Vec<u8>
where
    F: FnOnce(&mut VecWriter) -> Result<()>,
{
    let mut sink = VecWriter::default();
    f(&mut sink).expect("encoding into a Vec cannot fail");
    sink.0
}

fn is_bad_arg<T: std::fmt::Debug>(res: Result<T>) -> bool {
    matches!(res, Err(BeamError::BadArg))
}

/// The atom handle behind a `NewAtomCacheRef`, or `None` for a cached ref.
fn new_atom(r: &AtomCacheRef) -> Option<beam_term::Atom> {
    match r {
        AtomCacheRef::New { atom, .. } => Some(*atom),
        AtomCacheRef::Cached { .. } => None,
    }
}

/// The sequence id from the `erl_ext_dist.md` worked example:
/// `131,69,0,0,2,168,0,0,5,83,…` -> `0x000002A800000553`.
const DOC_SEQ_ID: u64 = 0x0000_02A8_0000_0553;
const DOC_SEQ_BYTES: [u8; 8] = [0, 0, 2, 168, 0, 0, 5, 83];

/// Builds a real distribution-header *tail* (`NumberOfAtomCacheRefs + Flags +
/// AtomCacheRefs`) with one `NewAtomCacheRef` per name, i.e. the bytes a `'D'`
/// header carries after `VERSION_MAGIC + 'D'`.
fn header_tail(names: &[&str]) -> (AtomTable, Vec<u8>) {
    let mut atoms = AtomTable::new();
    let refs = names
        .iter()
        .enumerate()
        .map(|(i, name)| AtomCacheRef::New {
            segment: (i % 8) as u8,
            index: (i + 1) as u8,
            atom: atoms.intern(name),
        })
        .collect();
    let header = DistHeader { refs, long_atoms: false };
    let mut sink = VecWriter::default();
    header.encode(&mut sink, &atoms, AtomEncoding::Utf8).expect("encode header");
    // Strip the `VERSION_MAGIC + 'D'` prefix, leaving the shared tail.
    (atoms, sink.0[2..].to_vec())
}

#[test]
fn fragment_header_prefix_is_byte_exact() {
    let bytes = encode_prefix(|w| {
        FragmentHeader { seq_id: DOC_SEQ_ID, fragment_count: 2 }.encode(w)
    });
    assert_eq!(bytes, vec![131, 69, 0, 0, 2, 168, 0, 0, 5, 83, 0, 0, 0, 0, 0, 0, 0, 2]);
    // Field offsets: VERSION_MAGIC(0) tag(1) seq(2..10) count(10..18).
    assert_eq!(bytes[0], 131);
    assert_eq!(bytes[1], DIST_FRAG_HEADER);
    assert_eq!(DIST_FRAG_HEADER, 69);
    assert_eq!(&bytes[2..10], &DOC_SEQ_BYTES);
    assert_eq!(&bytes[10..18], &2u64.to_be_bytes());
    assert_eq!(bytes.len(), DIST_FRAGMENT_HEADER_SIZE);
}

#[test]
fn fragment_cont_prefix_is_byte_exact() {
    let bytes = encode_prefix(|w| {
        FragmentCont { seq_id: DOC_SEQ_ID, fragment_index: 1 }.encode(w)
    });
    assert_eq!(bytes, vec![131, 70, 0, 0, 2, 168, 0, 0, 5, 83, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(bytes[0], 131);
    assert_eq!(bytes[1], DIST_FRAG_CONT);
    assert_eq!(DIST_FRAG_CONT, 70);
    assert_eq!(&bytes[2..10], &DOC_SEQ_BYTES);
    assert_eq!(&bytes[10..18], &1u64.to_be_bytes());
}

#[test]
fn prefixes_round_trip_and_reject_bad_input() {
    let header = FragmentHeader { seq_id: 0xDEAD_BEEF_0000_0001, fragment_count: 9 };
    let bytes = encode_prefix(|w| header.encode(w));
    assert_eq!(FragmentHeader::decode(&bytes).unwrap(), header);

    let cont = FragmentCont { seq_id: 0xDEAD_BEEF_0000_0001, fragment_index: 8 };
    let bytes = encode_prefix(|w| cont.encode(w));
    assert_eq!(FragmentCont::decode(&bytes).unwrap(), cont);

    // Wrong tag.
    let e = encode_prefix(|w| header.encode(w));
    let f = encode_prefix(|w| cont.encode(w));
    assert!(is_bad_arg(FragmentHeader::decode(&f)));
    assert!(is_bad_arg(FragmentCont::decode(&e)));
    // Truncated (needs all 18 prefix bytes).
    for n in 0..DIST_FRAGMENT_HEADER_SIZE {
        let mut cut = e.clone();
        cut.truncate(n);
        assert!(is_bad_arg(FragmentHeader::decode(&cut)), "length {n}");
    }
    // Wrong VERSION_MAGIC.
    let mut wrong = e.clone();
    wrong[0] = 130;
    assert!(is_bad_arg(FragmentHeader::decode(&wrong)));
}

#[test]
fn split_places_the_tail_and_chunks_at_the_documented_offsets() {
    let (_atoms, tail) = header_tail(&["a"]);
    let tail_len = tail.len();
    let payload = b"hello world";
    let frags = split(DOC_SEQ_ID, &tail, payload, 4).expect("split");

    // 11 bytes / 4 => 3 fragments (4, 4, 3).
    assert_eq!(frags.len(), 3);
    assert_eq!(&frags[0][2..10], &DOC_SEQ_BYTES);
    assert_eq!(&frags[0][10..18], &3u64.to_be_bytes());
    // The `'E'` header tail lands verbatim, then the first chunk.
    assert_eq!(&frags[0][DIST_FRAGMENT_HEADER_SIZE..DIST_FRAGMENT_HEADER_SIZE + tail_len], &tail[..]);
    assert_eq!(&frags[0][DIST_FRAGMENT_HEADER_SIZE + tail_len..], b"hell");
    // Continuations carry a decrementing fragment id and no tail.
    assert_eq!(frags[1][1], DIST_FRAG_CONT);
    assert_eq!(&frags[1][10..18], &2u64.to_be_bytes());
    assert_eq!(&frags[1][DIST_FRAGMENT_HEADER_SIZE..], b"o wo");
    assert_eq!(&frags[2][10..18], &1u64.to_be_bytes());
    assert_eq!(&frags[2][DIST_FRAGMENT_HEADER_SIZE..], b"rld");
}

#[test]
fn round_trip_a_payload_larger_than_the_default_fragment_size() {
    let (atoms, tail) = header_tail(&["reg", "call"]);
    let payload = vec![0xABu8; DIST_FRAGMENT_SIZE + 5];
    let seq_id = 0x0000_0102_0304_0506;

    let frags = split(seq_id, &tail, &payload, DIST_FRAGMENT_SIZE).expect("split");
    assert_eq!(frags.len(), 2);
    assert_eq!(&frags[0][10..18], &2u64.to_be_bytes());
    assert_eq!(&frags[1][10..18], &1u64.to_be_bytes());
    // The first fragment carries the `'E'` prefix + tail + a full chunk.
    assert_eq!(frags[0].len(), DIST_FRAGMENT_HEADER_SIZE + tail.len() + DIST_FRAGMENT_SIZE);
    // The continuation carries the `'F'` prefix + the remaining 5 bytes.
    assert_eq!(frags[1].len(), DIST_FRAGMENT_HEADER_SIZE + 5);

    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
    let body = match asm.push(&frags[1]).unwrap() {
        Assembly::Complete(body) => body,
        other => panic!("expected Complete, got {other:?}"),
    };
    // `VERSION_MAGIC + 'D' + tail + payload`.
    assert_eq!(body[0], 131);
    assert_eq!(body[1], DIST_HEADER);
    assert_eq!(&body[2..2 + tail.len()], &tail[..]);
    assert_eq!(&body[2 + tail.len()..], &payload[..]);
    // The reassembled body is a valid non-fragmented distribution message.
    let mut decode_atoms = AtomTable::new();
    let decoded = DistHeader::decode(&body, &mut decode_atoms, AtomEncoding::Utf8)
        .expect("decode reassembled header");
    assert_eq!(decoded.refs.len(), 2);
    assert_eq!(decode_atoms.name(new_atom(&decoded.refs[0]).unwrap()), Some("reg"));
    assert_eq!(decode_atoms.name(new_atom(&decoded.refs[1]).unwrap()), Some("call"));
    assert_eq!(atoms.len(), decode_atoms.len());
}

#[test]
fn round_trip_a_payload_that_is_an_exact_multiple_of_the_fragment_size() {
    let (_atoms, tail) = header_tail(&["x"]);
    let payload = vec![7u8; DIST_FRAGMENT_SIZE * 3];
    let frags = split(1, &tail, &payload, DIST_FRAGMENT_SIZE).expect("split");
    // Exactly 3 chunks: no extra empty trailing fragment.
    assert_eq!(frags.len(), 3);
    assert_eq!(&frags[0][10..18], &3u64.to_be_bytes());
    assert_eq!(&frags[1][10..18], &2u64.to_be_bytes());
    assert_eq!(&frags[2][10..18], &1u64.to_be_bytes());
    for f in &frags[1..] {
        assert_eq!(f.len(), DIST_FRAGMENT_HEADER_SIZE + DIST_FRAGMENT_SIZE);
    }

    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
    assert_eq!(asm.push(&frags[1]).unwrap(), Assembly::Pending);
    let body = match asm.push(&frags[2]).unwrap() {
        Assembly::Complete(body) => body,
        other => panic!("expected Complete, got {other:?}"),
    };
    assert_eq!(&body[2 + tail.len()..], &payload[..]);
}

#[test]
fn round_trip_a_payload_just_over_the_fragment_size_boundary() {
    let (_atoms, tail) = header_tail(&["x"]);
    let payload = vec![1u8; DIST_FRAGMENT_SIZE + 1];
    let frags = split(3, &tail, &payload, DIST_FRAGMENT_SIZE).expect("split");
    assert_eq!(frags.len(), 2);
    assert_eq!(frags[0].len(), DIST_FRAGMENT_HEADER_SIZE + tail.len() + DIST_FRAGMENT_SIZE);
    assert_eq!(frags[1].len(), DIST_FRAGMENT_HEADER_SIZE + 1);

    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
    match asm.push(&frags[1]).unwrap() {
        Assembly::Complete(body) => assert_eq!(&body[2 + tail.len()..], &payload[..]),
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn a_single_fragment_sequence_completes_immediately() {
    let (_atoms, tail) = header_tail(&["x"]);
    let payload = b"tiny";
    let frags = split(11, &tail, payload, DIST_FRAGMENT_SIZE).expect("split");
    assert_eq!(frags.len(), 1);
    assert_eq!(&frags[0][10..18], &1u64.to_be_bytes());

    let mut asm = FragmentAssembler::new();
    let body = match asm.push(&frags[0]).unwrap() {
        Assembly::Complete(body) => body,
        other => panic!("expected Complete, got {other:?}"),
    };
    assert_eq!(&body[2 + tail.len()..], payload);
    assert!(!asm.is_pending());
}

#[test]
fn out_of_order_and_duplicate_fragments_are_rejected() {
    let (_atoms, tail) = header_tail(&["x"]);
    let frags = split(1, &tail, &[0u8; 12], 4).expect("split"); // ids 3, 2, 1
    assert_eq!(frags.len(), 3);

    // Sending the final fragment (id 1) before id 2.
    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
    assert!(is_bad_arg(asm.push(&frags[2])));
    // A duplicate of the first fragment while a sequence is in flight.
    assert!(is_bad_arg(asm.push(&frags[0])));
    // The correct next fragment is still accepted; a repeat is a duplicate.
    assert_eq!(asm.push(&frags[1]).unwrap(), Assembly::Pending);
    assert!(is_bad_arg(asm.push(&frags[1])));
    assert!(matches!(asm.push(&frags[2]).unwrap(), Assembly::Complete(_)));
}

#[test]
fn fragment_count_mismatch_is_rejected() {
    let (_atoms, tail) = header_tail(&["x"]);
    // `'E'` claiming 2 fragments (8 bytes / 4), then a continuation numbered 2:
    // the receiver expects id 1 (count - 1 == 1).
    let first = split(4, &tail, b"abcdefgh", 4).expect("split")[0].clone();
    assert_eq!(&first[10..18], &2u64.to_be_bytes());
    let bad_cont = encode_prefix(|w| FragmentCont { seq_id: 4, fragment_index: 2 }.encode(w));

    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&first).unwrap(), Assembly::Pending);
    assert!(is_bad_arg(asm.push(&bad_cont)));
}

#[test]
fn truncated_fragment_frames_are_bad_arg() {
    let (_atoms, tail) = header_tail(&["x"]);
    let frags = split(2, &tail, &[0u8; 8], 4).expect("split");

    let mut asm = FragmentAssembler::new();
    // An `'E'` prefix with no header tail byte.
    assert!(is_bad_arg(asm.push(&[131, 69, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2])));
    // A continuation shorter than the 18-byte prefix.
    let mut short = frags[1].clone();
    short.truncate(DIST_FRAGMENT_HEADER_SIZE - 1);
    assert!(is_bad_arg(asm.push(&short)));
    // A good first fragment is still buffered afterwards.
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
}

#[test]
fn oversize_buffered_sequence_is_system_limit() {
    let (_atoms, tail) = header_tail(&["x"]);
    let frags = split(1, &tail, &[0u8; 40], 4).expect("split"); // ids 10, 9, …

    // Cap below the first chunk: the very first frame overflows the buffer.
    let mut asm = FragmentAssembler::with_limit(1);
    assert!(matches!(asm.push(&frags[0]), Err(BeamError::SystemLimit)));
    assert!(!asm.is_pending());

    // Cap above the first chunk but below the whole message: rejected mid-way.
    // The bound counts `header tail + payload`, not the 18-byte prefix.
    let mut asm = FragmentAssembler::with_limit(tail.len() + 8);
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending); //  4 + 4 = 8
    assert_eq!(asm.push(&frags[1]).unwrap(), Assembly::Pending); //  8 + 4 = 12
    assert!(matches!(asm.push(&frags[2]), Err(BeamError::SystemLimit))); // 16 > 12
}

#[test]
fn continuation_without_a_first_fragment_is_bad_arg() {
    let (_atoms, tail) = header_tail(&["x"]);
    let frags = split(1, &tail, &[0u8; 8], 4).expect("split");
    let mut asm = FragmentAssembler::new();
    assert!(is_bad_arg(asm.push(&frags[1])));
}

#[test]
fn mismatched_sequence_id_is_bad_arg() {
    let (_atoms, tail) = header_tail(&["x"]);
    let a = split(1, &tail, &[0u8; 8], 4).expect("split");
    let b = split(2, &tail, &[0u8; 8], 4).expect("split");

    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&a[0]).unwrap(), Assembly::Pending);
    // A continuation from a different sequence.
    assert!(is_bad_arg(asm.push(&b[1])));
    // A whole other sequence's first fragment while one is in flight.
    assert!(is_bad_arg(asm.push(&b[0])));
}

#[test]
fn non_fragmented_packets_are_left_to_the_caller() {
    let (_atoms, tail) = header_tail(&["x"]);
    // A normal `'D'` header.
    let mut d = vec![131, DIST_HEADER];
    d.extend_from_slice(&tail);
    d.extend_from_slice(b"payload");
    // A pass-through message (no distribution header).
    let pass_through = [1u8, 2, 3, 4];

    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&d).unwrap(), Assembly::NotFragmented);
    assert_eq!(asm.push(&pass_through).unwrap(), Assembly::NotFragmented);
    assert_eq!(asm.push(&[]).unwrap(), Assembly::NotFragmented);
    assert_eq!(asm.push(&[131]).unwrap(), Assembly::NotFragmented);
    assert!(!asm.is_pending());

    // A `'D'` packet may interleave with a pending fragmented sequence and the
    // sequence still completes.
    let frags = split(1, &tail, &[0u8; 8], 4).expect("split");
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
    assert_eq!(asm.push(&d).unwrap(), Assembly::NotFragmented);
    assert!(matches!(asm.push(&frags[1]).unwrap(), Assembly::Complete(_)));
}

#[test]
fn reset_drops_the_pending_sequence() {
    let (_atoms, tail) = header_tail(&["x"]);
    let frags = split(1, &tail, &[0u8; 8], 4).expect("split");
    let mut asm = FragmentAssembler::new();
    assert_eq!(asm.push(&frags[0]).unwrap(), Assembly::Pending);
    assert!(asm.is_pending());
    asm.reset();
    assert!(!asm.is_pending());
    // A follow-up continuation is now unexpected.
    assert!(is_bad_arg(asm.push(&frags[1])));
}

#[test]
fn decode_fragmented_header_reads_the_first_frame() {
    let (_atoms, tail) = header_tail(&["reg"]);
    let frags = split(DOC_SEQ_ID, &tail, b"payload!", 4).expect("split");
    let mut atoms = AtomTable::new();
    let frame = decode_fragmented_header(&frags[0], &mut atoms, AtomEncoding::Utf8)
        .expect("valid `'E'` frame");
    assert_eq!(frame.fragment.seq_id, DOC_SEQ_ID);
    assert_eq!(frame.fragment.fragment_count, 2);
    assert_eq!(frame.header.refs.len(), 1);
    assert_eq!(atoms.name(new_atom(&frame.header.refs[0]).unwrap()), Some("reg"));
    // The payload begins right after the shared header tail.
    assert_eq!(frame.payload_offset, DIST_FRAGMENT_HEADER_SIZE + tail.len());
    assert_eq!(&frags[0][frame.payload_offset..], b"payl");
}

#[test]
fn latin1_encoding_is_honoured_when_decoding_the_tail() {
    // An `'E'` frame whose atom text is a lone 0xFF byte: invalid UTF-8, valid
    // Latin-1. The reassembler must accept it under the Latin-1 negotiation.
    let mut atoms = AtomTable::new();
    let atom = atoms.intern("\u{00FF}");
    let header = DistHeader {
        refs: vec![AtomCacheRef::New { segment: 0, index: 1, atom }],
        long_atoms: false,
    };
    let mut sink = VecWriter::default();
    header.encode(&mut sink, &atoms, AtomEncoding::Latin1).expect("encode latin-1 header");
    let tail = sink.0[2..].to_vec();
    let frags = split(1, &tail, b"chunk", DIST_FRAGMENT_SIZE).expect("split");

    // Under Latin-1 the frame decodes; under UTF-8 the same bytes are rejected.
    let mut utf8 = FragmentAssembler::with_encoding(1 << 20, AtomEncoding::Utf8);
    assert!(is_bad_arg(utf8.push(&frags[0])));
    let mut latin1 = FragmentAssembler::with_encoding(1 << 20, AtomEncoding::Latin1);
    assert!(matches!(latin1.push(&frags[0]).unwrap(), Assembly::Complete(_)));
}

#[test]
fn split_rejects_a_zero_fragment_size() {
    let (_atoms, tail) = header_tail(&["x"]);
    assert!(is_bad_arg(split(1, &tail, b"data", 0)));
}

#[test]
fn an_empty_payload_splits_into_a_single_e_frame() {
    let (_atoms, tail) = header_tail(&["x"]);
    let frags = split(5, &tail, &[], DIST_FRAGMENT_SIZE).expect("split");
    assert_eq!(frags.len(), 1);
    assert_eq!(&frags[0][10..18], &1u64.to_be_bytes());
    assert_eq!(frags[0].len(), DIST_FRAGMENT_HEADER_SIZE + tail.len());

    let mut asm = FragmentAssembler::new();
    match asm.push(&frags[0]).unwrap() {
        Assembly::Complete(body) => {
            assert_eq!(body[0], 131);
            assert_eq!(body[1], DIST_HEADER);
            assert_eq!(&body[2..], &tail[..]);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}
