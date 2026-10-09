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

//! The `Code` chunk header.
//!
//! Layout: `u32 head_size` (== 16) then `u32 format_number` (must be
//! `BEAM_FORMAT_NUMBER` 0), `u32 max_opcode`, `u32 label_count`, `u32
//! function_count`; the instruction stream follows. The stream ends at the
//! `int_code_end/0` instruction, not at size exhaustion
//! (`beam_file.c:1527,2596`).
//!
//! `max_opcode` is validated as a *version gate* on top of its role as the opcode byte
//! bound: the C rejects a header above the roster's last opcode ("compiled for a later
//! version of the runtime system") and one below `swap/2` (OTP 22 or earlier) right after
//! the file has been read (`beam_load.c:189-225`).
use beam_common::Result;

use crate::error::{bad_file, truncated};
use crate::opcode::{MAX_GENERIC_OPCODE, MIN_SUPPORTED_MAX_OPCODE};

/// The minimum `head_size`: the four `u32` header fields after it.
const MIN_HEAD_SIZE: u32 = 16;

/// The decoded `Code` chunk header.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CodeHeader {
    /// The format number (must be 0).
    pub format_number: u32,
    /// The highest opcode number present.
    pub max_opcode: u32,
    /// The number of labels in the code.
    pub label_count: u32,
    /// The number of functions in the code.
    pub function_count: u32,
}

/// Parses the `Code` chunk header, returning it together with the instruction
/// stream (the bytes after the header).
///
/// A `format_number` other than 0, a `max_opcode` outside the runtime's opcode space
/// (`MAX_GENERIC_OPCODE`, `beam_load.c:189-196`; `MIN_SUPPORTED_MAX_OPCODE`,
/// `beam_load.c:197-225`), or a header that runs past the chunk yields
/// `BeamError::BadArg` — the same class as a bad `format_number`, because an image whose
/// header declares an opcode space this runtime cannot execute is structurally
/// unusable, not merely oversized.
pub fn parse_header(data: &[u8]) -> Result<(CodeHeader, &[u8])> {
    let mut reader = beam_codec::Reader::new(data);

    let head_size = reader.u32be()?;
    if head_size < MIN_HEAD_SIZE {
        return Err(bad_file("Code header too small"));
    }

    let format_number = reader.u32be()?;
    if format_number != 0 {
        return Err(bad_file("unsupported Code format number"));
    }
    let max_opcode = reader.u32be()?;
    // The file's opcode space must lie inside the roster this runtime implements:
    // above it the file needs a newer runtime, below it the file predates OTP 23 and may
    // carry constructs the current loader no longer transforms (`beam_load.c:189-225`).
    if max_opcode > u32::from(MAX_GENERIC_OPCODE) {
        return Err(bad_file("Code max_opcode is above the opcode roster"));
    }
    if max_opcode < u32::from(MIN_SUPPORTED_MAX_OPCODE) {
        return Err(bad_file("Code max_opcode predates OTP 23"));
    }
    let label_count = reader.u32be()?;
    let function_count = reader.u32be()?;
    // The instruction stream starts immediately after the 4-byte size word plus
    // `head_size` header bytes.
    let stream_start = 4usize
        .checked_add(head_size as usize)
        .ok_or_else(|| bad_file("Code head_size overflow"))?;
    if stream_start > data.len() {
        return Err(truncated("Code header"));
    }

    let header = CodeHeader {
        format_number,
        max_opcode,
        label_count,
        function_count,
    };
    Ok((header, &data[stream_start..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// Builds a `Code` chunk header with `head_size` and the four fields.
    fn header_bytes(
        head_size: u32,
        format_number: u32,
        max_opcode: u32,
        label_count: u32,
        function_count: u32,
    ) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&head_size.to_be_bytes());
        data.extend_from_slice(&format_number.to_be_bytes());
        data.extend_from_slice(&max_opcode.to_be_bytes());
        data.extend_from_slice(&label_count.to_be_bytes());
        data.extend_from_slice(&function_count.to_be_bytes());
        data
    }

    #[test]
    fn parses_a_valid_header() {
        let mut data = header_bytes(16, 0, 181, 15, 6);
        data.push(0xAA); // one byte of instruction stream
        let (header, stream) = parse_header(&data).unwrap();
        assert_eq!(header.format_number, 0);
        assert_eq!(header.max_opcode, 181);
        assert_eq!(header.label_count, 15);
        assert_eq!(header.function_count, 6);
        assert_eq!(stream, &[0xAA]);
    }

    #[test]
    fn larger_head_size_skips_extra_bytes() {
        // head_size 20 ⇒ 4 extra header bytes before the stream.
        let mut data = header_bytes(20, 0, 181, 1, 1);
        data.extend_from_slice(&[0, 0, 0, 0]);
        data.push(0x5A);
        let (_, stream) = parse_header(&data).unwrap();
        assert_eq!(stream, &[0x5A]);
    }

    #[test]
    fn empty_stream_is_allowed() {
        let data = header_bytes(16, 0, 181, 0, 0);
        let (_, stream) = parse_header(&data).unwrap();
        assert!(stream.is_empty());
    }
    #[test]
    fn nonzero_format_number_is_rejected() {
        let data = header_bytes(16, 1, 181, 15, 6);
        let err = parse_header(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn head_size_below_minimum_is_rejected() {
        let data = header_bytes(15, 0, 181, 15, 6);
        let err = parse_header(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn head_size_running_past_the_chunk_is_rejected() {
        // head_size claims 100 bytes but the chunk holds only the 20-byte header.
        let data = header_bytes(100, 0, 181, 15, 6);
        let err = parse_header(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_header_is_rejected() {
        let data = header_bytes(16, 0, 181, 15, 6);
        let err = parse_header(&data[..10]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    /// Asserts `parse_header` rejects the header built from `header_bytes` fields.
    #[track_caller]
    fn assert_header_rejected(
        head_size: u32,
        format_number: u32,
        max_opcode: u32,
        label_count: u32,
        function_count: u32,
        what: &str,
    ) {
        let data = header_bytes(head_size, format_number, max_opcode, label_count, function_count);
        match parse_header(&data) {
            Err(BeamError::BadArg) => {}
            Err(other) => panic!("{what}: expected BadArg, got {other:?}"),
            Ok(_) => panic!("{what}: expected BadArg, got a successful parse"),
        }
    }

    #[test]
    fn a_max_opcode_above_the_roster_is_rejected() {
        // `beam_load.c:189-196`: a header above `MAX_GENERIC_OPCODE` means the file was
        // built for a later runtime than this one.
        for max_opcode in [194u32, 200, 255, u32::MAX] {
            assert_header_rejected(16, 0, max_opcode, 15, 6, "max_opcode above the roster");
        }
        // Control: the roster's own last opcode loads.
        let mut control = header_bytes(16, 0, 193, 15, 6);
        control.push(0xAA);
        assert_eq!(parse_header(&control).unwrap().0.max_opcode, 193);
    }

    #[test]
    fn a_max_opcode_below_swap_is_rejected() {
        // `beam_load.c:197-225`: OTP 23-26 set `max_opcode` to exactly `swap/2`, so a
        // smaller value is an OTP 22 (or earlier) file the current loader refuses.
        for max_opcode in [0u32, 1, 168] {
            assert_header_rejected(16, 0, max_opcode, 15, 6, "max_opcode below swap/2");
        }
        // Control: `swap/2` itself (169) is the lowest accepted value.
        let mut control = header_bytes(16, 0, 169, 15, 6);
        control.push(0xAA);
        assert_eq!(parse_header(&control).unwrap().0.max_opcode, 169);
    }

    #[test]
    fn the_max_opcode_boundaries_are_inclusive() {
        // Both ends of the accepted range load: `swap/2` itself (169) and the roster's
        // last opcode (193).
        for max_opcode in [169u32, 181, 193] {
            let mut data = header_bytes(16, 0, max_opcode, 2, 1);
            data.push(0xAA);
            let (header, stream) = parse_header(&data)
                .unwrap_or_else(|error| panic!("max_opcode {max_opcode}: {error:?}"));
            assert_eq!(header.max_opcode, max_opcode);
            assert_eq!(stream, &[0xAA]);
        }
    }
}
