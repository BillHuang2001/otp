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
//! 483-500) and `../../erts/emulator/beam/packet_parser.h:152-164`
//! (`packet_get_body`).
//!
//! # Implemented subset
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
//! The `asn1`/`sunrm`/`cdr`/`fcgi`/`tpkt` length rules are **not** implemented
//! yet: they return the typed `unsupported_bif` capability gap, never a
//! fabricated length. The `ssl_tls`/`http*` types are routed elsewhere by
//! `super::decode_packet_3` and are rejected here as well.

use beam_common::Result;

use crate::context::BifContext;
use crate::error::unsupported_bif;

use super::{Body, Options, Outcome, PacketType};

/// The C `INT_MAX` (`limits.h`): the fallback packet-length ceiling of the
/// `remain:` label (`packet_parser.c:488-490`).
const INT_MAX: u64 = i32::MAX as u64;

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
        PacketLength::Packet { packet_sz, header_len } => {
            Outcome::Ok { packet_sz, body: Body::Binary { start: header_len, end: packet_sz } }
        }
        PacketLength::More(len) => Outcome::More(len),
        PacketLength::Invalid => Outcome::Invalid,
    })
}

/// `packet_get_length` for this module's types.
///
/// Every other valid packet type — the five unimplemented length rules as well
/// as the `ssl_tls`/`http*` types that `super::decode_packet_3` routes to
/// `tls`/`http` — is the typed capability gap, never a fabricated length.
fn packet_length(packet_type: PacketType, bytes: &[u8], opts: &Options) -> Result<PacketLength> {
    Ok(match packet_type {
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
        PacketType::Asn1
        | PacketType::Sunrm
        | PacketType::Cdr
        | PacketType::Fcgi
        | PacketType::Tpkt
        | PacketType::SslTls
        | PacketType::Http
        | PacketType::HttpH
        | PacketType::HttpBin
        | PacketType::HttpHBin => return Err(unsupported_bif()),
    })
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
