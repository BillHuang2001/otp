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

//! `erlang:decode_packet/3` — the packet length decoder and the `raw` family.
//!
//! Ground truth (read-only):
//! `../../erts/emulator/beam/erl_bif_port.c:1476-1617` (`decode_packet_3`: the
//! type switch, the option loop, the `{ok,…}`/`{more,…}`/`{error,invalid}`
//! dispatch), `../../erts/emulator/beam/packet_parser.c:248-501`
//! (`packet_get_length`, incl. the `more:`/`remain:`/`error:` labels at
//! 483-500), `../../erts/emulator/beam/packet_parser.h:34-50` (`enum
//! PacketParseType`) and `:152-164` (`packet_get_body`).
//!
//! # Implemented types (six)
//!
//! `raw`/`0` (`TCP_PB_RAW`), `1` (`TCP_PB_1`), `2` (`TCP_PB_2`), `4`
//! (`TCP_PB_4`) and `line` (`TCP_PB_LINE_LF`). Each reproduces the C's
//! length-extraction verbatim:
//!
//! * `raw`/`0` — the whole binary is one packet; an empty binary is
//!   `{more, undefined}` (`packet_parser.c:258-263`). `packet_size`/`line_length`
//!   are parsed but never applied (the C returns `n` before the `remain:` label).
//! * `1`/`2`/`4` — a 1/2/4-byte big-endian length prefix
//!   (`packet_parser.c:265-284`), then the shared `remain:` label
//!   (`:486-494`): an effective `max_plen` of `packet_size` when given, else
//!   `INT_MAX - hlen`; `plen > max_plen` is `{error, invalid}`, otherwise the
//!   packet is `hlen + plen` bytes long and the body starts at `hlen`
//!   (`packet_get_body`).
//! * `line` — a delimiter-terminated line (`packet_parser.c:295-322`), the
//!   delimiter being `\n` unless `{line_delimiter, C}` (only valid for `line`,
//!   `C <= 255`). A not-yet-terminated line is `{more, undefined}` unless it
//!   has already reached `line_length` (then the first `line_length` bytes are
//!   the packet), and a terminated line longer than `line_length` is truncated
//!   to it; `packet_size` bounds the line in both cases (`{error, invalid}`).
//!
//! # The result shape
//!
//! `{ok, Packet, Rest}` for a complete packet (the body excludes the length
//! prefix; `Rest` is the remainder of `Bin`), `{more, Length}` when more bytes
//! are needed (`undefined` when the required length is not yet known) and
//! `{error, invalid}` for a malformed/oversized declared length
//! (`erl_bif_port.c:1561-1607`). A declared length that exceeds `Bin` is
//! reported as `{more, Length}` **without allocating** the packet
//! (`erl_bif_port.c:1564-1569`), and every loop here is bounded by the input.
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
//!
//! # Honest capability gaps
//!
//! The other valid packet types — `asn1`, `sunrm`, `cdr`, `fcgi`, `tpkt`,
//! `http`, `httph`, `http_bin`, `httph_bin` and `ssl_tls`
//! (`packet_parser.h:34-50`) — are recognised (so their `Type` is not `badarg`)
//! but return the typed [`unsupported_bif`] capability gap. Their arguments are
//! still validated first, so `decode_packet(asn1, 42, [])` stays `badarg`
//! exactly as the oracle answers it. No packet type ever yields a fabricated
//! result.

use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

/// The default line delimiter — `decode_packet_3`'s `char delimiter = '\n'`
/// (`erl_bif_port.c:1490`).
const DEFAULT_DELIMITER: u8 = b'\n';

/// The C `INT_MAX` (`limits.h`): the fallback packet-length ceiling of the
/// `remain:` label (`packet_parser.c:488-490`).
const INT_MAX: u64 = i32::MAX as u64;

/// The packet type selected by `Type` (`enum PacketParseType`,
/// `packet_parser.h:34-50`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PacketType {
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
    /// A valid packet type this Milestone does not implement — the typed
    /// [`unsupported_bif`] capability gap (see the module docs).
    Unimplemented,
}

/// The outcome of the length extraction — the C `packet_get_length` plus the
/// `{ok,…}` / `{more,…}` / `{error,invalid}` decision in `decode_packet_3`
/// (`erl_bif_port.c:1558-1570`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PacketLength {
    /// A whole packet of `packet_sz` bytes, whose body begins at `header_len`
    /// (`packet_get_body`, `packet_parser.h:152-164`).
    Packet { packet_sz: usize, header_len: usize },
    /// More bytes are needed: `Some(n)` is the total size required (`{more, n}`),
    /// `None` the C's `0` length (`{more, undefined}`).
    More(Option<usize>),
    /// A malformed/oversized declared length (`packet_get_length` returning `< 0`).
    Invalid,
}

/// The `packet_size`/`line_length`/`line_delimiter` options
/// (`erl_bif_port.c:1479-1548`); `0` means "no limit" in the C.
#[derive(Clone, Copy, Debug)]
struct Options {
    max_plen: u64,
    trunc_len: u64,
    delimiter: u8,
}

/// `erlang:decode_packet/3`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for any argument the
/// oracle rejects, the typed [`unsupported_bif`] raise for a valid packet type
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

    // A valid-but-unimplemented type is an honest typed gap, reported only after
    // the argument validation above (so a bad `Bin`/`Opts` still answers `badarg`).
    if packet_type == PacketType::Unimplemented {
        return Err(unsupported_bif());
    }

    match packet_length(packet_type, &bytes, &opts) {
        PacketLength::Invalid => {
            let reason = ctx.atom("invalid")?;
            error_tuple(ctx, reason)
        }
        PacketLength::More(None) => {
            let reason = ctx.atom("undefined")?;
            more_tuple(ctx, reason)
        }
        PacketLength::More(Some(len)) => {
            let len = ctx.int(len as i64)?;
            more_tuple(ctx, len)
        }
        PacketLength::Packet { packet_sz, header_len } => {
            if packet_sz == 0 || packet_sz > bytes.len() {
                // `erl_bif_port.c:1564-1569`: `{more, PacketSz}` (or `undefined`
                // for the degenerate zero length) — never a partial packet.
                if packet_sz == 0 {
                    let reason = ctx.atom("undefined")?;
                    more_tuple(ctx, reason)
                } else {
                    let len = ctx.int(packet_sz as i64)?;
                    more_tuple(ctx, len)
                }
            } else {
                // A whole packet: the body (`packet_get_body`) and the remainder.
                let ok = ctx.atom("ok")?;
                let body = bits::make_binary(ctx.heap(), &bytes[header_len..packet_sz])?;
                let rest = bits::make_binary(ctx.heap(), &bytes[packet_sz..])?;
                let elems = [ok, body, rest];
                ctx.tuple(&elems)
            }
        }
    }
}

/// `{error, Reason}` (`erl_bif_port.c:1562-1563`).
fn error_tuple(ctx: &mut BifContext<'_>, reason: Eterm) -> Result<Eterm> {
    let tag = ctx.atom("error")?;
    let elems = [tag, reason];
    ctx.tuple(&elems)
}

/// `{more, Required}` (`erl_bif_port.c:1565-1569`).
fn more_tuple(ctx: &mut BifContext<'_>, required: Eterm) -> Result<Eterm> {
    let tag = ctx.atom("more")?;
    let elems = [tag, required];
    ctx.tuple(&elems)
}

/// The `Type` switch (`erl_bif_port.c:1492-1511`).
///
/// The four integer types and the packet-type atoms are accepted; the valid but
/// unimplemented atoms are [`PacketType::Unimplemented`]; everything else is the
/// oracle's `badarg`.
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
        // Valid packet types (`packet_parser.h:34-50`) this Milestone does not
        // implement: recognised so they are not a `badarg`, then reported as the
        // typed capability gap.
        Some(
            "asn1" | "sunrm" | "cdr" | "fcgi" | "tpkt" | "http" | "httph" | "http_bin"
            | "httph_bin" | "ssl_tls",
        ) => Ok(PacketType::Unimplemented),
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

// ---------------------------------------------------------------------------
// Length extraction (`packet_get_length`, packet_parser.c:248-501)
// ---------------------------------------------------------------------------

/// `packet_get_length` for the six implemented types.
fn packet_length(packet_type: PacketType, bytes: &[u8], opts: &Options) -> PacketLength {
    match packet_type {
        PacketType::Raw => {
            // `packet_parser.c:258-263`: the whole buffer is one packet (so
            // `packet_size`/`line_length` never apply), an empty buffer is `more`.
            if bytes.is_empty() {
                PacketLength::More(None)
            } else {
                PacketLength::Packet { packet_sz: bytes.len(), header_len: 0 }
            }
        }
        PacketType::Len1 => length_prefix(1, bytes, opts),
        PacketType::Len2 => length_prefix(2, bytes, opts),
        PacketType::Len4 => length_prefix(4, bytes, opts),
        PacketType::Line => line_length(bytes, opts),
        // Handled by `decode_packet_3` before this point.
        PacketType::Unimplemented => PacketLength::Invalid,
    }
}

/// `TCP_PB_1`/`TCP_PB_2`/`TCP_PB_4`: a big-endian length prefix of `hlen` bytes,
/// then the `remain:` label (`packet_parser.c:265-284, 486-494`).
fn length_prefix(hlen: usize, bytes: &[u8], opts: &Options) -> PacketLength {
    if bytes.len() < hlen {
        return PacketLength::More(None);
    }
    let plen = match hlen {
        1 => u64::from(bytes[0]),
        2 => u64::from(u16::from_be_bytes([bytes[0], bytes[1]])),
        _ => u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
    };
    // The default ceiling is `INT_MAX - hlen`, so `hlen + plen` never overflows.
    let max_plen = if opts.max_plen == 0 { INT_MAX - hlen as u64 } else { opts.max_plen };
    if plen > max_plen {
        return PacketLength::Invalid;
    }
    PacketLength::Packet { packet_sz: hlen + plen as usize, header_len: hlen }
}

/// `TCP_PB_LINE_LF` (`packet_parser.c:295-322`).
fn line_length(bytes: &[u8], opts: &Options) -> PacketLength {
    let n = bytes.len() as u64;
    match bytes.iter().position(|&b| b == opts.delimiter) {
        None => {
            if opts.max_plen != 0 && n > opts.max_plen {
                PacketLength::Invalid
            } else if opts.trunc_len != 0 && n >= opts.trunc_len {
                // The buffer is full: the first `trunc_len` bytes are the packet.
                PacketLength::Packet { packet_sz: opts.trunc_len as usize, header_len: 0 }
            } else {
                PacketLength::More(None)
            }
        }
        Some(pos) => {
            let len = pos as u64 + 1; // including the delimiter
            if opts.max_plen != 0 && len > opts.max_plen {
                PacketLength::Invalid
            } else if opts.trunc_len != 0 && len > opts.trunc_len {
                // A too-long terminated line is truncated to `trunc_len`.
                PacketLength::Packet { packet_sz: opts.trunc_len as usize, header_len: 0 }
            } else {
                PacketLength::Packet { packet_sz: len as usize, header_len: 0 }
            }
        }
    }
}

/// Registers `erlang:decode_packet/3` (`bif.tab:503`).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "decode_packet", 3, decode_packet_3);
}
