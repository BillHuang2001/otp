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
//!
//! # The trailing term (`carries_message`)
//!
//! `erts_dist_command` does not read every opcode's payload out of the control
//! tuple: several opcodes carry **one further term after the tuple**. The sender
//! side makes this explicit by setting `ctx->msg` before `erts_dsig_send`
//! (`dist.c:3194-3199`), and it happens for three opcode families:
//!
//! * the **send** family — `DOP_SEND`, `DOP_SEND_TT`, `DOP_SEND_SENDER`,
//!   `DOP_SEND_SENDER_TT`, `DOP_REG_SEND`, `DOP_REG_SEND_TT`, `DOP_ALIAS_SEND`,
//!   `DOP_ALIAS_SEND_TT` (`ctx->msg = message`) and `DOP_ALTACT_SIG_SEND`
//!   (`ctx->msg = reason`, `dist.c:1822-1824`);
//! * the **payload-exit** family used under `DFLAG_EXIT_PAYLOAD` — `DOP_PAYLOAD_EXIT`,
//!   `DOP_PAYLOAD_EXIT_TT` (`dist.c:1744-1745`), `DOP_PAYLOAD_EXIT2`,
//!   `DOP_PAYLOAD_EXIT2_TT` (`dist.c:1795-1798`) and `DOP_PAYLOAD_MONITOR_P_EXIT`
//!   (`dist.c:1511-1514`) — where `ctx->msg = reason` carries the exit reason as a
//!   payload because it is too large to inline;
//! * the **spawn-request** family — `DOP_SPAWN_REQUEST` / `DOP_SPAWN_REQUEST_TT`
//!   (`ctx->msg = alist`, `dist.c:1877`), where the trailing term is the argument
//!   list.
//!
//! `DOP_SPAWN_REPLY` / `DOP_SPAWN_REPLY_TT` deliberately do **not** carry one
//! (`ctx->msg = THE_NON_VALUE`, `dist.c:1897`), and neither do the signal opcodes
//! built through `dsig_send_ctl` (which sets `ctx->msg = THE_NON_VALUE`,
//! `dist.c:3201-3211`).
//!
//! # What is *not* in v6
//!
//! `enum dop` (`dist.h:140-183`) holds exactly the 30 opcodes modelled by
//! [`Dop`]. In particular there is **no `DOP_NODEUP`/`DOP_NODEDOWN`**: node
//! up/down are *local emulator events* delivered by `send_nodes_mon_msgs`
//! (`dist.c:6931`), called from the connection-teardown path (`dist.c:864`) and
//! the `nodeup` path (`dist.c:5063`, `dist.c:5475`) — never a control opcode — and
//! `net_kernel:monitor_nodes/1,2` is served by the local emulator. There is also
//! **no `DOP_DEMONITOR_ID` / `DOP_UNLINK_MONITOR_ID` / `DOP_DEMONITOR_MONITOR_ID`**.
//! `DOP_UNLINK` (`4`) still exists in the enum but is *never sent* (the new link
//! protocol has been mandatory since OTP 26 — `dist.c:2277-2282`), and the byte
//! `5` is the pre-OTP ancient `DOP_NODE_LINK`, explicitly reusable
//! (`dist.h:147`). [`Dop::from_u8`] rejects every byte that is not one of the 30.

use beam_alloc::Heap;
use beam_codec::{
    AtomCache, Decoder, Encoder, LARGE_TUPLE_EXT, SMALL_INTEGER_EXT, SMALL_TUPLE_EXT,
};
use beam_common::{BeamError, Result};
use beam_term::prelude::{is_small, make_small, signed_value};
use beam_term::{AtomTable, Eterm};

use crate::flags::{Dop, DOP_ALTACT_SIG_FLG_TOKEN};

/// A decoded distribution control message: the opcode plus its term arguments.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ControlMessage {
    /// The control opcode.
    pub dop: Dop,
    /// The term arguments that follow the opcode (and, for a message-carrying
    /// opcode, the trailing message term last).
    pub terms: Vec<Eterm>,
}

/// Whether a control opcode carries **one further term after the control tuple**
/// (`dist.c`: `/* {DOP_SEND, From, To} -- Message */`).
///
/// This is the complete v6 set — the send family, the `DFLAG_EXIT_PAYLOAD`
/// payload-exit family and the spawn-request family — derived from the sender
/// paths that set `ctx->msg` before `erts_dsig_send` (see the module docs for the
/// `dist.c` citations). It is **not** limited to the `/* -- Message */` opcodes:
/// a payload exit carries its (payload) *reason* and a spawn request its *argument
/// list* in exactly the same position.
///
/// The [`ControlMessage`] shape is *flattened*: the [`Dop::arity`] control
/// arguments, followed — for a `carries_message` opcode — by that single trailing
/// term last.
pub fn carries_message(dop: Dop) -> bool {
    matches!(
        dop,
        // The send family.
        Dop::Send
            | Dop::SendTt
            | Dop::SendSender
            | Dop::SendSenderTt
            | Dop::RegSend
            | Dop::RegSendTt
            | Dop::AliasSend
            | Dop::AliasSendTt
            | Dop::AltactSigSend
            // The `DFLAG_EXIT_PAYLOAD` payload-exit family.
            | Dop::PayloadExit
            | Dop::PayloadExitTt
            | Dop::PayloadExit2
            | Dop::PayloadExit2Tt
            | Dop::PayloadMonitorPExit
            // The spawn-request family (`ctx->msg = alist`; `dist.c:1877`).
            | Dop::SpawnRequest
            | Dop::SpawnRequestTt
    )
}

/// The number of **control arguments** (tuple elements after the opcode) an
/// opcode's control tuple carries, given the flattened `terms`.
///
/// For every opcode but `DOP_ALTACT_SIG_SEND` this is exactly [`Dop::arity`]. That
/// one is *variable arity*: the tuple is `{DOP_ALTACT_SIG_SEND, Flags, SenderPid,
/// To}` or `{DOP_ALTACT_SIG_SEND, Flags, SenderPid, To, Token}` — the extra token
/// element is present **iff** `Flags` has `ERTS_DOP_ALTACT_SIG_FLG_TOKEN` set
/// (`dist.c:2651-2658`). Reading the flag is what keeps the trailing message on
/// the *right side* of the tuple.
fn control_arg_count(dop: Dop, terms: &[Eterm]) -> usize {
    if dop == Dop::AltactSigSend {
        let token = terms.first().copied().is_some_and(altact_flags_has_token);
        return if token { 4 } else { 3 };
    }
    dop.arity()
}

/// `(Flags & ERTS_DOP_ALTACT_SIG_FLG_TOKEN) != 0` for an `AltactSigSend` `Flags`
/// term (`dist.c:2628-2658`).
///
/// The C accepts a small integer or a bignum and reads the low byte. Only the
/// low five bits are defined, so a small integer is sufficient here: a non-small
/// `Flags` (a bignum) is treated as token-less by the caller, which never
/// mis-places a message the caller did not ask for.
fn altact_flags_has_token(flags: Eterm) -> bool {
    is_small(flags) && (signed_value(flags) & i64::from(DOP_ALTACT_SIG_FLG_TOKEN)) != 0
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
/// control arguments and, when the opcode [`carries_message`], the single trailing
/// term. The trailing term is split off only when the count is exactly one more
/// than the opcode's control-argument count; otherwise every element is a control
/// argument, which keeps the variable-arity tolerance of `DOP_ALTACT_SIG_SEND`
/// (`dist.c:2625`: `tuple_arity >= 4`).
///
/// The control-argument count is [`Dop::arity`] for every opcode but
/// `DOP_ALTACT_SIG_SEND`, whose token element is present iff its `Flags` term has
/// `ERTS_DOP_ALTACT_SIG_FLG_TOKEN` set (`dist.c:2651-2658`) — so a token-carrying
/// altact tuple is `{op, Flags, Sender, To, Token} -- Message`, not a 6-tuple.
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
    let nargs = control_arg_count(dop, terms);
    let has_message = carries_message(dop) && terms.len() == nargs + 1;
    let (control_terms, message) = if has_message {
        (&terms[..nargs], Some(terms[nargs]))
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
/// `{DOP_*, …}` plus, for an opcode that [`carries_message`], the trailing term.
///
/// The tuple's arity is authoritative (`dist.c:2205`), so exactly that many
/// elements are decoded — the first is the opcode (which must be a
/// `SMALL_INTEGER_EXT` small integer), the rest are the control arguments. An
/// opcode that [`carries_message`] then contributes one more term (the trailing
/// message / payload / argument list). The result is the *flattened*
/// [`ControlMessage`] shape: the control arguments followed, for such an opcode,
/// by the trailing term last.
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
