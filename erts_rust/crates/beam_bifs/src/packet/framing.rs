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

//! The length-prefixed `decode_packet/3` families.
//!
//! Ground truth (read-only):
//! `../../erts/emulator/beam/packet_parser.c:248-501`
//! (`packet_get_length`, incl. the `more:`/`remain:`/`error:` labels at
//! 483-500), `../../erts/emulator/beam/packet_parser.h:152-164`
//! (`packet_get_body`) and the `get_int*` readers (`sys.h:1273-1318`).
//!
//! # Implemented subset
//!
//! The `raw`/`line` rules, the integer length prefixes and **all five**
//! length-framed protocol headers are implemented:
//!
//! * `raw`/`0` (`TCP_PB_RAW`) — the whole binary is one packet; an empty binary
//!   is `{more, undefined}` (`packet_parser.c:258-263`). `packet_size`/
//!   `line_length` are parsed but never applied (the C returns `n` before the
//!   `remain:` label).
//! * `1`/`2`/`4` (`TCP_PB_1`/`_2`/`_4`) — a 1/2/4-byte big-endian length prefix
//!   (`:265-284`), then the shared `remain:` label (`:486-494`): an effective
//!   `max_plen` of `packet_size` when given, else `INT_MAX - hlen`;
//!   `plen > max_plen` is `{error, invalid}`, otherwise the packet length is
//!   `hlen + plen` summed in the C's 32-bit `unsigned` and narrowed to the
//!   returned `int`, and the body starts at `hlen` (`packet_get_body`).
//! * `line` (`TCP_PB_LINE_LF`) — a delimiter-terminated line (`:295-322`), the
//!   delimiter being `\n` unless `{line_delimiter, C}` (only valid for `line`,
//!   `C <= 255`). A not-yet-terminated line is `{more, undefined}` unless it
//!   has already reached `line_length` (then the first `line_length` bytes are
//!   the packet), and a terminated line longer than `line_length` is truncated
//!   to it; `packet_size` bounds the line in both cases (`{error, invalid}`).
//! * `sunrm` (`TCP_PB_RM`, `:286-293`) — a 4-byte header whose big-endian
//!   length has its top bit masked off (an "end of record" flag).
//! * `asn1` (`TCP_PB_ASN1`, `:324-363`) — the short **and** the long form of the
//!   ASN.1 tag/length header, ported byte for byte: the long-tag continuation
//!   run, the `nn`-remaining bookkeeping, the `0..=4`-byte long-length payloads
//!   and the `n < 2` / `nn < 2` / `nn < length` "need more" conditions; a
//!   long-length selector above 4 is `{error, invalid}`.
//! * `cdr` (`TCP_PB_CDR`, `:365-377`) — the 12-byte `GIOP` header, the magic
//!   check and the `flags`-selected byte order of the 4-byte `message_size`.
//! * `fcgi` (`TCP_PB_FCGI`, `:379-389`) — the 8-byte header, the
//!   `version == 1` check and `contentLength + paddingLength`; its body drops
//!   the trailing padding (`packet_parser.h:158-160`).
//! * `tpkt` (`TCP_PB_TPKT`, `:451-463`) — the 4-byte header, the `vrsn == 3`
//!   check and the header-relative declared length in C's `unsigned`
//!   arithmetic.
//!
//! `ssl_tls` and the four `http*` types are routed elsewhere by
//! `super::decode_packet_3` (`tls`/`http`) and are rejected here as the typed
//! `unsupported_bif` capability gap.

use beam_common::Result;

use crate::context::BifContext;
use crate::error::unsupported_bif;

use super::{Body, Options, Outcome, PacketType};

/// The C `INT_MAX` (`limits.h`): the fallback packet-length ceiling of the
/// `remain:` label (`packet_parser.c:488-490`).
const INT_MAX: u64 = i32::MAX as u64;

/// `CDR_MAGIC` (`packet_parser.c:215`).
const CDR_MAGIC: [u8; 4] = *b"GIOP";
/// `sizeof(struct cdr_head)` (`packet_parser.c:217-225`): the 4-byte magic, the
/// `major`/`minor`/`flags`/`message_type` bytes and the 4-byte `message_size`.
const CDR_HEADER_LEN: usize = 12;
/// `sizeof(struct tpkt_head)` (`packet_parser.c:229-233`).
const TPKT_HEADER_LEN: usize = 4;
/// `TPKT_VRSN` (`packet_parser.c:227`).
const TPKT_VRSN: u8 = 3;
/// `sizeof(struct fcgi_head)` (`packet_parser.h:134-145`).
const FCGI_HEADER_LEN: usize = 8;
/// `FCGI_VERSION_1` (`packet_parser.h:133`).
const FCGI_VERSION_1: u8 = 1;

/// The outcome of the length extraction — the C `packet_get_length` plus the
/// `{ok,…}` / `{more,…}` / `{error,invalid}` decision in `decode_packet_3`
/// (`erl_bif_port.c:1558-1570`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PacketLength {
    /// A whole packet of `packet_sz` bytes, of which `body_start..body_end` is
    /// the body (`packet_get_body`, `packet_parser.h:152-164`).
    Packet { packet_sz: usize, body_start: usize, body_end: usize },
    /// More bytes are needed: `Some(n)` is the total size required (`{more, n}`),
    /// `None` the C's `0` length (`{more, undefined}`).
    More(Option<usize>),
    /// A malformed/oversized declared length (`packet_get_length` returning `< 0`).
    Invalid,
}

/// The frozen family-parser entry point (`super::decode_packet_3`): the packet
/// type, the aligned bytes of `Bin` and the parsed options in, an [`Outcome`]
/// out. The context is part of the seam for the families that build terms; a
/// length rule needs none — the rendering happens in `super`.
pub(super) fn parse(
    _ctx: &mut BifContext<'_>,
    ptype: PacketType,
    bytes: &[u8],
    opts: &Options,
) -> Result<Outcome> {
    Ok(match packet_length(ptype, bytes, opts)? {
        PacketLength::Packet { packet_sz, body_start, body_end } => {
            Outcome::Ok { packet_sz, body: Body::Binary { start: body_start, end: body_end } }
        }
        PacketLength::More(len) => Outcome::More(len),
        PacketLength::Invalid => Outcome::Invalid,
    })
}

/// `packet_get_length` for this module's types.
///
/// Every other valid packet type — the `ssl_tls`/`http*` types that
/// `super::decode_packet_3` routes to `tls`/`http` — is the typed capability
/// gap, never a fabricated length.
fn packet_length(packet_type: PacketType, bytes: &[u8], opts: &Options) -> Result<PacketLength> {
    Ok(match packet_type {
        PacketType::Raw => {
            // `packet_parser.c:258-263`: the whole buffer is one packet (so
            // `packet_size`/`line_length` never apply), an empty buffer is `more`.
            if bytes.is_empty() {
                PacketLength::More(None)
            } else {
                whole_packet(bytes.len())
            }
        }
        PacketType::Len1 => length_prefix(1, bytes, opts),
        PacketType::Len2 => length_prefix(2, bytes, opts),
        PacketType::Len4 => length_prefix(4, bytes, opts),
        PacketType::Line => line_length(bytes, opts),
        PacketType::Sunrm => sunrm_length(bytes, opts),
        PacketType::Asn1 => asn1_length(bytes, opts),
        PacketType::Cdr => cdr_length(bytes, opts),
        PacketType::Fcgi => fcgi_length(bytes, opts),
        PacketType::Tpkt => tpkt_length(bytes, opts),
        PacketType::SslTls
        | PacketType::Http
        | PacketType::HttpH
        | PacketType::HttpBin
        | PacketType::HttpHBin => return Err(unsupported_bif()),
    })
}

/// A complete packet whose body is every byte of it — `packet_get_body`'s
/// `default:` arm (`packet_parser.h:161-163`, "Return other packets as is").
fn whole_packet(packet_sz: usize) -> PacketLength {
    PacketLength::Packet { packet_sz, body_start: 0, body_end: packet_sz }
}

/// The C `remain:` label (`packet_parser.c:486-494`), shared by every
/// length-framed type — the five protocol headers below and the `1`/`2`/`4`
/// length prefixes (which reach it through [`remain_body`]).
///
/// `plen` is the *declared* payload length. In the C it is `unsigned`, i.e. a
/// 32-bit value: every producer below either reads a 1..=4-byte big-endian
/// integer (at most `u32::MAX`) or subtracts in `unsigned` arithmetic, so
/// `plen <= u32::MAX` always holds. `hlen` is the header length in bytes and
/// `max_plen` the ceiling — `packet_size` when one was given, else
/// `INT_MAX - hlen`; a declared length above the ceiling is `{error, invalid}`.
///
/// The C then returns `hlen + plen` as an `int`, and `decode_packet_3` tests the
/// result for `< 0` (`erl_bif_port.c:1561-1569`). When a caller supplies a
/// `packet_size` above `INT_MAX` the ceiling no longer bounds the sum, so both
/// the 32-bit wraparound **and** the signed reinterpretation become observable.
/// Probing the oracle pins them
/// (`decode_packet(tpkt, <<3,0,0,3,65,66>>, [{packet_size,4294967295}])` =>
/// `{ok,<<3,0,0>>,<<3,65,66>>}`, `<<3,0,0,0,…>>` => `{more,undefined}`,
/// `decode_packet(4, <<255,255,255,252,65>>, [{packet_size,4294967295}])` =>
/// `{more,undefined}`, `decode_packet(cdr, <<"GIOP",1,2,0,0,255,255,255,240,…>>,
/// …)` => `{error,invalid}`), so they are reproduced here rather than
/// "corrected".
///
/// The body of every one of these types is the whole packet
/// (`packet_get_body`'s `default:` arm); `fcgi` narrows it afterwards.
fn remain(hlen: usize, plen: u64, opts: &Options) -> PacketLength {
    remain_body(hlen, plen, 0, opts)
}

/// [`remain`] with the body's start offset made explicit.
///
/// `packet_get_body` advances the body pointer past the length prefix for
/// `TCP_PB_1`/`_2`/`_4` (`packet_parser.h:155-157`), so their body begins at
/// `hlen` rather than `0`; every other length-framed type returns the whole
/// packet (`body_start == 0`).
fn remain_body(hlen: usize, plen: u64, body_start: usize, opts: &Options) -> PacketLength {
    let max_plen = if opts.max_plen == 0 {
        // `INT_MAX - hlen` in C's `unsigned` arithmetic (`packet_parser.c:487`
        // asserts `INT_MAX >= hlen`, so this is a plain subtraction in practice).
        u64::from((INT_MAX as u32).wrapping_sub(hlen as u32))
    } else {
        opts.max_plen
    };
    if plen > max_plen {
        return PacketLength::Invalid;
    }
    let packet_sz = (hlen as u32).wrapping_add(plen as u32) as i32;
    if packet_sz < 0 {
        return PacketLength::Invalid;
    }
    let packet_sz = packet_sz as usize;
    // `packet_get_body` computes the body length as `packet_sz - body_start`. The
    // wrapped 32-bit sum can land below the header size only for `TCP_PB_1`/`_2`/
    // `_4` (whose body starts at `hlen`), where the C is handed a *negative* body
    // length that wrecks the emulator (the oracle aborts on it). Report the
    // header as inconsistent instead of building an inverted body. A zero packet
    // size is a genuine `{more, undefined}` that never reaches the body, so it
    // passes through for the caller to render.
    if packet_sz != 0 && packet_sz < body_start {
        return PacketLength::Invalid;
    }
    PacketLength::Packet { packet_sz, body_start, body_end: packet_sz }
}

/// The C `get_int32` (`sys.h:1274-1277`): a big-endian 32-bit read. The macro's
/// `int` result is only ever stored into the `unsigned` `plen`, so the bit
/// pattern — i.e. the `u32` value — is what matters.
fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The C `get_little_int32` (`sys.h:1279-1282`).
fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// The C `get_int16` (`sys.h:1310-1311`) — a big-endian 16-bit read, always
/// non-negative.
fn be_u16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

/// The C `get_int24` (`sys.h:1300-1302`) — a big-endian 24-bit read.
fn be_u24(bytes: &[u8]) -> u32 {
    (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2])
}

/// `TCP_PB_1`/`TCP_PB_2`/`TCP_PB_4`: a big-endian length prefix of `hlen` bytes,
/// then the `remain:` label (`packet_parser.c:265-284, 486-494`).
fn length_prefix(hlen: usize, bytes: &[u8], opts: &Options) -> PacketLength {
    if bytes.len() < hlen {
        return PacketLength::More(None);
    }
    let plen = match hlen {
        1 => u64::from(bytes[0]),
        2 => u64::from(be_u16(&bytes[0..2])),
        _ => u64::from(be_u32(&bytes[0..4])),
    };
    // The shared `remain:` label, with the body past the `hlen`-byte prefix
    // (`packet_get_body`'s `TCP_PB_1`/`_2`/`_4` arms).
    remain_body(hlen, plen, hlen, opts)
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
                whole_packet(opts.trunc_len as usize)
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
                whole_packet(opts.trunc_len as usize)
            } else {
                whole_packet(len as usize)
            }
        }
    }
}

/// `TCP_PB_RM` (`packet_parser.c:286-293`): `[L3,L2,L1,L0 | Data]` where the
/// top bit of the big-endian length signals the end of record and is masked off.
fn sunrm_length(bytes: &[u8], opts: &Options) -> PacketLength {
    const HLEN: usize = 4;
    if bytes.len() < HLEN {
        return PacketLength::More(None);
    }
    let plen = u64::from(be_u32(&bytes[0..4]) & 0x7fff_ffff);
    remain(HLEN, plen, opts)
}

/// `TCP_PB_ASN1` (`packet_parser.c:324-363`): the ASN.1 tag/length header, short
/// and long forms.
///
/// `tptr` is the C's cursor and `nn` the number of bytes still available from
/// it; the two are kept in lockstep (`tptr + nn == bytes.len()`) so that every
/// read below is in bounds by construction — exactly the property the C relies
/// on when it reads `*tptr` after an `nn`-guarded pointer bump.
fn asn1_length(bytes: &[u8], opts: &Options) -> PacketLength {
    let n = bytes.len();
    if n < 2 {
        return PacketLength::More(None);
    }
    let mut tptr = 1; // `*tptr++`
    let mut nn = n - 1; // `nn--`
    if bytes[0] & 0x1f == 0x1f {
        // Long tag format: skip the continuation bytes (`nn != 0` guards the
        // read, so `tptr < n` inside the loop).
        while nn != 0 && bytes[tptr] & 0x80 == 0x80 {
            tptr += 1;
            nn -= 1;
        }
        if nn < 2 {
            return PacketLength::More(None);
        }
        tptr += 1;
        nn -= 1;
    }
    // `tptr` now points at the length field and `nn` bytes remain from it.
    let length = u64::from(bytes[tptr] & 0x7f);
    let plen = if bytes[tptr] & 0x80 == 0x80 {
        // Long length format.
        tptr += 1;
        nn -= 1;
        if (nn as u64) < length {
            return PacketLength::More(None);
        }
        // The C advances `tptr` past the length payload (`tptr += 1/2/3/4`), so
        // `hlen` includes it.
        let (value, size) = match length {
            0 => (0, 0),
            1 => (u64::from(bytes[tptr]), 1),
            2 => (u64::from(be_u16(&bytes[tptr..tptr + 2])), 2),
            3 => (u64::from(be_u24(&bytes[tptr..tptr + 3])), 3),
            4 => (u64::from(be_u32(&bytes[tptr..tptr + 4])), 4),
            // The C's `default: goto error` — a selector above 4 is malformed.
            _ => return PacketLength::Invalid,
        };
        tptr += size;
        value
    } else {
        tptr += 1;
        length
    };
    remain(tptr, plen, opts)
}

/// `TCP_PB_CDR` (`packet_parser.c:365-377`, `struct cdr_head` at `:217-225`).
fn cdr_length(bytes: &[u8], opts: &Options) -> PacketLength {
    if bytes.len() < CDR_HEADER_LEN {
        return PacketLength::More(None);
    }
    if bytes[0..4] != CDR_MAGIC {
        return PacketLength::Invalid;
    }
    let message_size = &bytes[8..12];
    // `flags` is byte 6; bit 0 selects the byte order of `message_size`.
    let plen = if bytes[6] & 0x01 != 0 {
        u64::from(le_u32(message_size))
    } else {
        u64::from(be_u32(message_size))
    };
    remain(CDR_HEADER_LEN, plen, opts)
}

/// `TCP_PB_FCGI` (`packet_parser.c:379-389`, `struct fcgi_head` at
/// `packet_parser.h:134-145`).
fn fcgi_length(bytes: &[u8], opts: &Options) -> PacketLength {
    if bytes.len() < FCGI_HEADER_LEN {
        return PacketLength::More(None);
    }
    if bytes[0] != FCGI_VERSION_1 {
        return PacketLength::Invalid;
    }
    let content = (u64::from(bytes[4]) << 8) | u64::from(bytes[5]);
    let padding = u64::from(bytes[6]);
    let plen = content + padding;
    match remain(FCGI_HEADER_LEN, plen, opts) {
        // `packet_get_body` (`packet_parser.h:158-160`): `lenp -= paddingLength`.
        // `packet_sz == FCGI_HEADER_LEN + content + padding` and `padding <= 255`,
        // so the body never shrinks below the 8-byte header.
        PacketLength::Packet { packet_sz, .. } => PacketLength::Packet {
            packet_sz,
            body_start: 0,
            body_end: packet_sz - padding as usize,
        },
        other => other,
    }
}

/// `TCP_PB_TPKT` (`packet_parser.c:451-463`, `struct tpkt_head` at `:229-233`).
fn tpkt_length(bytes: &[u8], opts: &Options) -> PacketLength {
    if bytes.len() < TPKT_HEADER_LEN {
        return PacketLength::More(None);
    }
    if bytes[0] != TPKT_VRSN {
        return PacketLength::Invalid;
    }
    // `plen = get_int16(packet_length) - hlen` in C's `unsigned` arithmetic: a
    // declared length below the header size wraps to a huge `plen`, which the
    // `remain:` ceiling then rejects.
    let declared = u32::from(be_u16(&bytes[2..4]));
    let plen = u64::from(declared.wrapping_sub(TPKT_HEADER_LEN as u32));
    remain(TPKT_HEADER_LEN, plen, opts)
}
