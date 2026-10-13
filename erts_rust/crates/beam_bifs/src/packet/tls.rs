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

//! The `ssl_tls` (`TCP_PB_SSL_TLS`) packet family.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/packet_parser.c:465-476`
//! (`packet_get_length`'s `TCP_PB_SSL_TLS` arm — the 5-byte header rule and the
//! `Ssl-v2 Client hello` form) plus the shared `remain:` label at `:486-494`;
//! `packet_parser.c:864-884` (`packet_parse_ssl`, the two body shapes) and
//! `../../erts/emulator/beam/erl_bif_port.c:1432-1457` (the `ssl_tls_erl`
//! callback that builds the result term).
//!
//! # Implemented rules
//!
//! Both shapes of the family are implemented, and both end in the same
//! `{ssl_tls, [], ContentType, {Major, Minor}, Body}` 5-tuple (the second
//! element is the empty list `NIL`):
//!
//! * **SSLv2 ClientHello** (`(ptr[0] & 0x80) != 0 && ptr[2] == 1`): the declared
//!   length is the big-endian 16-bit word at `ptr[0..2]` masked with `0x7fff`,
//!   minus the 3-byte SSLv2 header. The body is the *reconstructed* record
//!   `<<1:8, Length:24, Version:16, Data/binary>>` — the 4-byte prefix
//!   `[1, Length as a 24-bit big-endian integer]` followed by `bytes[3..packet_sz]`
//!   — and the reported content type is the synthesized `22` (handshake) with the
//!   version read from `bytes[3..5]`.
//! * **Normal record header** (`<<ContentType:8, Version:16, Length:16>>`): the
//!   content type is `bytes[0]`, the version `bytes[1..3]`, the declared length
//!   the big-endian 16-bit word at `bytes[3..5]`, and the body is
//!   `bytes[5..packet_sz]`.
//!
//! The length arithmetic reproduces the C's `unsigned` (32-bit) locals exactly:
//! the SSLv2 subtraction wraps modulo `2^32`, the `remain:` ceiling is the
//! caller's `packet_size` when given and `INT_MAX - hlen` otherwise, and the
//! returned `hlen + plen` is narrowed to the C `int` (so a wrapped sum whose top
//! bit is set is the caller's `packet_sz < 0` => `{error, invalid}`). An
//! enormous declared length is therefore `{more, N}` or `{error, invalid}`
//! precisely as the C decides it.
//!
//! The one input the C leaves undefined — an SSLv2 hello whose wrapped length
//! leaves fewer than the 3 header bytes (`packet_sz < 3`, reachable only through
//! `{packet_size, N}` with `N >= 2^32 - 3`, where the C's `sys_memcpy` is handed
//! `packet_sz - 3 < 0` and aborts the emulator) — is the honest typed
//! `unsupported_bif` capability gap, never a fabricated packet. `packet_parse_ssl`
//! itself never fails on a defined input, so this family never reports a
//! malformed packet it has already accepted the length of.

use beam_common::Result;
use beam_term::{Eterm, NIL};

use crate::bits;
use crate::context::BifContext;
use crate::error::unsupported_bif;

use super::{Body, Options, Outcome, PacketType};

/// The `TCP_PB_SSL_TLS` header length (`packet_parser.c:466`).
const HDR_LEN: usize = 5;

/// The C `INT_MAX` (`limits.h`): the fallback packet-length ceiling of the
/// `remain:` label (`packet_parser.c:488-490`).
const INT_MAX: u64 = i32::MAX as u64;

/// The content type the SSLv2 ClientHello is rebuilt under — `handshake`
/// (`packet_parser.c:875`).
const SSL2_CONTENT_TYPE: u8 = 22;

/// The frozen `ssl_tls` entry point (see the module docs).
pub(super) fn parse(
    ctx: &mut BifContext<'_>,
    ptype: PacketType,
    bytes: &[u8],
    opts: &Options,
) -> Result<Outcome> {
    // `super::decode_packet_3` routes only `ssl_tls` here; any other type is a
    // caller error this family must not answer with a fabricated packet.
    if ptype != PacketType::SslTls {
        return Err(unsupported_bif());
    }
    // `packet_parser.c:467`: fewer than 5 bytes is "no length known yet".
    if bytes.len() < HDR_LEN {
        return Ok(Outcome::More(None));
    }

    let ssl2 = (bytes[0] & 0x80) != 0 && bytes[2] == 1;
    let plen = if ssl2 {
        // `:470`: `(get_int16(&ptr[0]) & 0x7fff) - 3`, all in `unsigned`
        // arithmetic, so a length field below 3 wraps to just under `2^32`.
        (u32::from(u16::from_be_bytes([bytes[0], bytes[1]])) & 0x7fff).wrapping_sub(3)
    } else {
        // `:474`: the big-endian 16-bit record length, unsigned.
        u32::from(u16::from_be_bytes([bytes[3], bytes[4]]))
    };

    // The shared `remain:` label (`:486-494`).
    let max_plen = if opts.max_plen == 0 {
        (INT_MAX - HDR_LEN as u64) as u32
    } else {
        // `parse_options` already caps the option at `UINT_MAX`; clamp rather
        // than truncate so a wider value can never wrap the comparison below.
        u32::try_from(opts.max_plen).unwrap_or(u32::MAX)
    };
    if plen > max_plen {
        return Ok(Outcome::Invalid);
    }
    // `return hlen + plen;` — `unsigned` arithmetic narrowed to the C `int`;
    // the caller's `packet_sz < 0` test is the `{error, invalid}` branch.
    let packet_sz = (HDR_LEN as u32).wrapping_add(plen) as i32;
    if packet_sz < 0 {
        return Ok(Outcome::Invalid);
    }
    let packet_sz = packet_sz as usize;

    // The body is a term built *now*, so the packet really has to be in the
    // buffer. `super::render` answers `{more, packet_sz}` for an `Ok` of this
    // size and never reads the body — report that directly rather than slicing
    // past the end of `bytes` (the C only reaches `packet_parse` when the whole
    // packet is present, `erl_bif_port.c:1564-1584`).
    if packet_sz > bytes.len() {
        return Ok(Outcome::More(Some(packet_sz)));
    }

    let body = packet_body(ctx, bytes, packet_sz, ssl2)?;
    Ok(Outcome::Ok { packet_sz, body: Body::Term(body) })
}

/// `packet_parse_ssl` (`packet_parser.c:864-884`) together with the `ssl_tls_erl`
/// callback (`erl_bif_port.c:1432-1457`): the `{ssl_tls, [], ContentType,
/// {Major, Minor}, Bin}` result term.
fn packet_body(
    ctx: &mut BifContext<'_>,
    bytes: &[u8],
    packet_sz: usize,
    ssl2: bool,
) -> Result<Eterm> {
    let (content_type, major, minor, bin) = if ssl2 {
        // `packet_parser.c:868-875`: `<<1:8, Length:24, Data/binary>>` — a 4-byte
        // prefix plus the `packet_sz - 3` bytes copied from `buf + 3`. The C
        // hands its `sys_memcpy` the signed `packet_sz - 3`, which is only a
        // defined length at `packet_sz >= 3` (below it the emulator aborts), so
        // that unreachable input is the typed capability gap.
        if packet_sz < 3 {
            return Err(unsupported_bif());
        }
        let mut image = Vec::with_capacity(packet_sz + 1);
        image.push(1);
        let length = (packet_sz - 3) as u32;
        image.push((length >> 16) as u8);
        image.push((length >> 8) as u8);
        image.push(length as u8);
        image.extend_from_slice(&bytes[3..packet_sz]);
        let bin = bits::make_binary(ctx.heap(), &image)?;
        (SSL2_CONTENT_TYPE, bytes[3], bytes[4], bin)
    } else {
        // `packet_parser.c:877-882`: the header fields verbatim, the body
        // `buf + 5` for `packet_sz - 5` bytes.
        let bin = bits::make_binary(ctx.heap(), &bytes[5..packet_sz])?;
        (bytes[0], bytes[1], bytes[2], bin)
    };

    let tag = ctx.atom("ssl_tls")?;
    let content_type = ctx.int(i64::from(content_type))?;
    let major = ctx.int(i64::from(major))?;
    let minor = ctx.int(i64::from(minor))?;
    let version = ctx.tuple(&[major, minor])?;
    // `TUPLE5(hp, am_ssl_tls, NIL, make_small(type), ver, bin)`.
    let elems = [tag, NIL, content_type, version, bin];
    ctx.tuple(&elems)
}
