//! The `DOP_*` control-message codec.
//!
//! A distribution control message is an **ETF tuple** whose first element is the
//! opcode as a small-integer term
//! (`erts/doc/guides/erl_dist_protocol.md` §"Distribution Message":
//! *"`ControlMessage` - A tuple passed using the external format of Erlang"*,
//! whose *"first element indicates the opcode"*). For a message-carrying opcode a
//! **single further term** — the message — follows the tuple on the wire:
//!
//! ```text
//! SMALL_TUPLE_EXT | LARGE_TUPLE_EXT   arity   DOP   argument_1 … argument_n   [Message]
//! └──────────── the control tuple {DOP_*, …} ─────────────┘                   └ send opcodes ┘
//! ```
//!
//! The opcode is a real `SMALL_INTEGER_EXT` term (`dist.c:2206` requires a *small*
//! `tuple[1]`; the encoder always writes `SMALL_INTEGER_EXT`), and every element is
//! a **body-form** external-term image — no `VERSION_MAGIC` byte, because the
//! enclosing distribution header already carries the single `131`
//! (`external.c:772`, `1231`). Term encoding/decoding is **only** `beam_codec` —
//! this module never re-implements the ETF codec.
//!
//! Ground truth: `erts/emulator/beam/dist.c` (`erts_dist_command`: the
//! `is_not_tuple`/`is_not_small(tuple[1])` gate, the per-opcode `tuple_arity`
//! checks and the `/* {DOP_SEND, From, To} -- Message */` trailing term),
//! `erts/emulator/beam/external.c` and `dist.h` (`enum dop`).

use beam_alloc::Heap;
use beam_codec::{
    AtomCache, Decoder, Encoder, LARGE_TUPLE_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT,
};
use beam_common::{BeamError, Result};
use beam_term::prelude::{is_small, make_small, signed_value};
use beam_term::{AtomTable, Eterm};

use crate::flags::Dop;

/// A decoded distribution control message: the opcode plus its term arguments.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ControlMessage {
    /// The control opcode.
    pub dop: Dop,
    /// The term arguments that follow the opcode (and, for a message-carrying
    /// opcode, the trailing message term last).
    pub terms: Vec<Eterm>,
}

/// Whether a control opcode's message carries the message term after the control
/// tuple (`dist.c`: `/* {DOP_SEND, From, To} -- Message */`; `DOP_REG_SEND`,
/// `DOP_ALIAS_SEND` and `DOP_ALTACT_SIG_SEND` likewise).
fn carries_message(dop: Dop) -> bool {
    matches!(
        dop,
        Dop::Send
            | Dop::SendTt
            | Dop::SendSender
            | Dop::SendSenderTt
            | Dop::RegSend
            | Dop::RegSendTt
            | Dop::AliasSend
            | Dop::AliasSendTt
            | Dop::AltactSigSend
    )
}

/// Encodes a control message into its on-wire **tuple form**: the ETF tuple
/// `{DOP_*, …}` (the opcode as its first element) followed, for a message-carrying
/// opcode, by the message term (`dist.c:2205`, `2468`).
///
/// This is [`encode_control_cached`] without an atom cache — every atom is written
/// inline, so the result is byte-identical to that call with `cache = None`.
pub fn encode_control(dop: Dop, terms: &[Eterm], atoms: &AtomTable) -> Result<Vec<u8>> {
    encode_control_cached(dop, terms, atoms, None)
}

/// Encodes a control message into its on-wire **tuple form**, optionally routing
/// every atom through an outbound distribution atom cache.
///
/// `terms` is the *flattened* shape [`ControlMessage`] carries: the [`Dop::arity`]
/// control arguments and, when the opcode carries a message, the single trailing
/// message term. The message is split off only when the count is exactly
/// `arity + 1`; otherwise every element is a control argument, which keeps the
/// variable-arity tolerance of `DOP_ALTACT_SIG_SEND` (`dist.c:2625`:
/// `tuple_arity >= 4`).
///
/// The opcode is a real small-integer term (`external.c:6680-6685`:
/// `ep[0] == SMALL_TUPLE_EXT && ep[1] == arity && ep[2] == SMALL_INTEGER_EXT &&
/// ep[3] == DOP_*`). Everything is body-form — no `VERSION_MAGIC` byte, because the
/// distribution header carries the single `131` (`external.c:772`, `1231`).
///
/// The arity is **not** validated here: like the C encoder, this function is
/// caller-driven (the caller owns the `{DOP_*, …}` shape it is building); the arity
/// check belongs to [`decode_control`].
///
/// With `cache = Some(writer)` the opcode, every control argument and the trailing
/// message are encoded through **one** [`Encoder`] that consults `writer` for every
/// atom in **payload order**. Because ERTS assigns a ref's payload index
/// sequentially as atoms are first encountered (`insert_acache_map`, `external.c`),
/// that order *is* the header's `AtomCacheReferenceIndex` order: the returned bytes
/// agree with the `DistHeader` the caller must emit alongside them
/// (`beam_dist::OutboundAtomCache` is the concrete writer). Atoms the writer does
/// not accept are written inline, exactly as before.
pub fn encode_control_cached(
    dop: Dop,
    terms: &[Eterm],
    atoms: &AtomTable,
    cache: Option<&mut dyn beam_codec::AtomCacheWriter>,
) -> Result<Vec<u8>> {
    let has_message = carries_message(dop) && terms.len() == dop.arity() + 1;
    let (control_terms, message) = if has_message {
        (&terms[..dop.arity()], Some(terms[dop.arity()]))
    } else {
        (terms, None)
    };

    // The control tuple `{DOP_*, …}` — the opcode is its first element.
    let arity = control_terms.len() + 1;
    let mut body = Vec::new();
    if arity > u8::MAX as usize {
        body.push(LARGE_TUPLE_EXT);
        body.extend_from_slice(&(arity as u32).to_be_bytes());
    } else {
        body.push(SMALL_TUPLE_EXT);
        body.push(arity as u8);
    }

    // One encoder over the whole body so the cache is consulted for every atom in
    // payload order (opcode → control arguments → trailing message).
    let mut encoder = Encoder::new_body(atoms);
    if let Some(cache) = cache {
        encoder = encoder.with_atom_cache(cache);
    }
    encoder.encode(make_small(i64::from(dop.as_u8())))?;
    for &term in control_terms {
        encoder.encode(term)?;
    }
    // The `send` opcodes carry the message term after the control tuple.
    if let Some(message) = message {
        encoder.encode(message)?;
    }
    body.extend_from_slice(&encoder.finish());
    Ok(body)
}

/// Decodes a control message from its on-wire tuple form: the ETF tuple
/// `{DOP_*, …}` plus, for a message-carrying opcode, the trailing message term.
///
/// The tuple's arity is authoritative (`dist.c:2205`), so exactly that many
/// elements are decoded — the first is the opcode (which must be a
/// `SMALL_INTEGER_EXT` small integer), the rest are the control arguments. A
/// message-carrying opcode then contributes one more term (the message). The
/// result is the *flattened* [`ControlMessage`] shape: the control arguments
/// followed, for a `send` opcode, by the message term last.
///
/// The arity rule mirrors `erts_dist_command`: the control arguments must number
/// exactly [`Dop::arity`], except the variable-arity `DOP_ALTACT_SIG_SEND`, which
/// accepts any count `>=` it. A malformed message (wrong tuple tag, short,
/// zero arity, non-small opcode, unknown opcode, wrong arity, trailing bytes) is
/// [`BeamError::BadArg`]; the decoder never panics.
pub fn decode_control(
    body: &[u8],
    heap: &mut Heap,
    atoms: &mut AtomTable,
    cache: Option<&AtomCache>,
) -> Result<ControlMessage> {
    // The control tuple header: `SMALL_TUPLE_EXT` + an 8-bit arity, or
    // `LARGE_TUPLE_EXT` + a 32-bit big-endian arity (`external.h`).
    let (arity, offset) = match body.split_first() {
        Some((&SMALL_TUPLE_EXT, rest)) => {
            let arity = usize::from(*rest.first().ok_or(BeamError::BadArg)?);
            (arity, 2usize)
        }
        Some((&LARGE_TUPLE_EXT, rest)) => {
            let bytes = rest.get(..4).ok_or(BeamError::BadArg)?;
            let arity = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            (arity as usize, 5usize)
        }
        _ => return Err(BeamError::BadArg),
    };
    // A control tuple always holds at least the opcode.
    if arity == 0 {
        return Err(BeamError::BadArg);
    }

    let stream = body.get(offset..).ok_or(BeamError::BadArg)?;
    // The first tuple element is the opcode — a `SMALL_INTEGER_EXT` small integer
    // (`dist.c:2206`; the C encoder always writes `SMALL_INTEGER_EXT`).
    if stream.first() != Some(&SMALL_INTEGER_EXT) {
        return Err(BeamError::BadArg);
    }
    let mut decoder = Decoder::new_body(stream);
    if let Some(cache) = cache {
        decoder = decoder.with_atom_cache(cache);
    }
    let opcode = decoder.decode(heap, atoms)?;
    if !is_small(opcode) {
        return Err(BeamError::BadArg);
    }
    let dop = u8::try_from(signed_value(opcode))
        .ok()
        .and_then(Dop::from_u8)
        .ok_or(BeamError::BadArg)?;

    // The remaining tuple elements are the control arguments.
    let mut terms = Vec::with_capacity(arity.saturating_sub(1));
    for _ in 1..arity {
        terms.push(decoder.decode(heap, atoms)?);
    }

    let arity_ok = match dop {
        Dop::AltactSigSend => terms.len() >= dop.arity(),
        _ => terms.len() == dop.arity(),
    };
    if !arity_ok {
        return Err(BeamError::BadArg);
    }

    // A `send` opcode carries the message term after the control tuple. It is
    // decoded only when the wire actually has a trailing term: a control-only form
    // (exactly the control arguments, no message) is accepted too, keeping the
    // flattened [`ControlMessage`] shape this crate exposes.
    if carries_message(dop) && decoder.remaining() > 0 {
        terms.push(decoder.decode(heap, atoms)?);
    }

    // The tuple arity and (for `send`) the message must account for every byte.
    if decoder.remaining() != 0 {
        return Err(BeamError::BadArg);
    }

    Ok(ControlMessage { dop, terms })
}
