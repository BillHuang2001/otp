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

//! The `LitT` chunk: the static-literal table.
//!
//! Layout: `u32 uncompressed_size`, then the literal record table
//! `i32 count` × `{ i32 ext_size, ext_size bytes }` where each payload is Erlang
//! external-term bytes (an ETF-encoded literal).
//!
//! `uncompressed_size == 0` means the record table is stored verbatim (OTP 28 and
//! later, `parse_decompressed_literals`); non-zero means the **rest of the chunk** is
//! a zlib (RFC 1950) stream that inflates to exactly `uncompressed_size` bytes of
//! record table (`beam_file.c:parse_literal_chunk`). Inflating reuses
//! [`beam_codec::compress::decompress`] (which bounds the output at 32 MiB), so no
//! zlib dependency is added here.
//!
//! This module owns the *framing*: it returns the raw ETF payloads in
//! `Operand::Literal(idx)` index order. [`crate::literal::LiteralArea`] decodes them
//! into real terms at load time (reachable through
//! [`Module::literal`](crate::ir::Module::literal)).
use beam_common::Result;

use crate::error::bad_file;

/// Parses a `LitT` chunk payload into its literal payloads (raw ETF bytes).
///
/// The returned vector has one entry per literal, in `Operand::Literal(idx)` index
/// order. A corrupt/truncated zlib stream, a declared `uncompressed_size` that
/// disagrees with the inflated length, or a truncated literal record yields
/// `BeamError::BadArg`.
pub fn parse(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut reader = beam_codec::Reader::new(data);

    // `0` ⇒ the record table follows verbatim; non-zero ⇒ the remaining bytes are a
    // zlib stream inflating to exactly that many bytes (`beam_file.c:1485-1506`).
    let uncompressed_size = reader.u32be()?;
    if uncompressed_size == 0 {
        return parse_records(&mut reader);
    }

    // Inflate the rest of the chunk. `decompress` caps the output at 32 MiB and maps
    // any zlib error to `BadArg`, so a hostile stream can neither over-allocate nor
    // be mis-parsed as literal records.
    let inflated = beam_codec::compress::decompress(reader.rest())?;
    if inflated.len() != uncompressed_size as usize {
        return Err(bad_file("LitT uncompressed size mismatch"));
    }

    let mut inflated_reader = beam_codec::Reader::new(&inflated);
    parse_records(&mut inflated_reader)
}

/// Parses the literal record table `i32 count × { i32 ext_size, ext_size bytes }`.
///
/// Shared by the verbatim and the inflated paths; the reader must be positioned at
/// the start of the record table.
fn parse_records(reader: &mut beam_codec::Reader<'_>) -> Result<Vec<Vec<u8>>> {
    let raw_count = reader.u32be()? as i32;
    if raw_count < 0 {
        return Err(bad_file("negative literal count"));
    }
    let count = raw_count as usize;

    let mut literals = Vec::new();
    for _ in 0..count {
        // Each record is `{ i32 ext_size, ext_size bytes }` of ETF term bytes.
        let ext_size = reader.u32be()? as i32;
        if ext_size < 0 {
            return Err(bad_file("negative literal size"));
        }
        let bytes = reader.bytes(ext_size as usize)?;
        literals.push(bytes.to_vec());
    }

    Ok(literals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_codec::compress::{compress, MAX_UNCOMPRESSED};
    use beam_common::BeamError;

    /// Builds the uncompressed record table `i32 count × { i32 ext_size, payload }`.
    fn record_table(records: &[(u32, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&(records.len() as u32).to_be_bytes());
        for (size, payload) in records {
            data.extend_from_slice(&size.to_be_bytes());
            data.extend_from_slice(payload);
        }
        data
    }

    /// Builds a verbatim `LitT` chunk from `uncompressed_size` and the records.
    fn lit_bytes(uncompressed_size: u32, records: &[(u32, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&uncompressed_size.to_be_bytes());
        data.extend_from_slice(&record_table(records));
        data
    }

    /// Builds a compressed `LitT` chunk: the correct `uncompressed_size` followed by
    /// the zlib stream of the record table.
    fn compressed_lit_bytes(records: &[(u32, &[u8])], level: u8) -> Vec<u8> {
        let table = record_table(records);
        let packed = compress(&table, level).unwrap();
        let mut data = Vec::new();
        data.extend_from_slice(&(table.len() as u32).to_be_bytes());
        data.extend_from_slice(&packed);
        data
    }

    #[test]
    fn parses_literal_records() {
        let data = lit_bytes(0, &[(3, &[1, 2, 3]), (2, &[9, 8])]);
        let literals = parse(&data).unwrap();
        assert_eq!(literals, vec![vec![1, 2, 3], vec![9, 8]]);
    }

    #[test]
    fn zero_literals_is_allowed() {
        assert!(parse(&lit_bytes(0, &[])).unwrap().is_empty());
    }

    #[test]
    fn the_framing_carries_payloads_that_are_not_terms() {
        // The `LitT` framing moves opaque byte records: it neither validates nor decodes
        // the ETF payload (`crate::literal::LiteralArea` does that, and rejects a
        // malformed one there). A record that is not a term is returned unchanged.
        let not_a_term = [0x00, 0xFF, 0x83];
        assert_eq!(
            parse(&lit_bytes(0, &[(3, &not_a_term)])).unwrap(),
            vec![not_a_term.to_vec()]
        );
    }
    #[test]
    fn compressed_chunk_parses_to_the_same_records() {
        let records: &[(u32, &[u8])] = &[
            (3, &[1, 2, 3]),
            (2, &[9, 8]),
            (0, &[]),
            (5, &[0xFF, 0x00, 0x7F, 0x80, 0x01]),
        ];
        let verbatim = parse(&lit_bytes(0, records)).unwrap();
        assert_eq!(verbatim, vec![
            vec![1, 2, 3],
            vec![9, 8],
            vec![],
            vec![0xFF, 0x00, 0x7F, 0x80, 0x01],
        ]);
        // Every compression level must inflate to byte-identical records.
        for level in 0..=9u8 {
            let data = compressed_lit_bytes(records, level);
            assert_eq!(parse(&data).unwrap(), verbatim, "level {level}");
        }
    }

    #[test]
    fn compressed_empty_table_round_trips() {
        let data = compressed_lit_bytes(&[], 6);
        assert!(parse(&data).unwrap().is_empty());
    }

    #[test]
    fn compressed_chunk_with_a_large_repetitive_payload_round_trips() {
        // A payload large enough to span several deflate blocks, so the inflate loop
        // in `decompress` runs more than once.
        let payload = vec![b'A'; 100_000];
        let data = compressed_lit_bytes(&[(payload.len() as u32, &payload)], 6);
        assert_eq!(parse(&data).unwrap(), vec![payload]);
    }

    #[test]
    fn corrupt_compressed_stream_is_rejected() {
        // `uncompressed_size != 0` but the body is not a zlib stream.
        let err = parse(&lit_bytes(42, &[])).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));

        // A truncated valid stream is rejected too (never a panic).
        let mut data = compressed_lit_bytes(&[(3, &[1, 2, 3])], 6);
        data.truncate(data.len() - 4);
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn every_truncation_of_a_compressed_chunk_is_a_typed_error() {
        // A randomized-ish, multi-block body, so the truncation sweep crosses the
        // header, several deflate blocks and the trailing Adler-32 checksum.
        let payload: Vec<u8> = (0..=255u8).cycle().take(20_000).collect();
        let records: &[(u32, &[u8])] = &[
            (3, &[1, 2, 3]),
            (payload.len() as u32, &payload),
            (5, &[0xFF, 0x00, 0x7F, 0x80, 0x01]),
        ];
        let data = compressed_lit_bytes(records, 6);

        for len in 0..data.len() {
            let err = parse(&data[..len])
                .err()
                .unwrap_or_else(|| panic!("truncating to {len} bytes must be rejected"));
            assert!(matches!(err, BeamError::BadArg), "len {len}: {err:?}");
        }
        // The untruncated chunk still parses, so the sweep above is not vacuous.
        assert_eq!(parse(&data).unwrap(), parse(&lit_bytes(0, records)).unwrap());
    }

    #[test]
    fn every_truncation_of_a_verbatim_chunk_is_a_typed_error() {
        // The record table declares its own count, so a prefix that cuts anywhere
        // (including exactly on a record boundary) still leaves a record unread.
        let records: &[(u32, &[u8])] = &[(3, &[1, 2, 3]), (2, &[9, 8]), (0, &[])];
        let data = lit_bytes(0, records);

        for len in 0..data.len() {
            let err = parse(&data[..len])
                .err()
                .unwrap_or_else(|| panic!("truncating to {len} bytes must be rejected"));
            assert!(matches!(err, BeamError::BadArg), "len {len}: {err:?}");
        }
        assert_eq!(parse(&data).unwrap(), parse(&lit_bytes(0, records)).unwrap());
    }
    #[test]
    fn uncompressed_size_disagreeing_with_the_inflated_length_is_rejected() {
        let table = record_table(&[(3, &[1, 2, 3])]);
        let packed = compress(&table, 6).unwrap();

        // Declared one byte too large and one byte too small: both are `BadArg`.
        for declared in [table.len() as u32 + 1, table.len() as u32 - 1] {
            let mut data = declared.to_be_bytes().to_vec();
            data.extend_from_slice(&packed);
            let err = parse(&data).unwrap_err();
            assert!(matches!(err, BeamError::BadArg), "declared {declared}");
        }
    }

    #[test]
    fn declared_size_over_the_decompress_bound_is_rejected() {
        // A zlib stream of a single-byte table but a declared size above the 32 MiB
        // output cap: the mismatch is caught without any unbounded allocation.
        let table = record_table(&[(1, &[7])]);
        let packed = compress(&table, 6).unwrap();
        let mut data = (MAX_UNCOMPRESSED as u32 + 1).to_be_bytes().to_vec();
        data.extend_from_slice(&packed);
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn negative_count_is_rejected() {
        let mut data = 0u32.to_be_bytes().to_vec();
        data.extend_from_slice(&(-1i32).to_be_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn negative_count_inside_a_compressed_table_is_rejected() {
        // The inflated table itself is malformed: this must be caught by the shared
        // record-table parser, not by a separate code path.
        let mut table = Vec::new();
        table.extend_from_slice(&(-1i32).to_be_bytes());
        let packed = compress(&table, 6).unwrap();
        let mut data = (table.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(&packed);
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn negative_literal_size_is_rejected() {
        let mut data = 0u32.to_be_bytes().to_vec();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&(-1i32).to_be_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_literal_is_rejected() {
        // Claims a 4-byte literal but supplies only two bytes.
        let mut data = 0u32.to_be_bytes().to_vec();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&4i32.to_be_bytes());
        data.extend_from_slice(&[1, 2]);
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_literal_inside_a_compressed_table_is_rejected() {
        let mut table = Vec::new();
        table.extend_from_slice(&1u32.to_be_bytes());
        table.extend_from_slice(&4i32.to_be_bytes());
        table.extend_from_slice(&[1, 2]);
        let packed = compress(&table, 6).unwrap();
        let mut data = (table.len() as u32).to_be_bytes().to_vec();
        data.extend_from_slice(&packed);
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn missing_uncompressed_size_is_rejected() {
        let err = parse(&[0, 0]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
