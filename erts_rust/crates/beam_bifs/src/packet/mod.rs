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

//! `erlang:decode_packet/3` — the argument validation, the per-type dispatch and
//! the result rendering.
//!
//! Ground truth (read-only):
//! `../../erts/emulator/beam/erl_bif_port.c:1476-1617` (`decode_packet_3`: the
//! type switch at 1492-1511, the option loop at 1518-1548, the
//! `{ok,…}`/`{more,…}`/`{error,invalid}` dispatch at 1558-1607) and
//! `../../erts/emulator/beam/packet_parser.h:34-50` (`enum PacketParseType`).
//!
//! # Dispatch
//!
//! `Type` resolves to a [`PacketType`] ([`resolve_type`], the C's type switch).
//! After the argument validation below, the byte view of `Bin` reaches exactly
//! **one** family parser — [`tls`] for `ssl_tls`, [`http`] for the four http
//! types, [`framing`] for every other valid type — and each returns an
//! [`Outcome`], which this module renders:
//!
//! * [`Outcome::Invalid`] => `{error, invalid}`;
//! * [`Outcome::More`] => `{more, undefined}` (`None`) or `{more, N}`
//!   (`Some(N)`, the total size required);
//! * [`Outcome::Ok`] => `{ok, Packet, Rest}`, but only when the packet really is
//!   in `Bin`: a `packet_sz` of `0` or one beyond `Bin`'s length is the *same*
//!   `{more, …}` rule (`0` => `undefined`, else `{more, packet_sz}`), never a
//!   partial packet (`erl_bif_port.c:1564-1569`). Otherwise `Rest` is
//!   `bytes[packet_sz..]` and `Packet` is either the body slice
//!   `bytes[start..end]` ([`Body::Binary`], the length-prefixed families) or a
//!   term the family parser already built ([`Body::Term`], the http family and
//!   `ssl_tls`).
//!
//! **The per-type implemented subset is documented in each submodule.** A valid
//! packet type whose rules are not implemented yet returns the typed
//! `unsupported_bif` capability gap — never a fabricated packet — and its
//! arguments are still validated first, so `decode_packet(asn1, 42, [])` stays
//! `badarg` exactly as the oracle answers it.
//!
//! # Argument validation (oracle-faithful)
//!
//! `Bin` must be a byte-aligned bitstring and `Opts` a (possibly empty) proper
//! list of `{packet_size, Uint}` / `{line_length, Uint}` / `{line_delimiter,
//! Uint}` 2-tuples; anything else — a non-binary `Bin`, a non-list `Opts`, an
//! unknown key, a malformed tuple, a negative or `> UINT_MAX` value, a
//! `line_delimiter` outside `line` or above 255 — is the oracle's `badarg`
//! (`erl_bif_port.c:1513-1548`). A **type that is not a packet type at all**
//! (e.g. `bogus`, `3`, `cd`, `ssl`) is likewise `badarg` — pinned by probing the
//! oracle, which reports `badarg` with `cause => badopt` rather than a
//! `{error, {unknown_packet_type, _}}` tuple.

mod framing;
mod http;
mod tls;

use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

/// The default line delimiter — `decode_packet_3`'s `char delimiter = '\n'`
/// (`erl_bif_port.c:1490`).
const DEFAULT_DELIMITER: u8 = b'\n';

/// The packet type selected by `Type` (`enum PacketParseType`,
/// `packet_parser.h:34-50`).
///
/// Every valid type has its own variant so that an unimplemented one is the
/// typed capability gap rather than a `badarg`; only a term that names no packet
/// type at all is `badarg` (see [`resolve_type`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PacketType {
    /// `raw` | `0` (`TCP_PB_RAW`).
    Raw,
    /// `1` (`TCP_PB_1`).
    Len1,
    /// `2` (`TCP_PB_2`).
    Len2,
    /// `4` (`TCP_PB_4`).
    Len4,
    /// `line` (`TCP_PB_LINE_LF`).
    Line,
    /// `asn1` (`TCP_PB_ASN1`).
    Asn1,
    /// `sunrm` (`TCP_PB_RM`).
    Sunrm,
    /// `cdr` (`TCP_PB_CDR`).
    Cdr,
    /// `fcgi` (`TCP_PB_FCGI`).
    Fcgi,
    /// `tpkt` (`TCP_PB_TPKT`).
    Tpkt,
    /// `ssl_tls` (`TCP_PB_SSL_TLS`).
    SslTls,
    /// `http` (`TCP_PB_HTTP`).
    Http,
    /// `httph` (`TCP_PB_HTTPH`).
    HttpH,
    /// `http_bin` (`TCP_PB_HTTP_BIN`).
    HttpBin,
    /// `httph_bin` (`TCP_PB_HTTPH_BIN`).
    HttpHBin,
}

/// The `packet_size`/`line_length`/`line_delimiter` options
/// (`erl_bif_port.c:1479-1548`); `0` means "no limit" in the C.
#[derive(Clone, Copy, Debug)]
pub(super) struct Options {
    /// `packet_size` — the C's `max_plen`.
    pub max_plen: u64,
    /// `line_length` — the C's `trunc_len`.
    pub trunc_len: u64,
    /// `line_delimiter` — the C's `delimiter` (only the `line` type; `\n` by
    /// default).
    pub delimiter: u8,
}

/// The body of a complete packet, as reported by a family parser.
pub(super) enum Body {
    /// A plain binary body: `bytes[start..end]`. The producing parser guarantees
    /// `start <= end <= packet_sz <= bytes.len()`.
    Binary { start: usize, end: usize },
    /// A term already built by the family parser (http family, `ssl_tls`).
    // The frozen seam a term-building family hands its packet back through. The
    // families that build a term are typed gaps today, so nothing constructs it
    // yet; the `allow` goes away with the first of them.
    #[allow(dead_code)]
    Term(Eterm),
}

/// The outcome of a family parser ([`framing::parse`], [`tls::parse`],
/// [`http::parse`]).
pub(super) enum Outcome {
    /// The packet is malformed — the C's negative `packet_get_length` =>
    /// `{error, invalid}`.
    Invalid,
    /// More bytes are needed: `Some(n)` is the total size required (`{more, n}`),
    /// `None` the C's `0` length (`{more, undefined}`).
    More(Option<usize>),
    /// A whole packet of `packet_sz` bytes plus its [`Body`].
    Ok { packet_sz: usize, body: Body },
}

/// `erlang:decode_packet/3`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any argument the
/// oracle rejects, the typed `unsupported_bif` raise for a valid packet type
/// that is not implemented, and [`BeamError::Alloc`](beam_common::BeamError::Alloc)
/// / [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the
/// result term cannot be built.
fn decode_packet_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let packet_type = resolve_type(ctx, args[0])?;
    let bin = args[1];
    let opts_term = args[2];

    // `erl_bif_port.c:1513-1516`: a non-bitstring `Bin` or a non-list `Opts`.
    if !bits::is_bitstring(bin) || (!access::is_list(opts_term) && !access::is_nil(opts_term)) {
        return Err(badarg());
    }
    // `erts_get_aligned_binary_bytes` (`erl_bif_port.c:1551-1556`): the bytes of a
    // byte-aligned bitstring, else `badarg` — `bits::bytes` enforces exactly that.
    let bytes = bits::bytes(bin)?;
    let opts = parse_options(ctx, opts_term, packet_type)?;

    // The one dispatch: the two term-building families, then the length-prefixed
    // ones. An unimplemented type is an honest typed gap raised *inside* its own
    // family module — never a fabricated result.
    let outcome = match packet_type {
        PacketType::SslTls => tls::parse(ctx, packet_type, &bytes, &opts)?,
        PacketType::Http | PacketType::HttpH | PacketType::HttpBin | PacketType::HttpHBin => {
            http::parse(ctx, packet_type, &bytes, &opts)?
        }
        _ => framing::parse(ctx, packet_type, &bytes, &opts)?,
    };

    render(ctx, &bytes, outcome)
}

/// Renders a family parser's [`Outcome`] as the result term.
fn render(ctx: &mut BifContext<'_>, bytes: &[u8], outcome: Outcome) -> Result<Eterm> {
    match outcome {
        Outcome::Invalid => {
            let reason = ctx.intern_atom("invalid")?;
            error_tuple(ctx, reason)
        }
        Outcome::More(None) => {
            let reason = ctx.intern_atom("undefined")?;
            more_tuple(ctx, reason)
        }
        Outcome::More(Some(len)) => {
            let len = ctx.int(len as i64)?;
            more_tuple(ctx, len)
        }
        Outcome::Ok { packet_sz, body } => {
            if packet_sz == 0 || packet_sz > bytes.len() {
                // `erl_bif_port.c:1564-1569`: `{more, PacketSz}` (or `undefined`
                // for the degenerate zero length) — never a partial packet.
                if packet_sz == 0 {
                    let reason = ctx.intern_atom("undefined")?;
                    more_tuple(ctx, reason)
                } else {
                    let len = ctx.int(packet_sz as i64)?;
                    more_tuple(ctx, len)
                }
            } else {
                // A whole packet: the body (`packet_get_body`) and the remainder.
                let ok = ctx.intern_atom("ok")?;
                let packet = match body {
                    // `start <= end <= packet_sz` is the `Body::Binary` contract.
                    Body::Binary { start, end } => bits::make_binary(ctx.heap(), &bytes[start..end])?,
                    Body::Term(term) => term,
                };
                let rest = bits::make_binary(ctx.heap(), &bytes[packet_sz..])?;
                let elems = [ok, packet, rest];
                ctx.tuple(&elems)
            }
        }
    }
}

/// `{error, Reason}` (`erl_bif_port.c:1562-1563`).
fn error_tuple(ctx: &mut BifContext<'_>, reason: Eterm) -> Result<Eterm> {
    let tag = ctx.intern_atom("error")?;
    let elems = [tag, reason];
    ctx.tuple(&elems)
}

/// `{more, Required}` (`erl_bif_port.c:1565-1569`).
fn more_tuple(ctx: &mut BifContext<'_>, required: Eterm) -> Result<Eterm> {
    let tag = ctx.intern_atom("more")?;
    let elems = [tag, required];
    ctx.tuple(&elems)
}

/// The `Type` switch (`erl_bif_port.c:1492-1511`).
///
/// The four integer types and all eleven packet-type atoms are accepted — each
/// valid type maps to its own [`PacketType`], so that the unimplemented ones are
/// recognised (not a `badarg`) and reported as the typed capability gap by their
/// family parser. Everything else is the oracle's `badarg`.
fn resolve_type(ctx: &BifContext<'_>, term: Eterm) -> Result<PacketType> {
    if access::is_small(term) {
        return match signed_value(term) {
            0 => Ok(PacketType::Raw),
            1 => Ok(PacketType::Len1),
            2 => Ok(PacketType::Len2),
            4 => Ok(PacketType::Len4),
            // Any other integer is `badarg` (the C's `default:` arm, with
            // `fvalue = am_badopt`).
            _ => Err(badarg()),
        };
    }
    match atom_name(ctx, term) {
        Some("raw") => Ok(PacketType::Raw),
        Some("line") => Ok(PacketType::Line),
        Some("asn1") => Ok(PacketType::Asn1),
        Some("sunrm") => Ok(PacketType::Sunrm),
        Some("cdr") => Ok(PacketType::Cdr),
        Some("fcgi") => Ok(PacketType::Fcgi),
        Some("tpkt") => Ok(PacketType::Tpkt),
        Some("ssl_tls") => Ok(PacketType::SslTls),
        Some("http") => Ok(PacketType::Http),
        Some("httph") => Ok(PacketType::HttpH),
        Some("http_bin") => Ok(PacketType::HttpBin),
        Some("httph_bin") => Ok(PacketType::HttpHBin),
        _ => Err(badarg()),
    }
}

/// The option loop (`erl_bif_port.c:1518-1548`).
///
/// Each element must be a `{Key, Value}` 2-tuple whose `Key` is one of the three
/// option atoms and whose `Value` converts to a `Uint` (`term_to_Uint`); every
/// other shape — an improper list, a non-tuple, a wrong arity, an unknown key, a
/// non-`Uint` value, a `line_delimiter` outside `line` or above 255 — is
/// `badarg`. A repeated key keeps the last value, as the C does.
fn parse_options(ctx: &BifContext<'_>, opts: Eterm, packet_type: PacketType) -> Result<Options> {
    let mut out = Options { max_plen: 0, trunc_len: 0, delimiter: DEFAULT_DELIMITER };
    let mut cur = opts;
    while !access::is_nil(cur) {
        if !access::is_list(cur) {
            // An improper list tail reaches here (`{packet_size, 1} | foo`).
            return Err(badarg());
        }
        let cell = access::car(cur);
        if access::is_tuple(cell) && access::tuple_size(cell) == 2 {
            let value = access::tuple_element(cell, 1);
            if let Some(val) = uint_value(value) {
                let key = access::tuple_element(cell, 0);
                match atom_name(ctx, key) {
                    Some("packet_size") => out.max_plen = val,
                    Some("line_length") => out.trunc_len = val,
                    Some("line_delimiter") => {
                        // `erl_bif_port.c:1537-1541`: only the `line` type, only
                        // a byte value.
                        if packet_type == PacketType::Line && val <= 255 {
                            out.delimiter = val as u8;
                        } else {
                            return Err(badarg());
                        }
                    }
                    // An unknown key (or a non-atom key) falls out of the C's
                    // switch into `BIF_ERROR(BADARG)`.
                    _ => return Err(badarg()),
                }
                cur = access::cdr(cur);
                continue;
            }
        }
        return Err(badarg());
    }
    Ok(out)
}

/// The C `term_to_Uint` (`erl_term.h`) restricted by the BIF's `val <= UINT_MAX`
/// check (`erl_bif_port.c:1529`).
///
/// Only a non-negative small integer can qualify: every positive bignum is at
/// least `2^60` and therefore above `UINT_MAX`, and `term_to_Uint` rejects a
/// negative either way. `UINT_MAX` on a 64-bit host is `u32::MAX`.
fn uint_value(term: Eterm) -> Option<u64> {
    if !access::is_small(term) {
        return None;
    }
    let value = signed_value(term);
    if (0..=i64::from(u32::MAX)).contains(&value) {
        Some(value as u64)
    } else {
        None
    }
}

/// The length of `term`'s atom name, when `term` is an atom and the shared table
/// spells it.
fn atom_name<'a>(ctx: &'a BifContext<'_>, term: Eterm) -> Option<&'a str> {
    if !access::is_atom(term) {
        return None;
    }
    ctx.atoms().name(atom_val(term))
}

/// Registers `erlang:decode_packet/3` (`bif.tab:503`).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "decode_packet", 3, decode_packet_3);
}
