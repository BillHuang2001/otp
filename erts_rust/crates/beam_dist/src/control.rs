//! The `DOP_*` control-message codec.
//!
//! A control message is `dop` byte + the concatenated **body-form** external term
//! format encodings of its term arguments (no `VERSION_MAGIC`; the version byte
//! is carried once by the enclosing distribution header). Term encoding/decoding
//! is **only** `beam_codec` — this module never re-implements the ETF codec.
//!
//! Ground truth: `erts/emulator/beam/dist.c` (`enum dop`, the `{DOP_*, …}` tuple
//! shapes) and `dist.h`.

use beam_alloc::Heap;
use beam_codec::AtomCache;
use beam_common::Result;
use beam_term::{AtomTable, Eterm};

use crate::flags::Dop;

/// A decoded distribution control message: the opcode plus its term arguments.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ControlMessage {
    /// The control opcode.
    pub dop: Dop,
    /// The term arguments that follow the opcode.
    pub terms: Vec<Eterm>,
}

/// Encodes a control-message body (`dop` byte + concatenated body-form terms).
///
/// Each term is a [`beam_codec::Encoder::new_body`] image (no version byte),
/// because a distribution control message is a tuple whose version byte is
/// carried once by the enclosing header.
pub fn encode_control(dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Result<Vec<u8>> {
    todo!("M1")
}

/// Decodes a control-message body.
///
/// Reads the opcode, then decodes successive [`beam_codec::Decoder::new_body`]
/// terms into `heap`, resolving `ATOM_CACHE_REF` through `cache` when supplied.
/// An unknown opcode, or a term count that disagrees with [`Dop::arity`], is
/// `BeamError::BadArg`.
pub fn decode_control(
    body: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    cache: Option<&AtomCache>,
) -> Result<ControlMessage> {
    todo!("M1")
}
