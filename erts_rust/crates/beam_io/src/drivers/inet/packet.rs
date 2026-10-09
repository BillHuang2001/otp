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

//! The `tcp_inet` **packet-framing decoder** — a term-free, byte-level port of the C
//! `packet_parser`.
//!
//! When the Erlang side configures a socket with `{packet, N}` (the driver value of `N` is the
//! numeric code of [`PacketType`]), a `tcp_inet` port no longer delivers the raw byte stream:
//! it buffers received bytes and asks the C `packet_get_length` how many of them make up one
//! complete *frame*, then delivers that frame as a single `{tcp, Port, Data}` message. This
//! module is that decoder, ported byte for byte from the C, together with the header strip.
//!
//! Exactly two C functions are ported and nothing else:
//!
//! * **`packet_get_length`** (`erts/emulator/beam/packet_parser.c:248`) — the header/length
//!   decoder. The C returns `> 0` = the **total** frame length *including its header*, `0` =
//!   "not enough bytes to decide yet", `< 0` = "invalid format". Those are the three
//!   data-carrying variants of [`PacketLength`] ([`Complete`](PacketLength::Complete) /
//!   [`NeedMore`](PacketLength::NeedMore) / [`Invalid`](PacketLength::Invalid)). The C's
//!   `htype`, `max_plen` (`{packet_size, N}`; `0` = no limit), `trunc_len` (`0` = no limit)
//!   and `delimiter` (`{line_delimiter, C}`, default `\n`) are the four fields of
//!   [`PacketOptions`]. The C's `int* statep` is an HTTP-only protocol state that none of the
//!   types decoded here read, so this port is stateless.
//! * **`packet_get_body`** (`erts/emulator/beam/packet_parser.h:152`) — the header strip:
//!   [`PacketType::payload_range`] gives the payload byte range of a complete frame. The C
//!   advances the buffer by `1`/`2`/`4` bytes for `TCP_PB_1`/`TCP_PB_2`/`TCP_PB_4` and
//!   shortens the length by the FCGI `paddingLength` for `TCP_PB_FCGI`; every other type is
//!   returned "as is".
//!
//! # What is decoded, and what is honestly refused
//!
//! The **byte-oriented** types — whose total length is a pure function of the buffered bytes —
//! are decoded: [`Raw`](PacketType::Raw), [`Len1`](PacketType::Len1),
//! [`Len2`](PacketType::Len2), [`Len4`](PacketType::Len4), [`Asn1`](PacketType::Asn1),
//! [`Rm`](PacketType::Rm), [`Cdr`](PacketType::Cdr), [`Fcgi`](PacketType::Fcgi),
//! [`LineLf`](PacketType::LineLf) and [`Tpkt`](PacketType::Tpkt) (see
//! [`PacketType::is_supported`]).
//!
//! The **HTTP family** ([`Http`](PacketType::Http), [`HttpH`](PacketType::HttpH),
//! [`HttpBin`](PacketType::HttpBin), [`HttpHBin`](PacketType::HttpHBin)) and
//! [`SslTls`](PacketType::SslTls) are **not** decoded. Their bodies are turned into Erlang
//! terms by the C's `PacketCallbacks` (`http_request`/`http_header`/`http_eoh`/`ssl_tls`, …,
//! `packet_parser.h:90`) — that is `decode_packet`'s job and it needs a term builder this
//! term-free decoder deliberately does not have. They report
//! [`PacketLength::Unsupported`], a distinct outcome a caller maps to a typed `unsupported`
//! error ([`PacketLength::as_error`]), rather than a [`Complete`](PacketLength::Complete)
//! length that would let the driver deliver a half-parsed "frame".
//!
//! # Faithfully reproduced quirks
//!
//! Three C behaviours look like bugs if "cleaned up", so they are kept verbatim:
//!
//! * **`{packet, 0}` [`Raw`](PacketType::Raw) ignores `max_plen`.** `TCP_PB_RAW` returns the
//!   number of buffered bytes directly (`packet_parser.c:258`) without passing through the
//!   shared `remain:` check, so a `{packet_size, N}` has no effect in raw mode.
//! * **Line mode compares its two limits asymmetrically.** With no delimiter yet buffered the
//!   C tests `n > max_plen` (strictly greater) but `n >= trunc_len` (greater *or equal*); once
//!   a delimiter is found it tests `len > max_plen` and `len > trunc_len`
//!   (`packet_parser.c:298`-`:320`).
//! * **`Raw` with an empty buffer is "need more", not "complete 0".** The C's
//!   `if (n == 0) goto more;` means a raw port never delivers a zero-length frame.
//!
//! Two further decisions are documented at their call sites: the `INT_MAX` default for
//! `max_plen` (the C's `remain:`, see [`remain`]) and the FCGI payload range keeping its
//! 8-byte header (the C never advances the buffer for FCGI, see
//! [`PacketType::payload_range`]).
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/packet_parser.{c,h}`.

use std::ops::Range;

use beam_common::{BeamError, Result};

use crate::error;

/// The delimiter `{packet, line}` uses when `{line_delimiter, C}` is not set — the C's
/// `desc->delimiter = '\n'` (`inet_drv.c:10934`).
pub const DEFAULT_DELIMITER: u8 = b'\n';

/// The one-byte length header of `{packet, 1}` (`TCP_PB_1`).
const HEADER_1: usize = 1;

/// The two-byte length header of `{packet, 2}` (`TCP_PB_2`).
const HEADER_2: usize = 2;

/// The four-byte length header of `{packet, 4}` and `{packet, rm}` (`TCP_PB_4`/`TCP_PB_RM`).
const HEADER_4: usize = 4;

/// `sizeof(struct cdr_head)` (`packet_parser.c:217`).
const CDR_HEADER: usize = 12;

/// `sizeof(struct fcgi_head)` (`packet_parser.h:134`).
const FCGI_HEADER: usize = 8;

/// `sizeof(struct tpkt_head)` (`packet_parser.c:229`).
const TPKT_HEADER: usize = 4;

/// `FCGI_VERSION_1` (`packet_parser.h:133`).
const FCGI_VERSION_1: u8 = 1;

/// `TPKT_VRSN` (`packet_parser.c:227`).
const TPKT_VRSN: u8 = 3;

/// `CDR_MAGIC`, the four bytes a GIOP/IIOP header starts with (`packet_parser.c:215`).
const CDR_MAGIC: [u8; 4] = *b"GIOP";

/// `INT_MAX` — the C's `max_plen` ceiling (`remain:`, `packet_parser.c:488`) and the range of
/// `packet_get_length`'s `int` return value.
const INT_MAX: usize = i32::MAX as usize;

/// A `{packet, N}` packet type — the C's `enum PacketParseType`
/// (`erts/emulator/beam/packet_parser.h:33`).
///
/// The `u8` discriminant **is** the value the Erlang `{packet, N}` option encodes: the driver
/// stores the option integer straight into `desc->inet.htype` (`inet_set_opts`,
/// `inet_drv.c:7023`) and hands it to `packet_get_length`. [`code`](PacketType::code) and
/// [`from_code`](PacketType::from_code) are therefore the wire mapping
/// `{packet, N}` ⇄ `TCP_PB_*`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum PacketType {
    /// `{packet, 0}` / `{packet, raw}` (C `TCP_PB_RAW`) — no framing at all: every buffered
    /// byte is one frame (and `max_plen` is ignored).
    Raw = 0,

    /// `{packet, 1}` (C `TCP_PB_1`) — a one-byte length header.
    Len1 = 1,

    /// `{packet, 2}` (C `TCP_PB_2`) — a two-byte big-endian length header.
    Len2 = 2,

    /// `{packet, 4}` (C `TCP_PB_4`) — a four-byte big-endian length header.
    Len4 = 3,

    /// `{packet, asn1}` (C `TCP_PB_ASN1`) — an ASN.1 BER tag plus a definite-length field,
    /// both in short or long form.
    Asn1 = 4,

    /// `{packet, rm}` / `{packet, sunrm}` (C `TCP_PB_RM`) — a four-byte length header whose
    /// top bit flags the end of a record and is masked off the length.
    Rm = 5,

    /// `{packet, cdr}` (C `TCP_PB_CDR`) — a 12-byte GIOP/IIOP header (`"GIOP"` magic, version,
    /// flags and a possibly little-endian message size).
    Cdr = 6,

    /// `{packet, fcgi}` (C `TCP_PB_FCGI`) — an eight-byte FastCGI record header
    /// (version, type, request id, content length, padding length).
    Fcgi = 7,

    /// `{packet, line}` (C `TCP_PB_LINE_LF`) — all bytes up to and including the delimiter.
    LineLf = 8,

    /// `{packet, tpkt}` (C `TCP_PB_TPKT`) — a four-byte TPKT header whose own length field
    /// counts the whole frame.
    Tpkt = 9,

    /// `{packet, http}` (C `TCP_PB_HTTP`) — **unsupported** here: its body needs the C's HTTP
    /// term-building callbacks.
    Http = 10,

    /// `{packet, httph}` (C `TCP_PB_HTTPH`) — **unsupported** here: its body needs the C's
    /// HTTP term-building callbacks.
    HttpH = 11,

    /// `{packet, ssl_tls}` (C `TCP_PB_SSL_TLS`) — **unsupported** here: its body needs the C's
    /// TLS term-building callback.
    SslTls = 12,

    /// `{packet, http_bin}` (C `TCP_PB_HTTP_BIN`) — **unsupported** here: its body needs the
    /// C's HTTP term-building callbacks.
    HttpBin = 13,

    /// `{packet, httph_bin}` (C `TCP_PB_HTTPH_BIN`) — **unsupported** here: its body needs the
    /// C's HTTP term-building callbacks.
    HttpHBin = 14,
}

impl PacketType {
    /// Every variant, in numeric (= `{packet, N}`) order — the C's `enum PacketParseType`.
    pub const ALL: [PacketType; 15] = [
        PacketType::Raw,
        PacketType::Len1,
        PacketType::Len2,
        PacketType::Len4,
        PacketType::Asn1,
        PacketType::Rm,
        PacketType::Cdr,
        PacketType::Fcgi,
        PacketType::LineLf,
        PacketType::Tpkt,
        PacketType::Http,
        PacketType::HttpH,
        PacketType::SslTls,
        PacketType::HttpBin,
        PacketType::HttpHBin,
    ];

    /// The numeric code the `{packet, N}` option carries — the C's enum value, which the driver
    /// stores verbatim in `desc->inet.htype`.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The variant for a `{packet, N}` numeric code, or `None` when no type uses that code
    /// (the C's `default: return -1` arm, `packet_parser.c:478`).
    pub const fn from_code(code: u8) -> Option<PacketType> {
        Some(match code {
            0 => PacketType::Raw,
            1 => PacketType::Len1,
            2 => PacketType::Len2,
            3 => PacketType::Len4,
            4 => PacketType::Asn1,
            5 => PacketType::Rm,
            6 => PacketType::Cdr,
            7 => PacketType::Fcgi,
            8 => PacketType::LineLf,
            9 => PacketType::Tpkt,
            10 => PacketType::Http,
            11 => PacketType::HttpH,
            12 => PacketType::SslTls,
            13 => PacketType::HttpBin,
            14 => PacketType::HttpHBin,
            _ => return None,
        })
    }

    /// Whether this decoder extracts a frame length for the type.
    ///
    /// False for the HTTP family and `ssl_tls`, whose body parsing needs the C's term-building
    /// callbacks; [`PacketOptions::decode`] answers [`PacketLength::Unsupported`] for them.
    pub const fn is_supported(self) -> bool {
        !matches!(
            self,
            PacketType::Http
                | PacketType::HttpH
                | PacketType::SslTls
                | PacketType::HttpBin
                | PacketType::HttpHBin
        )
    }

    /// The payload byte range of a complete frame — the C's `packet_get_body`
    /// (`packet_parser.h:152`).
    ///
    /// `frame` is one whole frame as sized by [`PacketOptions::decode`]. The header is stripped
    /// for [`Len1`](PacketType::Len1)/[`Len2`](PacketType::Len2)/[`Len4`](PacketType::Len4)
    /// (by `1`/`2`/`4` leading bytes) and for [`Fcgi`](PacketType::Fcgi) (by the header's
    /// `paddingLength`, read from the frame's byte 6, whose padding sits at the *end*); every
    /// other type — including the unsupported ones — is returned "as is" (`0..frame.len()`),
    /// exactly as the C's `default:` arm does.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] when the frame is shorter than the type's header, or
    /// when an FCGI frame's `paddingLength` is larger than the frame itself. (A frame produced
    /// by this decoder can never trip either check — an FCGI frame is at least
    /// `8 + paddingLength` bytes — so this only rejects a caller-supplied frame the C would
    /// read out of bounds.)
    pub fn payload_range(self, frame: &[u8]) -> Result<Range<usize>> {
        let len = frame.len();
        match self {
            PacketType::Len1 => strip_header(len, HEADER_1),
            PacketType::Len2 => strip_header(len, HEADER_2),
            PacketType::Len4 => strip_header(len, HEADER_4),
            PacketType::Fcgi => {
                if len < FCGI_HEADER {
                    return Err(error::badarg());
                }
                let padding = frame[6] as usize;
                if padding > len {
                    return Err(error::badarg());
                }
                Ok(0..len - padding)
            }
            _ => Ok(0..len),
        }
    }
}

/// The `hlen`-byte payload range of a `len`-byte frame (`hlen..len`), or `badarg` when the
/// frame is shorter than its header — the C's `packet_get_body` "skip the header" arms.
fn strip_header(len: usize, hlen: usize) -> Result<Range<usize>> {
    if len < hlen {
        return Err(error::badarg());
    }
    Ok(hlen..len)
}

/// The outcome of decoding one frame header from the bytes read so far — the result of the C
/// `packet_get_length` (`> 0` / `0` / `< 0`), plus an explicit "not implemented" outcome.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PacketLength {
    /// The frame's **total** byte length is known, **including its header** (the C's `> 0`).
    ///
    /// Note what the C means by "length known": the value is returned as soon as the header
    /// suffices, even when fewer than that many bytes have been buffered yet — the driver then
    /// reads the remainder. For [`Raw`](PacketType::Raw) and a delimited
    /// [`LineLf`](PacketType::LineLf) frame the value always equals the buffered byte count.
    Complete(usize),

    /// Not enough bytes are buffered to decide the length; read more and call again (the C's
    /// `0`).
    NeedMore,

    /// The buffered bytes cannot be a valid frame, or the frame would exceed `max_plen` (the
    /// C's `< 0`).
    Invalid,

    /// No length extraction is implemented for the packet type — the HTTP family and `ssl_tls`
    /// (see the module docs). Kept distinct from [`Invalid`](PacketLength::Invalid) so a caller
    /// raises a typed `unsupported` rather than a `badarg`.
    Unsupported,
}

impl PacketLength {
    /// Whether the frame length is known ([`Complete`](PacketLength::Complete)).
    pub const fn is_complete(self) -> bool {
        matches!(self, PacketLength::Complete(_))
    }

    /// The total frame length when it is known ([`Complete`](PacketLength::Complete)), else
    /// `None`.
    pub const fn complete(self) -> Option<usize> {
        match self {
            PacketLength::Complete(len) => Some(len),
            _ => None,
        }
    }

    /// The typed error to raise for a failed decode, or `None` for a decoded result.
    ///
    /// [`Invalid`](PacketLength::Invalid) maps to the `badarg` argument fault (the C's `< 0`);
    /// [`Unsupported`](PacketLength::Unsupported) maps to the `notsup` raise, so a caller can
    /// tell "this is not a valid frame" from "this runtime does not decode this type".
    pub fn as_error(self) -> Option<BeamError> {
        match self {
            PacketLength::Complete(_) | PacketLength::NeedMore => None,
            PacketLength::Invalid => Some(error::badarg()),
            PacketLength::Unsupported => Some(error::notsup()),
        }
    }
}

/// The framing parameters of a `{packet, N}` / `{packet_size, N}` / `{line_delimiter, C}`
/// socket — the arguments of the C's `packet_get_length` (`packet_parser.c:248`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PacketOptions {
    /// The packet type (`{packet, N}`; the C's `htype`).
    pub packet: PacketType,

    /// The line delimiter (`{line_delimiter, C}`; the C's `delimiter`), used only by
    /// [`PacketType::LineLf`]. Defaults to [`DEFAULT_DELIMITER`].
    pub delimiter: u8,

    /// The maximum frame length (`{packet_size, N}`; the C's `max_plen`). `0` means "no limit":
    /// the C substitutes `INT_MAX - hlen` (`remain:`, `packet_parser.c:488`). It is compared
    /// against the **payload** length (the C's `plen`), never the total frame length — the
    /// oracle documents `{packet_size, Size}` as "the maximum allowed size of the packet
    /// *body*". Ignored by [`PacketType::Raw`].
    pub max_plen: usize,

    /// The truncation length (the C's `trunc_len`, which the driver sets to its input buffer
    /// size, `inet_drv.c:12976`). `0` means "no limit". In line mode a buffered line at least
    /// this long is delivered as exactly `trunc_len` bytes without waiting for a delimiter.
    pub trunc_len: usize,
}

impl Default for PacketOptions {
    /// The C driver's defaults: `{packet, raw}`, delimiter `\n`, no packet-size and no
    /// truncation limit (`inet_drv.c:10923`, `:10934`).
    fn default() -> Self {
        PacketOptions {
            packet: PacketType::Raw,
            delimiter: DEFAULT_DELIMITER,
            max_plen: 0,
            trunc_len: 0,
        }
    }
}

impl PacketOptions {
    /// The framing options of `packet` with the C driver's other defaults.
    pub const fn new(packet: PacketType) -> Self {
        PacketOptions {
            packet,
            delimiter: DEFAULT_DELIMITER,
            max_plen: 0,
            trunc_len: 0,
        }
    }

    /// The same options with the line delimiter set (`{line_delimiter, C}`).
    pub const fn with_delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = delimiter;
        self
    }

    /// The same options with the maximum frame length set (`{packet_size, N}`).
    pub const fn with_max_plen(mut self, max_plen: usize) -> Self {
        self.max_plen = max_plen;
        self
    }

    /// The same options with the line truncation length set (the C's `trunc_len`).
    pub const fn with_trunc_len(mut self, trunc_len: usize) -> Self {
        self.trunc_len = trunc_len;
        self
    }

    /// Decode the frame header from the bytes read so far — the C's `packet_get_length`.
    ///
    /// `data` is everything buffered for the current frame; the return value says whether the
    /// frame's total length is known ([`PacketLength::Complete`]), whether more bytes are
    /// needed ([`PacketLength::NeedMore`]) or whether the bytes are not a valid frame
    /// ([`PacketLength::Invalid`]). An unsupported type always answers
    /// [`PacketLength::Unsupported`], whatever `data` holds.
    pub fn decode(&self, data: &[u8]) -> PacketLength {
        match self.packet {
            PacketType::Raw => raw(data),
            PacketType::Len1 => prefixed(data, HEADER_1, self.max_plen),
            PacketType::Len2 => prefixed(data, HEADER_2, self.max_plen),
            PacketType::Len4 => prefixed(data, HEADER_4, self.max_plen),
            PacketType::Asn1 => asn1(data, self.max_plen),
            PacketType::Rm => rm(data, self.max_plen),
            PacketType::Cdr => cdr(data, self.max_plen),
            PacketType::Fcgi => fcgi(data, self.max_plen),
            PacketType::LineLf => line(data, self.delimiter, self.max_plen, self.trunc_len),
            PacketType::Tpkt => tpkt(data, self.max_plen),
            PacketType::Http
            | PacketType::HttpH
            | PacketType::HttpBin
            | PacketType::HttpHBin
            | PacketType::SslTls => PacketLength::Unsupported,
        }
    }
}

// ---------------------------------------------------------------------------
// The per-type decoders (`packet_get_length`, `packet_parser.c:257-481`)
// ---------------------------------------------------------------------------

/// The big-endian `len`-byte value at `data[at..]` — the C's `get_int8`/`get_int16`/`get_int24`/
/// `get_int32` (`sys.h:1274`-`:1318`). The caller guarantees `data[at..at + len]` is in bounds.
fn be_n(data: &[u8], at: usize, len: usize) -> usize {
    data[at..at + len]
        .iter()
        .fold(0usize, |acc, &byte| (acc << 8) | byte as usize)
}

/// The C's `remain:` tail (`packet_parser.c:486`) — the shared `max_plen` check and the
/// `hlen + plen` total.
fn remain(hlen: usize, plen: usize, max_plen: usize) -> PacketLength {
    // C: if (max_plen == 0) max_plen = INT_MAX - hlen; `saturating_sub` keeps that exact value
    // for every header this decoder has (all far below INT_MAX) without a debug underflow.
    let limit = if max_plen == 0 {
        INT_MAX.saturating_sub(hlen)
    } else {
        max_plen
    };
    if plen > limit {
        return PacketLength::Invalid;
    }
    // The C returns an `int`; a total that cannot fit is a malformed frame (and, on the wrapped
    // paths like a short TPKT length, exactly the case the `max_plen` check already rejects).
    match hlen.checked_add(plen) {
        Some(total) if total <= INT_MAX => PacketLength::Complete(total),
        _ => PacketLength::Invalid,
    }
}

/// `TCP_PB_RAW`: every buffered byte is one frame. `max_plen` is deliberately not consulted
/// (`packet_parser.c:258`).
fn raw(data: &[u8]) -> PacketLength {
    if data.is_empty() {
        PacketLength::NeedMore
    } else {
        PacketLength::Complete(data.len())
    }
}

/// `TCP_PB_1`/`TCP_PB_2`/`TCP_PB_4`: a big-endian length in an `hlen`-byte header
/// (`packet_parser.c:265`-`:284`).
fn prefixed(data: &[u8], hlen: usize, max_plen: usize) -> PacketLength {
    if data.len() < hlen {
        return PacketLength::NeedMore;
    }
    remain(hlen, be_n(data, 0, hlen), max_plen)
}

/// `TCP_PB_RM`: a four-byte big-endian length whose top bit signals the end of a record and is
/// masked off (`packet_parser.c:286`).
fn rm(data: &[u8], max_plen: usize) -> PacketLength {
    if data.len() < HEADER_4 {
        return PacketLength::NeedMore;
    }
    remain(HEADER_4, be_n(data, 0, HEADER_4) & 0x7fff_ffff, max_plen)
}

/// `TCP_PB_ASN1`: an ASN.1 BER tag (short `0x1f`-free, or long form) followed by a
/// definite-length field (short form = low 7 bits, long form = the low 7 bits count the
/// following length bytes, at most four) (`packet_parser.c:324`-`:363`).
fn asn1(data: &[u8], max_plen: usize) -> PacketLength {
    let n = data.len();
    if n < 2 {
        return PacketLength::NeedMore;
    }
    // `i` is the C's `tptr - ptr`; `nn` is the number of bytes from `i` on (the C's `nn`). The
    // invariant `nn == n - i` is maintained at every step below.
    let mut i = 1usize;
    let mut nn = n - 1;
    if data[0] & 0x1f == 0x1f {
        // Long tag form: skip the tag's continuation bytes (high bit set).
        while nn != 0 && data[i] & 0x80 == 0x80 {
            i += 1;
            nn -= 1;
        }
        if nn < 2 {
            return PacketLength::NeedMore;
        }
        i += 1;
        nn -= 1;
    }
    // `i` now indexes the first byte of the length field.
    let length = (data[i] & 0x7f) as usize;
    let plen;
    if data[i] & 0x80 == 0x80 {
        // Long length form: `length` further bytes hold the value.
        i += 1;
        nn -= 1;
        if nn < length {
            return PacketLength::NeedMore;
        }
        plen = match length {
            0 => 0,
            1 => be_n(data, i, 1),
            2 => be_n(data, i, 2),
            3 => be_n(data, i, 3),
            4 => be_n(data, i, 4),
            // The C's `default: goto error` — a length field wider than four bytes.
            _ => return PacketLength::Invalid,
        };
        i += length;
    } else {
        // Short length form: the low seven bits are the length.
        plen = length;
        i += 1;
    }
    remain(i, plen, max_plen)
}

/// `TCP_PB_CDR`: a 12-byte GIOP header (`"GIOP"`, major, minor, flags, message type, and a
/// four-byte message size whose byte order is selected by bit 0 of the flags)
/// (`packet_parser.c:365`-`:377`).
fn cdr(data: &[u8], max_plen: usize) -> PacketLength {
    if data.len() < CDR_HEADER {
        return PacketLength::NeedMore;
    }
    if data[..4] != CDR_MAGIC {
        return PacketLength::Invalid;
    }
    // `flags` is byte 6; `message_size` is bytes 8..12. `get_little_int32` reads it
    // little-endian, `get_int32` big-endian (`sys.h:1274`-`:1282`).
    let size = [data[8], data[9], data[10], data[11]];
    let plen = if data[6] & 0x01 == 0x01 {
        u32::from_le_bytes(size) as usize
    } else {
        u32::from_be_bytes(size) as usize
    };
    remain(CDR_HEADER, plen, max_plen)
}

/// `TCP_PB_FCGI`: an eight-byte FastCGI record header; the frame is the header plus the
/// sixteen-bit content length plus the padding length (`packet_parser.c:379`-`:389`).
fn fcgi(data: &[u8], max_plen: usize) -> PacketLength {
    if data.len() < FCGI_HEADER {
        return PacketLength::NeedMore;
    }
    if data[0] != FCGI_VERSION_1 {
        return PacketLength::Invalid;
    }
    let content = ((data[4] as usize) << 8) | data[5] as usize;
    let padding = data[6] as usize;
    remain(FCGI_HEADER, content + padding, max_plen)
}

/// `TCP_PB_LINE_LF`: all bytes up to and including the delimiter, subject to the two
/// asymmetrically-compared limits (`packet_parser.c:295`-`:322`, see the module docs).
fn line(data: &[u8], delimiter: u8, max_plen: usize, trunc_len: usize) -> PacketLength {
    match data.iter().position(|&byte| byte == delimiter) {
        None => {
            if max_plen != 0 && data.len() > max_plen {
                PacketLength::Invalid
            } else if trunc_len != 0 && data.len() >= trunc_len {
                PacketLength::Complete(trunc_len)
            } else {
                PacketLength::NeedMore
            }
        }
        Some(pos) => {
            let len = pos + 1; // including the delimiter
            if max_plen != 0 && len > max_plen {
                PacketLength::Invalid
            } else if trunc_len != 0 && len > trunc_len {
                PacketLength::Complete(trunc_len)
            } else {
                PacketLength::Complete(len)
            }
        }
    }
}

/// `TCP_PB_TPKT`: a four-byte header whose own big-endian length counts the whole frame (the C
/// subtracts the header) (`packet_parser.c:451`-`:462`).
fn tpkt(data: &[u8], max_plen: usize) -> PacketLength {
    if data.len() < TPKT_HEADER {
        return PacketLength::NeedMore;
    }
    if data[0] != TPKT_VRSN {
        return PacketLength::Invalid;
    }
    // The C computes `get_int16(packet_length) - hlen` in 32-bit unsigned arithmetic, so a
    // `packet_length` smaller than the header wraps to a huge value that the `max_plen` check
    // then rejects; the same wrap is modelled here (as it lands on the same `Invalid`).
    let packet_length = be_n(data, 2, 2) as i32;
    let plen = packet_length.wrapping_sub(TPKT_HEADER as i32) as u32 as usize;
    remain(TPKT_HEADER, plen, max_plen)
}

#[cfg(test)]
mod tests {
    use super::{PacketLength, PacketOptions, PacketType, DEFAULT_DELIMITER};
    use beam_common::BeamError;

    /// Decode `data` under `packet` with the C driver's other defaults.
    fn decode(packet: PacketType, data: &[u8]) -> PacketLength {
        PacketOptions::new(packet).decode(data)
    }

    /// A 12-byte `GIOP` header with the given `flags` byte and `message_size` bytes.
    fn cdr_frame(flags: u8, message_size: [u8; 4]) -> [u8; 12] {
        let mut frame = *b"GIOP\x01\x02\x00\x00\x00\x00\x00\x00";
        frame[6] = flags;
        frame[8..12].copy_from_slice(&message_size);
        frame
    }

    /// An eight-byte FastCGI header with the given version, content length and padding length.
    fn fcgi_frame(version: u8, content: [u8; 2], padding: u8) -> [u8; 8] {
        [version, 1, 0, 0, content[0], content[1], padding, 0]
    }

    // --- PacketType identity -------------------------------------------------

    #[test]
    fn every_type_round_trips_through_its_numeric_code() {
        for (index, &packet) in PacketType::ALL.iter().enumerate() {
            assert_eq!(packet.code() as usize, index, "{packet:?}");
            assert_eq!(PacketType::from_code(packet.code()), Some(packet));
        }
    }

    #[test]
    fn a_code_with_no_type_is_none() {
        assert_eq!(PacketType::from_code(15), None);
        assert_eq!(PacketType::from_code(255), None);
    }

    #[test]
    fn only_the_http_family_and_ssl_tls_are_unsupported() {
        let unsupported: Vec<PacketType> = PacketType::ALL
            .into_iter()
            .filter(|packet| !packet.is_supported())
            .collect();
        assert_eq!(
            unsupported,
            vec![
                PacketType::Http,
                PacketType::HttpH,
                PacketType::SslTls,
                PacketType::HttpBin,
                PacketType::HttpHBin,
            ]
        );
    }

    #[test]
    fn default_options_are_raw_lines_with_no_limits() {
        let options = PacketOptions::default();
        assert_eq!(options.packet, PacketType::Raw);
        assert_eq!(options.delimiter, DEFAULT_DELIMITER);
        assert_eq!(DEFAULT_DELIMITER, b'\n');
        assert_eq!(options.max_plen, 0);
        assert_eq!(options.trunc_len, 0);
    }

    // --- Raw -----------------------------------------------------------------

    #[test]
    fn raw_delivers_every_buffered_byte() {
        assert_eq!(decode(PacketType::Raw, b"abc"), PacketLength::Complete(3));
        assert_eq!(decode(PacketType::Raw, b"\0"), PacketLength::Complete(1));
    }

    #[test]
    fn raw_with_no_bytes_needs_more() {
        assert_eq!(decode(PacketType::Raw, b""), PacketLength::NeedMore);
    }

    #[test]
    fn raw_ignores_max_plen() {
        let options = PacketOptions::new(PacketType::Raw).with_max_plen(1);
        assert_eq!(options.decode(b"abcde"), PacketLength::Complete(5));
    }

    // --- 1/2/4-byte big-endian length prefixes -------------------------------

    #[test]
    fn len1_has_a_one_byte_header() {
        assert_eq!(
            decode(PacketType::Len1, &[0x02, b'a', b'b']),
            PacketLength::Complete(3)
        );
    }

    #[test]
    fn len2_has_a_two_byte_header() {
        assert_eq!(
            decode(PacketType::Len2, &[0x00, 0x03, b'a', b'b', b'c']),
            PacketLength::Complete(5)
        );
    }

    #[test]
    fn len4_has_a_four_byte_header() {
        assert_eq!(
            decode(PacketType::Len4, &[0, 0, 0, 2, b'a', b'b']),
            PacketLength::Complete(6)
        );
    }

    #[test]
    fn a_prefixed_length_is_known_from_the_header_alone() {
        // Only the two header bytes are buffered but the frame length is already known — the
        // C's `tlen > 0` even when `tlen > n` (the driver reads the remainder).
        let header_only = [0x00, 0x0A];
        assert_eq!(
            decode(PacketType::Len2, &header_only),
            PacketLength::Complete(12)
        );
    }

    #[test]
    fn a_frame_split_across_two_chunks_is_need_more_then_complete() {
        let first = [0x00u8, 0x03];
        // The header alone already fixes the length; what "need more" means here is the
        // one-byte-only prefix variant below.
        assert_eq!(decode(PacketType::Len2, &first), PacketLength::Complete(5));

        let mut chunk = Vec::from(first);
        chunk.extend_from_slice(b"xy");
        assert_eq!(decode(PacketType::Len2, &chunk), PacketLength::Complete(5));
    }

    #[test]
    fn a_len1_length_needs_its_single_header_byte() {
        assert_eq!(decode(PacketType::Len1, b""), PacketLength::NeedMore);
        assert_eq!(decode(PacketType::Len1, &[0x05]), PacketLength::Complete(6));
    }

    #[test]
    fn a_len4_length_needs_all_four_header_bytes() {
        for len in 0..4 {
            let data = vec![0u8; len];
            assert_eq!(decode(PacketType::Len4, &data), PacketLength::NeedMore);
        }
        assert_eq!(
            decode(PacketType::Len4, &[0, 0, 0, 1]),
            PacketLength::Complete(5)
        );
    }

    #[test]
    fn a_header_only_buffer_is_complete_with_an_empty_payload() {
        assert_eq!(
            decode(PacketType::Len2, &[0x00, 0x00]),
            PacketLength::Complete(2)
        );
        assert_eq!(
            PacketType::Len2.payload_range(&[0x00, 0x00]).unwrap(),
            2..2
        );
    }

    #[test]
    fn a_zero_max_plen_means_the_int_max_ceiling_not_infinity() {
        // With `max_plen == 0` the C substitutes `INT_MAX - hlen`, so a claimed length that
        // cannot fit an `int` is still rejected...
        assert_eq!(
            decode(PacketType::Len4, &[0xff, 0xff, 0xff, 0xff]),
            PacketLength::Invalid
        );
        // ... while every plausible length is accepted.
        assert_eq!(
            decode(PacketType::Len2, &[0x7f, 0xff]),
            PacketLength::Complete(2 + 0x7fff)
        );
        // The largest payload the ceiling admits, landing exactly on `INT_MAX` in total.
        assert_eq!(
            decode(PacketType::Len4, &[0x7f, 0xff, 0xff, 0xfb]),
            PacketLength::Complete(i32::MAX as usize)
        );
    }

    #[test]
    fn a_prefixed_frame_over_max_plen_is_invalid() {
        // `max_plen` bounds the *payload* (`plen`), not the total frame.
        let options = PacketOptions::new(PacketType::Len2).with_max_plen(4);
        assert_eq!(options.decode(&[0x00, 0x05]), PacketLength::Invalid);

        let len1 = PacketOptions::new(PacketType::Len1).with_max_plen(4);
        assert_eq!(len1.decode(&[0x05]), PacketLength::Invalid);

        let len4 = PacketOptions::new(PacketType::Len4).with_max_plen(6);
        assert_eq!(len4.decode(&[0, 0, 0, 7]), PacketLength::Invalid);
    }

    #[test]
    fn a_prefixed_payload_exactly_at_max_plen_is_valid() {
        let options = PacketOptions::new(PacketType::Len2).with_max_plen(5);
        // plen = 5 is not "greater than" max_plen = 5; the total is then 2 + 5.
        assert_eq!(options.decode(&[0x00, 0x05]), PacketLength::Complete(7));
    }

    // --- RM ------------------------------------------------------------------

    #[test]
    fn rm_masks_the_end_of_record_bit() {
        assert_eq!(
            decode(PacketType::Rm, &[0x80, 0x00, 0x00, 0x03]),
            PacketLength::Complete(7)
        );
        assert_eq!(
            decode(PacketType::Rm, &[0x00, 0x00, 0x00, 0x03]),
            PacketLength::Complete(7)
        );
    }

    #[test]
    fn rm_needs_its_four_header_bytes() {
        assert_eq!(decode(PacketType::Rm, &[0, 0, 0]), PacketLength::NeedMore);
    }

    #[test]
    fn rm_over_max_plen_is_invalid() {
        let options = PacketOptions::new(PacketType::Rm).with_max_plen(2);
        assert_eq!(options.decode(&[0, 0, 0, 3]), PacketLength::Invalid);
    }

    // --- ASN1 ----------------------------------------------------------------

    #[test]
    fn asn1_short_form_tag_and_length() {
        // 0x04 = OCTET STRING, 0x03 = three content bytes.
        assert_eq!(
            decode(PacketType::Asn1, &[0x04, 0x03, b'a', b'b', b'c']),
            PacketLength::Complete(5)
        );
    }

    #[test]
    fn asn1_short_form_length_is_known_before_the_content_arrives() {
        assert_eq!(
            decode(PacketType::Asn1, &[0x04, 0x03]),
            PacketLength::Complete(5)
        );
    }

    #[test]
    fn asn1_long_form_length() {
        // 0x30 = SEQUENCE, 0x82 = length in the next two bytes (0x0100).
        assert_eq!(
            decode(PacketType::Asn1, &[0x30, 0x82, 0x01, 0x00]),
            PacketLength::Complete(4 + 0x100)
        );
    }

    #[test]
    fn asn1_three_byte_long_length() {
        assert_eq!(
            decode(PacketType::Asn1, &[0x30, 0x83, 0x00, 0x00, 0x02]),
            PacketLength::Complete(5 + 2)
        );
    }

    #[test]
    fn asn1_zero_long_length_is_a_complete_two_byte_header() {
        // A long-length field of width 0: the header is the tag plus the 0x80 byte, and the
        // payload is empty.
        assert_eq!(
            decode(PacketType::Asn1, &[0x30, 0x80]),
            PacketLength::Complete(2)
        );
    }

    #[test]
    fn asn1_long_tag_form() {
        // Tag 0x3F (low bits all set) with a two-byte high-tag-number continuation
        // (0x81 0x7F), then a one-byte length of 3: header 4, payload 3.
        assert_eq!(
            decode(PacketType::Asn1, &[0x3f, 0x81, 0x7f, 0x03]),
            PacketLength::Complete(7)
        );
    }

    #[test]
    fn asn1_long_tag_needs_two_more_bytes() {
        assert_eq!(
            decode(PacketType::Asn1, &[0x3f, 0x81]),
            PacketLength::NeedMore
        );
    }

    #[test]
    fn asn1_needs_at_least_two_bytes() {
        assert_eq!(decode(PacketType::Asn1, b""), PacketLength::NeedMore);
        assert_eq!(decode(PacketType::Asn1, &[0x30]), PacketLength::NeedMore);
    }

    #[test]
    fn asn1_long_length_needs_more_bytes() {
        // 0x82 promises two length bytes but only one is buffered.
        assert_eq!(
            decode(PacketType::Asn1, &[0x30, 0x82, 0x01]),
            PacketLength::NeedMore
        );
    }

    #[test]
    fn asn1_a_length_field_wider_than_four_bytes_is_invalid() {
        assert_eq!(
            decode(PacketType::Asn1, &[0x30, 0x85, 0, 0, 0, 0, 0]),
            PacketLength::Invalid
        );
    }

    // --- CDR -----------------------------------------------------------------

    #[test]
    fn cdr_big_endian_message_size() {
        let frame = cdr_frame(0x00, [0, 0, 0, 5]);
        assert_eq!(decode(PacketType::Cdr, &frame), PacketLength::Complete(17));
    }

    #[test]
    fn cdr_little_endian_message_size() {
        let frame = cdr_frame(0x01, [5, 0, 0, 0]);
        assert_eq!(decode(PacketType::Cdr, &frame), PacketLength::Complete(17));
    }

    #[test]
    fn cdr_bad_magic_is_invalid() {
        let frame = cdr_frame(0x00, [0, 0, 0, 5]);
        let mut bad = frame;
        bad[0] = b'J';
        assert_eq!(decode(PacketType::Cdr, &bad), PacketLength::Invalid);
    }

    #[test]
    fn cdr_needs_the_twelve_byte_header() {
        let frame = cdr_frame(0x00, [0, 0, 0, 5]);
        assert_eq!(decode(PacketType::Cdr, &frame[..11]), PacketLength::NeedMore);
    }

    #[test]
    fn cdr_over_max_plen_is_invalid() {
        let frame = cdr_frame(0x00, [0, 0, 0, 5]);
        let options = PacketOptions::new(PacketType::Cdr).with_max_plen(4);
        assert_eq!(options.decode(&frame), PacketLength::Invalid);
    }

    // --- FCGI ----------------------------------------------------------------

    #[test]
    fn fcgi_frame_is_header_plus_content_plus_padding() {
        let frame = fcgi_frame(1, [0x00, 0x03], 0x02);
        assert_eq!(decode(PacketType::Fcgi, &frame), PacketLength::Complete(13));
    }

    #[test]
    fn fcgi_bad_version_is_invalid() {
        let frame = fcgi_frame(2, [0x00, 0x03], 0x02);
        assert_eq!(decode(PacketType::Fcgi, &frame), PacketLength::Invalid);
    }

    #[test]
    fn fcgi_needs_the_eight_byte_header() {
        let frame = fcgi_frame(1, [0x00, 0x03], 0x02);
        assert_eq!(decode(PacketType::Fcgi, &frame[..7]), PacketLength::NeedMore);
    }

    // --- TPKT ----------------------------------------------------------------

    #[test]
    fn tpkt_length_field_counts_the_whole_frame() {
        assert_eq!(
            decode(PacketType::Tpkt, &[3, 0, 0x00, 0x0b]),
            PacketLength::Complete(11)
        );
    }

    #[test]
    fn tpkt_bad_version_is_invalid() {
        assert_eq!(
            decode(PacketType::Tpkt, &[4, 0, 0x00, 0x0b]),
            PacketLength::Invalid
        );
    }

    #[test]
    fn tpkt_packet_length_smaller_than_the_header_is_invalid() {
        assert_eq!(
            decode(PacketType::Tpkt, &[3, 0, 0x00, 0x00]),
            PacketLength::Invalid
        );
    }

    #[test]
    fn tpkt_needs_the_four_byte_header() {
        assert_eq!(decode(PacketType::Tpkt, &[3, 0, 0]), PacketLength::NeedMore);
    }

    // --- LINE_LF -------------------------------------------------------------

    #[test]
    fn a_delimited_line_is_complete_including_its_delimiter() {
        assert_eq!(decode(PacketType::LineLf, b"abc\n"), PacketLength::Complete(4));
        assert_eq!(decode(PacketType::LineLf, b"\n"), PacketLength::Complete(1));
    }

    #[test]
    fn a_carriage_return_is_part_of_the_line() {
        // The delimiter is `\n`, so the `\r` of a CRLF terminator stays in the frame.
        assert_eq!(
            decode(PacketType::LineLf, b"abc\r\n"),
            PacketLength::Complete(5)
        );
    }

    #[test]
    fn a_line_without_a_delimiter_needs_more() {
        assert_eq!(decode(PacketType::LineLf, b"abc"), PacketLength::NeedMore);
        assert_eq!(decode(PacketType::LineLf, b""), PacketLength::NeedMore);
    }

    #[test]
    fn a_custom_delimiter_frames_the_line() {
        let options = PacketOptions::new(PacketType::LineLf).with_delimiter(b';');
        assert_eq!(options.decode(b"ab;c"), PacketLength::Complete(3));
        assert_eq!(options.decode(b"ab"), PacketLength::NeedMore);
        // The default `\n` is not the delimiter here.
        assert_eq!(options.decode(b"ab\n"), PacketLength::NeedMore);
    }

    #[test]
    fn a_line_at_least_trunc_len_long_is_truncated_without_a_delimiter() {
        let options = PacketOptions::new(PacketType::LineLf).with_trunc_len(3);
        assert_eq!(options.decode(b"abcdef"), PacketLength::Complete(3));
        // Exactly `trunc_len` already triggers the `>=` comparison.
        assert_eq!(options.decode(b"abc"), PacketLength::Complete(3));
        // One byte less does not.
        assert_eq!(options.decode(b"ab"), PacketLength::NeedMore);
    }

    #[test]
    fn a_delimited_line_longer_than_trunc_len_is_truncated() {
        let options = PacketOptions::new(PacketType::LineLf).with_trunc_len(3);
        // The line *including* its delimiter is 7 bytes, longer than `trunc_len`.
        assert_eq!(options.decode(b"abcdef\n"), PacketLength::Complete(3));
        // A line whose length (with delimiter) is exactly `trunc_len` is *not* truncated
        // (the comparison is a strict `>`).
        let exact = PacketOptions::new(PacketType::LineLf).with_trunc_len(4);
        assert_eq!(exact.decode(b"abc\n"), PacketLength::Complete(4));
    }

    #[test]
    fn a_line_without_a_delimiter_over_max_plen_is_invalid() {
        let options = PacketOptions::new(PacketType::LineLf).with_max_plen(3);
        assert_eq!(options.decode(b"abcd"), PacketLength::Invalid);
        // Exactly `max_plen` is not "greater than" it, so it is still need-more.
        assert_eq!(options.decode(b"abc"), PacketLength::NeedMore);
    }

    #[test]
    fn a_delimited_line_over_max_plen_is_invalid() {
        let options = PacketOptions::new(PacketType::LineLf).with_max_plen(3);
        assert_eq!(options.decode(b"abcd\n"), PacketLength::Invalid);
    }

    // --- Unsupported types ---------------------------------------------------

    #[test]
    fn the_unsupported_types_report_unsupported() {
        for packet in [
            PacketType::Http,
            PacketType::HttpH,
            PacketType::HttpBin,
            PacketType::HttpHBin,
            PacketType::SslTls,
        ] {
            assert_eq!(
                decode(packet, b"GARBAGE BYTES"),
                PacketLength::Unsupported,
                "{packet:?}"
            );
        }
    }

    #[test]
    fn every_supported_type_decodes_something_other_than_unsupported() {
        for packet in PacketType::ALL {
            let outcome = decode(packet, b"");
            if packet.is_supported() {
                assert_ne!(outcome, PacketLength::Unsupported, "{packet:?}");
            } else {
                assert_eq!(outcome, PacketLength::Unsupported, "{packet:?}");
            }
        }
    }

    // --- PacketLength helpers ------------------------------------------------

    #[test]
    fn packet_length_helpers_expose_the_total_length() {
        assert!(PacketLength::Complete(5).is_complete());
        assert_eq!(PacketLength::Complete(5).complete(), Some(5));
        assert!(!PacketLength::NeedMore.is_complete());
        assert_eq!(PacketLength::NeedMore.complete(), None);
        assert_eq!(PacketLength::Invalid.complete(), None);
    }

    #[test]
    fn as_error_maps_invalid_to_badarg_and_unsupported_to_notsup() {
        assert!(matches!(
            PacketLength::Invalid.as_error(),
            Some(BeamError::BadArg)
        ));
        let unsupported = PacketLength::Unsupported
            .as_error()
            .expect("an unsupported type is a typed error");
        assert_eq!(unsupported.as_atom(), Some("notsup"));
        assert!(PacketLength::Complete(1).as_error().is_none());
        assert!(PacketLength::NeedMore.as_error().is_none());
    }

    // --- payload_range (the C's packet_get_body) ------------------------------

    #[test]
    fn payload_range_strips_the_length_prefix_of_1_2_and_4() {
        assert_eq!(PacketType::Len1.payload_range(b"\x01ab").unwrap(), 1..3);
        assert_eq!(PacketType::Len2.payload_range(b"\x00\x02ab").unwrap(), 2..4);
        assert_eq!(
            PacketType::Len4.payload_range(b"\x00\x00\x00\x02ab").unwrap(),
            4..6
        );
    }

    #[test]
    fn payload_range_is_the_whole_frame_for_every_other_type() {
        for packet in [
            PacketType::Raw,
            PacketType::Asn1,
            PacketType::Rm,
            PacketType::Cdr,
            PacketType::LineLf,
            PacketType::Tpkt,
            PacketType::Http,
            PacketType::SslTls,
        ] {
            assert_eq!(packet.payload_range(b"payload").unwrap(), 0..7, "{packet:?}");
        }
    }

    #[test]
    fn payload_range_keeps_the_fcgi_header_and_drops_the_trailing_padding() {
        // The C never advances the buffer for FCGI, so the 8-byte header is part of the
        // "body"; only the padding at the end is removed.
        let frame = fcgi_frame(1, [0x00, 0x03], 0x02);
        assert_eq!(PacketType::Fcgi.payload_range(&frame).unwrap(), 0..6);
    }

    #[test]
    fn payload_range_rejects_a_frame_shorter_than_its_header() {
        assert!(PacketType::Len1.payload_range(b"").is_err());
        assert!(PacketType::Len2.payload_range(b"\x00").is_err());
        assert!(PacketType::Len4.payload_range(b"\x00\x00\x00").is_err());
        assert!(PacketType::Fcgi.payload_range(&[0u8; 7]).is_err());
    }

    #[test]
    fn payload_range_rejects_fcgi_padding_larger_than_the_frame() {
        let mut frame = [0u8; 8];
        frame[6] = 0xff;
        assert!(PacketType::Fcgi.payload_range(&frame).is_err());
    }

    // --- The decoder in the shape the driver uses it --------------------------

    #[test]
    fn a_full_decode_then_strip_round_trip_for_a_prefixed_type() {
        let options = PacketOptions::new(PacketType::Len2).with_max_plen(64);
        let frame = [0x00, 0x03, b'a', b'b', b'c'];
        let total = options.decode(&frame).complete().expect("a known length");
        assert_eq!(total, frame.len());
        let body = options.packet.payload_range(&frame[..total]).unwrap();
        assert_eq!(&frame[body], b"abc");
    }

    #[test]
    fn a_full_decode_then_strip_round_trip_for_a_delimited_type() {
        let options = PacketOptions::new(PacketType::LineLf).with_delimiter(b'|');
        let buffered = b"hello|world|";
        let total = options.decode(buffered).complete().expect("a known length");
        assert_eq!(total, 6);
        let body = options.packet.payload_range(&buffered[..total]).unwrap();
        assert_eq!(&buffered[body], b"hello|");
    }
}
