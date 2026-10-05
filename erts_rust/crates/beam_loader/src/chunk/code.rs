//! The `Code` chunk header.
//!
//! Layout: `u32 head_size` (== 16) then `u32 format_number` (must be
//! `BEAM_FORMAT_NUMBER` 0), `u32 max_opcode`, `u32 label_count`, `u32
//! function_count`; the instruction stream follows. The stream ends at the
//! `int_code_end/0` instruction, not at size exhaustion
//! (`beam_file.c:1527,2596`).
use beam_common::Result;

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
/// A `format_number` other than 0, or a header that runs past the chunk, yields
/// `BeamError::BadArg`.
// TODO(beam_loader-impl): read `head_size` and the four `u32` header fields, guard
// TODO(beam_loader-impl): `format_number == 0`, and return the remaining instruction
// TODO(beam_loader-impl): stream bytes.
pub fn parse_header(data: &[u8]) -> Result<(CodeHeader, &[u8])> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}
