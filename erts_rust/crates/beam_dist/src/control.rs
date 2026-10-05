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
use beam_codec::{AtomCache, Decoder, Encoder};
use beam_common::{BeamError, Result};
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
///
/// The arity is **not** validated here: like the C encoder, this function is
/// caller-driven (the caller owns the `{DOP_*, …}` shape it is building); the
/// arity check belongs to [`decode_control`].
pub fn encode_control(dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    body.push(dop.as_u8());
    for &term in terms {
        let mut encoder = Encoder::new_body(atoms);
        encoder.encode(term)?;
        body.extend_from_slice(&encoder.finish());
    }
    Ok(body)
}

/// Decodes a control-message body.
///
/// Reads the opcode, then decodes successive [`beam_codec::Decoder::new_body`]
/// terms into `heap`, resolving `ATOM_CACHE_REF` through `cache` when supplied.
/// An unknown opcode, or a term count that disagrees with [`Dop::arity`], is
/// `BeamError::BadArg`.
///
/// One opcode is deliberately **variable-arity**: `DOP_ALTACT_SIG_SEND` carries
/// `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To}` or `…, To, Token}` and
/// `dist.c:2625` only requires `tuple_arity >= 4` (the tuple includes the
/// opcode, so `>= 3` terms here) because "the tuple might be extended with more
/// elements in the future". [`Dop::arity`] is therefore its *minimum*, not its
/// exact count, and this decoder accepts any count `>=` it. Every other opcode
/// requires exact equality.
pub fn decode_control(
    body: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    cache: Option<&AtomCache>,
) -> Result<ControlMessage> {
    // An absent opcode byte is a malformed (empty) control message.
    let (opcode, payload) = body.split_first().ok_or(BeamError::BadArg)?;
    let dop = Dop::from_u8(*opcode).ok_or(BeamError::BadArg)?;

    let mut decoder = Decoder::new_body(payload);
    if let Some(cache) = cache {
        decoder = decoder.with_atom_cache(cache);
    }

    // The term arguments are the concatenated body-form images that fill the
    // remainder of the control message; a truncated one surfaces as BadArg from
    // the codec and is propagated unchanged.
    let mut terms = Vec::new();
    while decoder.remaining() > 0 {
        terms.push(decoder.decode(heap, atoms)?);
    }

    let expected = dop.arity();
    let arity_ok = match dop {
        Dop::AltactSigSend => terms.len() >= expected,
        _ => terms.len() == expected,
    };
    if !arity_ok {
        return Err(BeamError::BadArg);
    }

    Ok(ControlMessage { dop, terms })
}
