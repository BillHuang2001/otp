//! The instruction-stream decoder: `Code` bytes → [`RawInstr`]s.
//!
//! Each instruction is one opcode byte (`1..=max_opcode`) followed by
//! [`Opcode::arity`] operands. Each operand is one *tag/length* byte plus an
//! optional payload (`beam_file.c:167-240`); the tag is `byte & 0x07` (a file only
//! ever carries tags 0–7):
//!
//! | tag | meaning |
//! |-----|---------|
//! | `0` | `u` untagged integer |
//! | `1` | `i` integer (may be negative) |
//! | `2` | `a` atom index (0 ⇒ NIL) |
//! | `3` | `x` X register |
//! | `4` | `y` Y register |
//! | `5` | `f` label (0 ⇒ [`crate::operand::NO_FAIL`]) |
//! | `6` | `h` character |
//! | `7` | `z` extended sub-tag (see the crate `CONTEXT.md`) |
//!
//! The tag byte also encodes the operand's variable-length integer: `byte & 0x08 ==
//! 0` ⇒ value `byte >> 4`; else `byte & 0x10 == 0` ⇒ a 1-byte big-endian tail
//! (0–0x7FF); else `(byte >> 5) + 2` big-endian magnitude bytes, with
//! `(byte >> 5) == 7` meaning a `u`-tagged sub-varint gives `count = S + 9` bytes.
//!
//! All reads go through [`beam_codec::Reader`] (the shared bounds-checked
//! big-endian cursor); a malformed stream yields `BeamError::BadArg`, never a panic.
use beam_common::Result;

use crate::ir::{CodeVersion, TypeTable};
use crate::opcode::Opcode;
use crate::operand::Operand;

/// One decoded instruction: opcode, operands and its byte offset in the stream.
///
/// The offset lets [`crate::lower`] attach line/backtrace information and lets the
/// engine report a source location for an exception.
#[derive(Clone, Debug)]
pub struct RawInstr {
    /// The instruction's opcode.
    pub opcode: Opcode,
    /// The decoded operands, in the order the opcode declares them.
    pub operands: Vec<Operand>,
    /// The byte offset of the opcode in the `Code` instruction stream.
    pub offset: u32,
}

/// Decodes a `Code` instruction stream into [`RawInstr`]s.
///
/// `version` supplies `max_opcode` (the valid opcode-byte bound). The stream ends
/// at the `int_code_end/0` instruction, not at size exhaustion. A truncated
/// instruction, an unknown opcode tag, an out-of-range register/atom/label index or
/// an unknown `z` sub-tag yields `BeamError::BadArg`.
// TODO(beam_loader-impl): loop reading one opcode byte + `Opcode::arity` operands
// TODO(beam_loader-impl): via `decode_operand`, recording the byte offset, and stop
// TODO(beam_loader-impl): after `int_code_end` (genop 3).
pub fn decode_stream(stream: &[u8], version: &CodeVersion) -> Result<Vec<RawInstr>> {
    let _ = (stream, version);
    Err(beam_common::BeamError::BadArg)
}

/// Decodes one operand from the instruction stream.
///
/// Reads the tag/length byte and its payload from `r`, returning the typed
/// [`Operand`]. `types` is the module's [`TypeTable`]: when it is a fallback the
/// `z,5` `tr` operand collapses to a plain X/Y register (the type index is
/// discarded), and a `z,1` argument list recurses into [`decode_operand`].
///
/// A short read, an out-of-file tag or an unknown/obsolete `z` sub-tag (notably
/// sub-tag 0, the obsolete float form) yields `BeamError::BadArg`.
// TODO(beam_loader-impl): read the tag byte, split `byte & 0x07` into the tag and
// TODO(beam_loader-impl): decode the packed variable-length integer, then dispatch:
// TODO(beam_loader-impl): u/i ⇒ Integer, a ⇒ Atom/Nil, x/y ⇒ X/Y, f ⇒ Label,
// TODO(beam_loader-impl): h ⇒ Character, z ⇒ sub-tags 1 (List), 2 (FloatReg),
// TODO(beam_loader-impl): 3 (Allocation), 4 (Literal), 5 (Typed `tr`).
pub fn decode_operand<'a>(
    r: &mut beam_codec::Reader<'a>,
    types: &TypeTable,
) -> Result<Operand> {
    let _ = (r, types);
    Err(beam_common::BeamError::BadArg)
}
