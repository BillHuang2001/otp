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

//! The http packet family — `http`, `httph`, `http_bin` and `httph_bin`.
//!
//! Ground truth (read-only):
//! * the length rule — `../../erts/emulator/beam/packet_parser.c:390-450`
//!   (`TCP_PB_HTTPH`/`HTTPH_BIN` set the header state and fall through to
//!   `TCP_PB_HTTP`/`HTTP_BIN`, the `data \r\n(SP data\r\n)*` rule with the
//!   `line_length`-as-`max_plen` fallback at 404-410);
//! * the message parse — `packet_parser.c:662-862` (`packet_parse_http`), with
//!   `http_hash_lookup`/`http_parse_absoluteURI`/`http_parse_uri` at 504-652;
//! * the header/method tables and the `tspecial` class — `packet_parser.c:62-212`
//!   (52 header names, 7 methods, `HTTP_MAX_NAME_LEN = 50`);
//! * the term builders — `../../erts/emulator/beam/erl_bif_port.c:1276-1430`
//!   (`http_bld_string`/`http_response_erl`/`http_bld_uri`/`http_request_erl`/
//!   `http_header_erl`/`http_eoh_erl`/`http_error_erl`).
//!
//! # Implemented subset
//!
//! **All four types are implemented**, as a faithful port of the routines above:
//!
//! * both length rules (request/response mode and header mode, including the
//!   continuation-line scan and the `packet_size`/`line_length` ceiling);
//! * the request line (`{http_request, Method, Uri, {Major,Minor}}`, and the
//!   `{0,9}` version of a request that stops after the URI), the status line
//!   (`{http_response, {Major,Minor}, Status, Phrase}`), the header field
//!   (`{http_header, Bit, Name, Oname, Value}` with the canonical-name
//!   normalisation, the 52-entry name table and the `> 50`-character arm that
//!   emits the *original* name), the blank header line (`http_eoh`) and the
//!   malformed line (`{http_error, Line}`);
//! * every `Uri` shape — the `'*'` atom, `{abs_path, S}`, `{absoluteURI, http |
//!   https, Host, Port | undefined, Path}` (including the IPv6 `[...]` form with
//!   its port), a bare string and `{scheme, S1, S2}`.
//!
//! The counts accumulated by the C (`major`, `minor`, `status`, the absolute-URI
//! port) are `int` there and wrap on overflow; they are accumulated in the same
//! 32-bit wrapping arithmetic here, so an over-long digit run reproduces the
//! oracle's (possibly negative) value instead of a wrong one.
//!
//! Two C internals are deliberately not reproduced because they are not
//! observable: the header/method hash buckets (their only effect is to *select*
//! an entry, which an exact `(length, bytes)` scan decides identically — the C's
//! own comparison is byte-exact too), and the `hash_update` state that only feeds
//! that lookup. Everything the C returns is reproduced exactly; a shape the port
//! cannot decide would be reported as [`unsupported_bif`] rather than guessed,
//! but no such shape remains for these four types.
//!
//! [`unsupported_bif`]: crate::error::unsupported_bif

use beam_common::Result;
use beam_term::Eterm;

use crate::bits;
use crate::context::BifContext;

use super::{Body, Options, Outcome, PacketType};

/// `HTTP_MAX_NAME_LEN` (`packet_parser.c:64`): a header name up to and including
/// this length is the case-normalised buffer, a longer one the original bytes.
const MAX_NAME_LEN: usize = 50;

/// The 52 canonical header names in `http_hdr_strings` order
/// (`packet_parser.c:68-122`); the emitted `Bit` is the 1-based index into this
/// table and `Name` its atom.
static HEADER_NAMES: [&str; 52] = [
    "Cache-Control",
    "Connection",
    "Date",
    "Pragma",
    "Transfer-Encoding",
    "Upgrade",
    "Via",
    "Accept",
    "Accept-Charset",
    "Accept-Encoding",
    "Accept-Language",
    "Authorization",
    "From",
    "Host",
    "If-Modified-Since",
    "If-Match",
    "If-None-Match",
    "If-Range",
    "If-Unmodified-Since",
    "Max-Forwards",
    "Proxy-Authorization",
    "Range",
    "Referer",
    "User-Agent",
    "Age",
    "Location",
    "Proxy-Authenticate",
    "Public",
    "Retry-After",
    "Server",
    "Vary",
    "Warning",
    "Www-Authenticate",
    "Allow",
    "Content-Base",
    "Content-Encoding",
    "Content-Language",
    "Content-Length",
    "Content-Location",
    "Content-Md5",
    "Content-Range",
    "Content-Type",
    "Etag",
    "Expires",
    "Last-Modified",
    "Accept-Ranges",
    "Set-Cookie",
    "Set-Cookie2",
    "X-Forwarded-For",
    "Cookie",
    "Keep-Alive",
    "Proxy-Connection",
];

/// The 7 canonical method names in `http_meth_strings` order
/// (`packet_parser.c:125-134`).
static METHOD_NAMES: [&str; 7] = ["OPTIONS", "GET", "HEAD", "POST", "PUT", "DELETE", "TRACE"];

/// The C `tspecial` table (`http_init`, `packet_parser.c:181-191`).
///
/// `tspecial[x]` is 1 for `x <= 32`, 0 for `33..=126` except the delimiters of
/// `"()<>@,;:\\\"/[]?={} \t"`, 0 for `127` (never written by the loop that stops
/// at 127) and 1 for every byte at or above 128 (`is_tspecial`'s `else` arm).
static TSPECIAL_DELIMITERS: &[u8] = b"()<>@,;:\\\"/[]?={} \t";

/// `is_tspecial(x)` (`packet_parser.c:145`).
fn is_tspecial(byte: u8) -> bool {
    match byte {
        // `(x > 32) && (x < 128)` fails for both arms, so `tspecial[x]` is not
        // even read: every byte <= 32 and every byte >= 128 is a tspecial.
        0..=32 | 128..=255 => true,
        // `tspecial[127]` is left at 0 by the `http_init` loop (`i < 127`).
        127 => false,
        _ => TSPECIAL_DELIMITERS.contains(&byte),
    }
}

/// `SP(ptr)` (`packet_parser.c:144`) — a space or a horizontal tab.
fn is_sp(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

/// The frozen family-parser entry point (`super::decode_packet_3`): the packet
/// type, the aligned bytes of `Bin` and the parsed options in, an [`Outcome`]
/// out. The http family builds terms, so it hands its packet back through
/// [`Body::Term`].
pub(super) fn parse(
    ctx: &mut BifContext<'_>,
    ptype: PacketType,
    bytes: &[u8],
    opts: &Options,
) -> Result<Outcome> {
    // `httph`/`httph_bin` start in header mode: `packet_get_length` sets the
    // state and falls through to the request/response rule.
    let header_mode = matches!(ptype, PacketType::HttpH | PacketType::HttpHBin);
    // `pca.string_as_bin` (`erl_bif_port.c:1579-1580`): `http_bin`/`httph_bin`
    // emit every string as a binary.
    let as_binary = matches!(ptype, PacketType::HttpBin | PacketType::HttpHBin);

    let packet_sz = match packet_length(header_mode, bytes, opts) {
        Length::Invalid => return Ok(Outcome::Invalid),
        Length::More => return Ok(Outcome::More(None)),
        Length::Packet(packet_sz) => packet_sz,
    };
    let packet = &bytes[..packet_sz];

    let term = match message(ctx, packet, header_mode, as_binary)? {
        Parsed::Term(term) => term,
        // `packet_parse` (`packet_parser.h:170-177`): a negative
        // `packet_parse_http` is reported as `{http_error, Line}` over the
        // **whole** packet.
        Parsed::Malformed => {
            let line = build_string(ctx, as_binary, packet)?;
            let tag = ctx.intern_atom("http_error")?;
            ctx.tuple(&[tag, line])?
        }
    };
    Ok(Outcome::Ok { packet_sz, body: Body::Term(term) })
}

/// The outcome of the length extraction — the http arm of `packet_get_length`
/// (`packet_parser.c:390-450`) plus its `done:`/`more:`/`error:` labels.
enum Length {
    /// A whole packet of this many bytes (the C's `plen` at `done:`).
    Packet(usize),
    /// `more:` — a length the C reports as `0` => `{more, undefined}`.
    More,
    /// `error:` — the C's `-1` => `{error, invalid}`.
    Invalid,
}

/// `packet_get_length` for the four http types.
///
/// `header_mode` is the initial state (`httph`/`httph_bin` start at `!0`). The
/// scan is the C's: the first line ending, extended over every continuation line
/// (`SP data\r\n`) while in header mode — but *not* past the buffer: a packet
/// that ends exactly at the buffer's last line ending is `more`, since one more
/// byte would decide whether the line continues.
fn packet_length(header_mode: bool, bytes: &[u8], opts: &Options) -> Length {
    let n = bytes.len();
    // `packet_parser.c:397-399`: a bare `\n` / `\r\n` buffer is a whole packet
    // regardless of `packet_size`.
    if (n == 1 && bytes[0] == b'\n') || (n == 2 && bytes[0] == b'\r' && bytes[1] == b'\n') {
        return Length::Packet(n);
    }

    // `packet_parser.c:404-410`: an unset `packet_size` falls back to
    // `line_length`, for the old `decode_packet` users that limited http lines
    // that way.
    let max_plen = if opts.max_plen == 0 { opts.trunc_len } else { opts.max_plen };

    // The C searches `[ptr1, ptr1 + len)` for a `\n`, always measuring `plen`
    // from the packet start. `start + window == n` on every iteration.
    let mut start = 0usize;
    let mut window = n;
    loop {
        let Some(offset) = bytes[start..start + window].iter().position(|&b| b == b'\n') else {
            // No line ending left: only the full-buffer ceiling can reject it.
            if max_plen != 0 && n as u64 >= max_plen {
                return Length::Invalid;
            }
            return Length::More;
        };
        let newline = start + offset;
        let plen = newline + 1;
        if !header_mode {
            if max_plen != 0 && plen as u64 > max_plen {
                return Length::Invalid;
            }
            return Length::Packet(plen);
        }
        if plen < n {
            // A header value continues on the next line when the byte after the
            // line ending is a space or a tab (`plen > 2` excludes a bare
            // `\r\n`, which is the end of the headers).
            if is_sp(bytes[newline + 1]) && plen > 2 {
                start = newline + 1;
                window = n - plen;
            } else {
                if max_plen != 0 && plen as u64 > max_plen {
                    return Length::Invalid;
                }
                return Length::Packet(plen);
            }
        } else {
            if max_plen != 0 && plen as u64 > max_plen {
                return Length::Invalid;
            }
            return Length::More;
        }
    }
}

/// The outcome of `packet_parse_http` for one packet.
enum Parsed {
    /// The packet's term (the `pcb->…` callback's result).
    Term(Eterm),
    /// The C's `-1`, rendered by the caller as `{http_error, Line}`.
    Malformed,
}

/// `packet_parse_http` (`packet_parser.c:662-862`) over one whole packet.
fn message(
    ctx: &mut BifContext<'_>,
    packet: &[u8],
    header_mode: bool,
    as_binary: bool,
) -> Result<Parsed> {
    // `packet_parser.c:669-673`: strip the trailing CRLF (a bare NL as well) —
    // but only when the CR really is the second-to-last byte.
    let end = if packet.len() >= 2 && packet[packet.len() - 2] == b'\r' {
        packet.len() - 2
    } else if !packet.is_empty() && packet[packet.len() - 1] == b'\n' {
        packet.len() - 1
    } else {
        packet.len()
    };
    let line = &packet[..end];

    if !header_mode {
        // A start-line: `HTTP/x.y …` is a status line, everything else a
        // request line.
        if line.len() >= 5 && &line[..5] == b"HTTP/" {
            status_line(ctx, line, as_binary)
        } else {
            request_line(ctx, line, as_binary)
        }
    } else if line.is_empty() {
        // `packet_parser.c:809-813`: the end of the headers.
        Ok(Parsed::Term(ctx.intern_atom("http_eoh")?))
    } else {
        header_line(ctx, line, as_binary)
    }
}

/// The `Status-Line` arm of `packet_parse_http` (`packet_parser.c:678-728`).
fn status_line(ctx: &mut BifContext<'_>, line: &[u8], as_binary: bool) -> Result<Parsed> {
    // "HTTP/" has already been matched by the caller.
    let mut i = 5;

    let start = i;
    let major = digits(line, &mut i);
    if i == start || i >= line.len() || line[i] != b'.' {
        return Ok(Parsed::Malformed);
    }
    i += 1;

    let start = i;
    let minor = digits(line, &mut i);
    if i == start {
        return Ok(Parsed::Malformed);
    }

    // At least one space or tab before the status code.
    let start = i;
    while i < line.len() && is_sp(line[i]) {
        i += 1;
    }
    if i == start {
        return Ok(Parsed::Malformed);
    }

    let status = digits(line, &mut i);

    // A space after the status code is optional only when the phrase is empty.
    let start = i;
    while i < line.len() && is_sp(line[i]) {
        i += 1;
    }
    if i == start && i < line.len() {
        return Ok(Parsed::Malformed);
    }

    let phrase = build_string(ctx, as_binary, &line[i..])?;
    let version = version_term(ctx, major, minor)?;
    let status = ctx.int(i64::from(status))?;
    let tag = ctx.intern_atom("http_response")?;
    Ok(Parsed::Term(ctx.tuple(&[tag, version, status, phrase])?))
}

/// The `Request-Line` arm of `packet_parse_http` (`packet_parser.c:729-799`).
fn request_line(ctx: &mut BifContext<'_>, line: &[u8], as_binary: bool) -> Result<Parsed> {
    // The method runs to the first tspecial byte.
    let mut i = 0;
    while i < line.len() && !is_tspecial(line[i]) {
        i += 1;
    }
    let method_len = i;
    if i == line.len() || method_len == 0 || !is_sp(line[i]) {
        return Ok(Parsed::Malformed);
    }

    while i < line.len() && is_sp(line[i]) {
        i += 1;
    }
    let uri_start = i;
    // The request target runs to the next space or tab (a tspecial byte is
    // allowed inside it).
    while i < line.len() && !is_sp(line[i]) {
        i += 1;
    }
    let uri_len = i - uri_start;
    if uri_len == 0 {
        return Ok(Parsed::Malformed);
    }
    while i < line.len() && is_sp(line[i]) {
        i += 1;
    }

    let uri = parse_uri(&line[uri_start..uri_start + uri_len]);

    // No version at all: the C's `0.9` request (`packet_parser.c:764-769`).
    let (major, minor) = if i == line.len() {
        (0, 9)
    } else {
        if line.len() - i < 8 || &line[i..i + 5] != b"HTTP/" {
            return Ok(Parsed::Malformed);
        }
        i += 5;
        let start = i;
        let major = digits(line, &mut i);
        if i == start || i >= line.len() || line[i] != b'.' {
            return Ok(Parsed::Malformed);
        }
        i += 1;
        let start = i;
        let minor = digits(line, &mut i);
        if i == start {
            return Ok(Parsed::Malformed);
        }
        (major, minor)
    };

    // The method is the table's atom when it is one of the seven, else its
    // (un-normalised) bytes.
    let method = match lookup(&METHOD_NAMES, &line[..method_len]) {
        Some((_, name)) => ctx.intern_atom(name)?,
        None => build_string(ctx, as_binary, &line[..method_len])?,
    };
    let uri = build_uri(ctx, &uri, as_binary)?;
    let version = version_term(ctx, major, minor)?;
    let tag = ctx.intern_atom("http_request")?;
    Ok(Parsed::Term(ctx.tuple(&[tag, method, uri, version])?))
}

/// The header-field arm of `packet_parse_http` (`packet_parser.c:801-860`).
fn header_line(ctx: &mut BifContext<'_>, line: &[u8], as_binary: bool) -> Result<Parsed> {
    // The name runs to the first tspecial byte, which must be the `:`.
    let mut i = 0;
    let mut name_len = 0usize;
    // The case-normalised name: the first character and every character after a
    // `-` upper-cased, the rest lower-cased. It holds at most
    // `MAX_NAME_LEN` characters; beyond that the original bytes are used.
    let mut normalised: Vec<u8> = Vec::with_capacity(MAX_NAME_LEN);
    let mut upper_next = true;
    loop {
        let byte = line[i];
        if is_tspecial(byte) {
            break;
        }
        if name_len < MAX_NAME_LEN {
            let mut c = byte;
            if upper_next {
                if c.is_ascii_lowercase() {
                    c = c.to_ascii_uppercase();
                }
                upper_next = false;
            } else if c.is_ascii_uppercase() {
                c = c.to_ascii_lowercase();
            } else if c == b'-' {
                upper_next = true;
            }
            normalised.push(c);
        }
        name_len += 1;
        i += 1;
        if i == line.len() {
            return Ok(Parsed::Malformed);
        }
    }
    if line[i] != b':' {
        return Ok(Parsed::Malformed);
    }

    // `packet_parser.c:841-849`: only a name that fits the buffer can name a
    // canonical header; a longer one is returned in its original spelling.
    let canonical = if name_len <= MAX_NAME_LEN {
        lookup(&HEADER_NAMES, &normalised)
    } else {
        None
    };
    let name_bytes: &[u8] =
        if name_len <= MAX_NAME_LEN { &normalised } else { &line[..name_len] };

    i += 1;
    // Skip the white space after the `:`.
    while i < line.len() && is_sp(line[i]) {
        i += 1;
    }

    let (bit, name) = match canonical {
        Some((index, name)) => (index + 1, ctx.intern_atom(name)?),
        None => (0, build_string(ctx, as_binary, name_bytes)?),
    };
    let oname = build_string(ctx, as_binary, &line[..name_len])?;
    let value = build_string(ctx, as_binary, &line[i..])?;
    let tag = ctx.intern_atom("http_header")?;
    let bit = ctx.int(bit as i64)?;
    Ok(Parsed::Term(ctx.tuple(&[tag, bit, name, oname, value])?))
}

/// The C's digit loop (`major`/`minor`/`status`/port accumulators).
///
/// The C accumulates into an `int` that wraps on overflow; widening to
/// `u32`-with-wrapping keeps the same low 32 bits (the ring operations are
/// `* 10` and `+ d`) and the `as i32` reinterpretation gives the same signed
/// value, so an over-long digit run matches the oracle instead of diverging.
fn digits(line: &[u8], i: &mut usize) -> i32 {
    let mut acc: u32 = 0;
    while *i < line.len() && line[*i].is_ascii_digit() {
        acc = acc.wrapping_mul(10).wrapping_add(u32::from(line[*i] - b'0'));
        *i += 1;
    }
    acc as i32
}

/// `{Major, Minor}` (`erl_bif_port.c:1305`).
fn version_term(ctx: &mut BifContext<'_>, major: i32, minor: i32) -> Result<Eterm> {
    let major = ctx.int(i64::from(major))?;
    let minor = ctx.int(i64::from(minor))?;
    ctx.tuple(&[major, minor])
}

/// The exact-name lookup `http_hash_lookup` performs (`packet_parser.c:504-518`).
///
/// The C walks the bucket selected by a rolling hash and then decides with
/// `ap->h == h && ap->len == len && strncmp(name) == 0`. The hash can only
/// *skip* an entry that the remaining tests would have rejected anyway (equal
/// lengths and equal bytes imply equal hashes), so an exact `(length, bytes)`
/// scan over the table decides identically. Returns the entry's index and name.
fn lookup<'a>(table: &[&'a str], name: &[u8]) -> Option<(usize, &'a str)> {
    table
        .iter()
        .enumerate()
        .find(|(_, entry)| entry.len() == name.len() && entry.as_bytes() == name)
        .map(|(index, entry)| (index, *entry))
}

/// `http_bld_string` (`erl_bif_port.c:1276-1292`): a byte-aligned binary for the
/// `*_bin` types, else the latin1 byte list.
fn build_string(ctx: &mut BifContext<'_>, as_binary: bool, bytes: &[u8]) -> Result<Eterm> {
    if as_binary {
        return bits::make_binary(ctx.heap(), bytes);
    }
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// The parsed request target (`PacketHttpURI` + `URI_*`).
enum Uri<'a> {
    /// `URI_STAR` — the atom `'*'`.
    Star,
    /// `URI_ABS_PATH` — `{abs_path, S}`.
    AbsPath(&'a [u8]),
    /// `URI_HTTP`/`URI_HTTPS` —
    /// `{absoluteURI, http | https, Host, Port | undefined, Path}`.
    Absolute { https: bool, host: &'a [u8], port: i32, path: &'a [u8] },
    /// `URI_STRING` — the bare string.
    Plain(&'a [u8]),
    /// `URI_SCHEME` — `{scheme, S1, S2}`.
    Scheme { scheme: &'a [u8], rest: &'a [u8] },
}

/// `http_parse_uri` (`packet_parser.c:615-652`).
fn parse_uri(uri: &[u8]) -> Uri<'_> {
    if uri.len() == 1 && uri[0] == b'*' {
        return Uri::Star;
    }
    if uri.len() <= 1 || uri[0] == b'/' {
        return Uri::AbsPath(uri);
    }
    if uri.len() >= 7 && uri[..7].eq_ignore_ascii_case(b"http://") {
        return absolute_uri(&uri[7..], false);
    }
    if uri.len() >= 8 && uri[..8].eq_ignore_ascii_case(b"https://") {
        return absolute_uri(&uri[8..], true);
    }
    match uri.iter().position(|&b| b == b':') {
        None => Uri::Plain(uri),
        Some(at) => Uri::Scheme { scheme: &uri[..at], rest: &uri[at + 1..] },
    }
}

/// `http_parse_absoluteURI` (`packet_parser.c:520-582`) over the bytes left
/// after the `scheme://` prefix.
fn absolute_uri(rest: &[u8], https: bool) -> Uri<'_> {
    let (authority, path): (&[u8], &[u8]) = match rest.iter().position(|&b| b == b'/') {
        // No path at all: the C reports `/` and the whole remainder is the host.
        None => (rest, b"/"),
        Some(at) => (&rest[..at], &rest[at..]),
    };
    let (host, port) = split_host_port(authority);
    Uri::Absolute { https, host, port, path }
}

/// The host/port split of `http_parse_absoluteURI`.
///
/// Returns the reported host and the port (`0` is the C's "undefined"). Only a
/// port whose digits run to the end of the authority and whose value is non-zero
/// is reported; the IPv6 `[host]` form drops everything after the closing
/// bracket except a `:port` tail.
fn split_host_port(authority: &[u8]) -> (&[u8], i32) {
    let Some(colon) = authority.iter().position(|&b| b == b':') else {
        // `host` (no port delimiter at all).
        return (authority, 0);
    };

    if authority[0] == b'[' {
        if let Some(close) = authority.iter().position(|&b| b == b']') {
            let host_len = close + 1;
            // `memrchr(':') == v + 1` is exactly "the closing bracket is
            // followed by the only remaining colon".
            if let Some(tail) = authority[host_len..].strip_prefix(b":") {
                if !tail.contains(&b':') {
                    if let Some(port) = whole_port(tail) {
                        return (&authority[..host_len], port);
                    }
                }
            }
            return (&authority[..host_len], 0);
        }
    }

    // `host[:port]`: the reported host stops at the first colon.
    let tail = &authority[colon + 1..];
    match whole_port(tail) {
        Some(port) => (&authority[..colon], port),
        None => (&authority[..colon], 0),
    }
}

/// The C's port loop: every byte must be a digit (`n == 0`) and the accumulated
/// value non-zero, else the port stays undefined.
fn whole_port(tail: &[u8]) -> Option<i32> {
    let mut port: u32 = 0;
    let mut i = 0;
    while i < tail.len() && tail[i].is_ascii_digit() {
        port = port.wrapping_mul(10).wrapping_add(u32::from(tail[i] - b'0'));
        i += 1;
    }
    if i == tail.len() && port != 0 {
        Some(port as i32)
    } else {
        None
    }
}

/// `http_bld_uri` (`erl_bif_port.c:1315-1356`).
fn build_uri(ctx: &mut BifContext<'_>, uri: &Uri<'_>, as_binary: bool) -> Result<Eterm> {
    match uri {
        Uri::Star => ctx.intern_atom("*"),
        Uri::AbsPath(s) => {
            let s = build_string(ctx, as_binary, s)?;
            let tag = ctx.intern_atom("abs_path")?;
            ctx.tuple(&[tag, s])
        }
        Uri::Absolute { https, host, port, path } => {
            let host = build_string(ctx, as_binary, host)?;
            let path = build_string(ctx, as_binary, path)?;
            let scheme = ctx.intern_atom(if *https { "https" } else { "http" })?;
            let port = if *port == 0 {
                ctx.intern_atom("undefined")?
            } else {
                ctx.int(i64::from(*port))?
            };
            let tag = ctx.intern_atom("absoluteURI")?;
            ctx.tuple(&[tag, scheme, host, port, path])
        }
        Uri::Plain(s) => build_string(ctx, as_binary, s),
        Uri::Scheme { scheme, rest } => {
            let s1 = build_string(ctx, as_binary, scheme)?;
            let s2 = build_string(ctx, as_binary, rest)?;
            let tag = ctx.intern_atom("scheme")?;
            ctx.tuple(&[tag, s1, s2])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_table_holds_the_c_table_in_order() {
        // The `Bit` a known header reports is its 1-based index here, so the
        // order of `packet_parser.c:68-122` is load-bearing. Spot-check the two
        // ends and every entry's uniqueness (`http_hash_insert` keys on the name).
        assert_eq!(HEADER_NAMES.len(), 52);
        assert_eq!(HEADER_NAMES[0], "Cache-Control");
        assert_eq!(HEADER_NAMES[13], "Host");
        assert_eq!(HEADER_NAMES[37], "Content-Length");
        assert_eq!(HEADER_NAMES[51], "Proxy-Connection");
        for (i, name) in HEADER_NAMES.iter().enumerate() {
            assert!(name.len() <= MAX_NAME_LEN, "{name} exceeds the name buffer");
            assert!(name.is_ascii(), "{name} is not ASCII");
            assert!(!HEADER_NAMES[i + 1..].contains(name), "{name} is duplicated");
        }
        assert_eq!(METHOD_NAMES, ["OPTIONS", "GET", "HEAD", "POST", "PUT", "DELETE", "TRACE"]);
    }

    #[test]
    fn the_tspecial_class_matches_the_c_table() {
        // `tspecial[x]` is 1 for `x <= 32` and for every byte >= 128 ...
        for byte in 0..=32u8 {
            assert!(is_tspecial(byte), "{byte} must be a tspecial");
        }
        for byte in 128..=255u8 {
            assert!(is_tspecial(byte), "{byte} must be a tspecial");
        }
        // ... 0 for the printable range except the delimiters ...
        for byte in 33..=126u8 {
            let expected = TSPECIAL_DELIMITERS.contains(&byte);
            assert_eq!(is_tspecial(byte), expected, "{byte} ({})", byte as char);
        }
        // ... and 0 for 127, which `http_init` never writes.
        assert!(!is_tspecial(127));
        // The delimiters the parse relies on.
        for byte in [b' ', b'\t', b'(', b')', b'<', b'>', b'@', b',', b';', b':', b'\\', b'"', b'/',
            b'[', b']', b'?', b'=', b'{', b'}']
        {
            assert!(is_tspecial(byte), "{} is a delimiter", byte as char);
        }
        for byte in [b'a', b'Z', b'0', b'-', b'_', b'.', b'%', b'~'] {
            assert!(!is_tspecial(byte), "{} is not a tspecial", byte as char);
        }
    }

    #[test]
    fn the_length_rule_accepts_only_a_terminated_line() {
        let opts = Options { max_plen: 0, trunc_len: 0, delimiter: b'\n' };
        // A whole line is a packet ...
        assert!(matches!(packet_length(false, b"GET / HTTP/1.1\r\nrest", &opts),
                         Length::Packet(16)));
        // ... an unterminated one needs more ...
        assert!(matches!(packet_length(false, b"GET / HTTP/1.1", &opts), Length::More));
        // ... and an empty buffer too.
        assert!(matches!(packet_length(false, b"", &opts), Length::More));
        // A bare `\n`/`\r\n` buffer is a whole packet whatever the ceiling.
        let one = Options { max_plen: 1, trunc_len: 0, delimiter: b'\n' };
        assert!(matches!(packet_length(false, b"\r\nrest", &one), Length::Invalid));
        assert!(matches!(packet_length(false, b"\nrest", &one), Length::Packet(1)));
    }

    #[test]
    fn a_header_line_extends_over_its_continuation_lines() {
        let opts = Options { max_plen: 0, trunc_len: 0, delimiter: b'\n' };
        // The continuation joins the field, so the packet is longer than the
        // first line ...
        assert!(matches!(packet_length(true, b"Host: x\r\n more\r\nz", &opts),
                         Length::Packet(16)));
        // ... but a line ending at the buffer's end may still continue.
        assert!(matches!(packet_length(true, b"Host: x\r\n", &opts), Length::More));
        assert!(matches!(packet_length(true, b"Host: x\r\nrest", &opts), Length::Packet(9)));
        // A bare CRLF is never a continuation.
        assert!(matches!(packet_length(true, b"\r\n x\r\nz", &opts), Length::Packet(2)));
    }
}
