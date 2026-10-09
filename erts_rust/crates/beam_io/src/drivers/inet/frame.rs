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

//! The `tcp_inet` **read-path framing glue** — the bridge between the socket options the
//! driver stores ([`SocketOptions`]) and the pure, term-free packet decoder in
//! [`super::packet`].
//!
//! [`super::packet`] turns a byte buffer plus four framing parameters into "this is one frame
//! of `n` bytes" — a faithful port of the C's `packet_get_length` (`packet_parser.c:248`). This
//! module is the small layer on top that the driver's read path actually calls:
//!
//! * [`options`] builds the decoder's [`PacketOptions`] from the driver's stored
//!   `{packet, N}`/`{packet_size, N}`/`{line_delimiter, C}` socket options, exactly as the C
//!   passes them to `packet_get_length(desc->inet.htype, ptr, n, desc->inet.psize,
//!   desc->i_bufsz, desc->inet.delimiter, &desc->http_state)` (`inet_drv.c:12975`);
//! * [`framed`] answers whether the current `{packet, N}` code needs framing at all — only
//!   [`PacketType::Raw`] and a code no [`PacketType`] uses deliver the byte stream untouched;
//! * [`extract`] takes at most one whole frame's **payload** off the front of the buffer the
//!   driver accumulates, the driver-side half of the C's `tcp_remain`/`tcp_reply_data` pair: it
//!   reports a whole frame ([`Some`]), "not yet complete" ([`None`]) or a typed decode failure,
//!   and **never** a partial, merged or wrong frame.
//!
//! The decode itself is [`PacketType::payload_range`] (the C's `packet_get_body`,
//! `packet_parser.h:152`): the header is stripped for [`Len1`](PacketType::Len1)/
//! [`Len2`](PacketType::Len2)/[`Len4`](PacketType::Len4) and for [`Fcgi`](PacketType::Fcgi), and
//! every other type — [`LineLf`](PacketType::LineLf) included — is returned "as is", so a
//! `{packet, line}` payload **keeps its delimiter**, just as the oracle's `gen_tcp:recv` does.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/packet_parser.{c,h}` and
//! `erts/emulator/drivers/common/inet_drv.c` (`inet_set_opts`'s option store, `tcp_remain`,
//! `tcp_reply_data`).

use beam_common::Result;

use crate::error;

use super::opt::SocketOptions;
use super::packet::{PacketLength, PacketOptions, PacketType};

/// The decoder parameters the driver's stored socket options describe — the arguments the C
/// passes to `packet_get_length` (`inet_drv.c:12975`).
///
/// The mapping is the C's own:
///
/// * `{packet, N}` (`desc->inet.htype`) ⇒ [`PacketOptions::packet`], through
///   [`PacketType::from_code`]. The stored option **is** the C's `TCP_PB_*` code, which the
///   Erlang layer's `prim_inet` encodes (`{packet,4}` ⇒ `TCP_PB_4` = 3,
///   `prim_inet.erl:1754`); `getopts` reports it back verbatim. An out-of-range code — which the
///   option layer stores verbatim and the C's `packet_get_length` `default:` arm would reject —
///   falls back to [`PacketType::Raw`], matching [`framed`] (see that function for the choice).
/// * `{line_delimiter, C}` (`desc->inet.delimiter`) ⇒ [`PacketOptions::delimiter`].
/// * `{packet_size, N}` (`desc->inet.psize`) ⇒ [`PacketOptions::max_plen`] **when non-zero** —
///   the C passes `desc->inet.psize` straight in as `max_plen` (`:12976`), where `0` means "no
///   limit". The C stores the option as an `unsigned int` (`inet_set_opts`, `:7031`), so a
///   negative `{packet_size, N}` becomes the large unsigned the C would use.
///
/// [`PacketOptions::trunc_len`] stays `0` ("no limit"): the C sets it to the descriptor's fixed
/// input-buffer size (`desc->i_bufsz`, `:12976`), a buffer this driver does not have — it
/// accumulates into a growable `Vec` instead — so there is no buffer size to truncate a line at.
/// A `{packet_size, N}` still bounds a frame through `max_plen`.
pub(super) fn options(options: &SocketOptions) -> PacketOptions {
    let packet = PacketType::from_code(options.packet as u8).unwrap_or(PacketType::Raw);
    let mut framing = PacketOptions::new(packet).with_delimiter(options.line_delim);
    let packet_size = options.packet_size as u32;
    if packet_size != 0 {
        framing = framing.with_max_plen(packet_size as usize);
    }
    framing
}

/// Whether the current `{packet, N}` code needs the packet decoder on the read path.
///
/// `None` — the byte stream is delivered untouched — for [`PacketType::Raw`] and for a code no
/// [`PacketType`] uses. `Some(packet)` for every framing type, **including** the unsupported
/// HTTP/`ssl_tls` types ([`PacketType::is_supported`] is false), which [`extract`] then rejects
/// with a typed error rather than delivering a half-parsed frame.
///
/// **Choice (the C's `default:` arm diverges):** an out-of-range code is treated as "no framing"
/// rather than failing the read the way the C's `packet_get_length` `default: return -1` would.
/// The option layer stores the code verbatim, so this is reachable only through a hand-set
/// `htype`; delivering the stream raw is the honest "this driver cannot frame that code"
/// default, and it never fabricates a frame.
pub(super) fn framed(options: &SocketOptions) -> Option<PacketType> {
    match PacketType::from_code(options.packet as u8) {
        Some(PacketType::Raw) | None => None,
        Some(packet) => Some(packet),
    }
}

/// Takes at most one complete frame's **payload** off the front of `pending`.
///
/// Reads the frame header off the buffered bytes ([`PacketOptions::decode`], the C's
/// `packet_get_length`) and, when the whole frame is present, strips its header
/// ([`PacketType::payload_range`], the C's `packet_get_body`), consumes the frame's `total`
/// bytes from `pending` and returns the payload.
///
/// * `Ok(Some(payload))` — one complete frame; **exactly one** frame is consumed per call, so a
///   caller driving one `recv` gets one frame (the C's `TCP_REQ_RECV` delivers one packet);
/// * `Ok(None)` — not enough bytes are buffered yet (the C's `tcp_remain` `tlen == 0` / a header
///   whose declared body has not all arrived): the buffer is left untouched for the next read;
/// * `Err(_)` — the buffered bytes cannot be a frame for this type
///   ([`PacketLength::Invalid`] ⇒ `badarg`) or the type is not decoded at all
///   ([`PacketLength::Unsupported`] ⇒ `notsup`). The buffer is left untouched.
///
/// It never returns a partial or merged frame: the payload comes from a frame the decoder sized
/// in full.
pub(super) fn extract(pending: &mut Vec<u8>, framing: &PacketOptions) -> Result<Option<Vec<u8>>> {
    match framing.decode(pending) {
        // The frame's total length is known and the whole frame is buffered: strip the header
        // and hand back the payload, consuming the whole frame.
        PacketLength::Complete(total) if total <= pending.len() => {
            let payload = framing.packet.payload_range(&pending[..total])?;
            let frame = pending[payload].to_vec();
            pending.drain(..total);
            Ok(Some(frame))
        }
        // The length is known but its body is not all here, or there is not even a full header
        // yet: read more and try again.
        PacketLength::Complete(_) | PacketLength::NeedMore => Ok(None),
        // The bytes are not a valid frame, or the type is not decoded here.
        PacketLength::Invalid => Err(error::badarg()),
        PacketLength::Unsupported => Err(error::notsup()),
    }
}

#[cfg(test)]
mod tests {
    use super::{extract, framed, options, SocketOptions};
    use super::{PacketOptions, PacketType};
    use beam_common::BeamError;

    /// The stored socket options for `packet_code`/`packet_size`/`line_delim` (everything else
    /// default).
    fn socket(packet_code: u8, packet_size: i32, line_delim: u8) -> SocketOptions {
        let mut stored = SocketOptions::default();
        stored.packet = packet_code as i32;
        stored.packet_size = packet_size;
        stored.line_delim = line_delim;
        stored
    }

    // --- options -------------------------------------------------------------

    #[test]
    fn options_map_the_packet_code_the_delimiter_and_the_packet_size() {
        // `{packet, 4}` is stored as the C's `TCP_PB_4` code (3), not as the Erlang value 4.
        let framing = options(&socket(PacketType::Len4.code(), 0, b'|'));
        assert_eq!(framing.packet, PacketType::Len4);
        assert_eq!(framing.delimiter, b'|');
        assert_eq!(framing.max_plen, 0, "`packet_size` 0 is 'no limit'");
        assert_eq!(framing.trunc_len, 0, "no fixed input buffer to truncate at");
    }

    #[test]
    fn a_nonzero_packet_size_becomes_the_max_frame_length() {
        assert_eq!(options(&socket(PacketType::Len2.code(), 100, b'\n')).max_plen, 100);
        // The C stores `psize` as an `unsigned int`, so a negative option is a large unsigned.
        assert_eq!(
            options(&socket(PacketType::Len2.code(), -1, b'\n')).max_plen,
            u32::MAX as usize
        );
    }

    #[test]
    fn an_unknown_packet_code_falls_back_to_raw() {
        assert_eq!(options(&socket(200, 0, b'\n')).packet, PacketType::Raw);
    }

    // --- framed --------------------------------------------------------------

    #[test]
    fn framed_is_none_only_for_raw_and_an_unknown_code() {
        assert_eq!(framed(&socket(PacketType::Raw.code(), 0, b'\n')), None);
        assert_eq!(framed(&socket(99, 0, b'\n')), None, "an out-of-range code");
        for packet in [
            PacketType::Len1,
            PacketType::Len2,
            PacketType::Len4,
            PacketType::Asn1,
            PacketType::LineLf,
            PacketType::Http,
        ] {
            assert_eq!(framed(&socket(packet.code(), 0, b'\n')), Some(packet));
        }
    }

    // --- extract -------------------------------------------------------------

    #[test]
    fn extract_returns_one_full_frame_and_consumes_just_it() {
        let framing = PacketOptions::new(PacketType::Len2);
        // Two frames back to back: the first is consumed, the second is left untouched.
        let mut pending = vec![0x00, 0x03, b'a', b'b', b'c', 0x00, 0x01, b'z'];
        let payload = extract(&mut pending, &framing).expect("a decode").expect("a frame");
        assert_eq!(payload, b"abc", "the two-byte header is stripped");
        assert_eq!(pending, vec![0x00, 0x01, b'z'], "only one frame is consumed");
        let payload = extract(&mut pending, &framing).expect("a decode").expect("a frame");
        assert_eq!(payload, b"z");
        assert!(pending.is_empty());
    }

    #[test]
    fn extract_is_none_until_the_whole_frame_is_buffered() {
        let framing = PacketOptions::new(PacketType::Len4);
        // Less than a whole header: the length is not even known yet.
        let mut pending = vec![0x00, 0x00, 0x00];
        assert_eq!(extract(&mut pending, &framing).expect("a decode"), None);
        assert_eq!(pending, vec![0x00, 0x00, 0x00], "the partial header is retained");
        // The whole header, but the declared four-byte body is only partly there.
        pending.extend_from_slice(&[0x04, b'a', b'b']);
        assert_eq!(extract(&mut pending, &framing).expect("a decode"), None);
        assert_eq!(pending, vec![0x00, 0x00, 0x00, 0x04, b'a', b'b'], "kept intact");
        // The rest arrives.
        pending.extend_from_slice(b"cd");
        assert_eq!(
            extract(&mut pending, &framing).expect("a decode").expect("a frame"),
            b"abcd"
        );
        assert!(pending.is_empty());
    }

    #[test]
    fn extract_invalid_is_badarg_and_unsupported_is_notsup() {
        // An ASN.1 *long* length field wider than four bytes the decoder rejects (the C's
        // `default: goto error`): tag `0x04` (short form) then `0x85` = "a five-byte length".
        let invalid = vec![0x04, 0x85, 0, 0, 0, 0, 0];
        let mut pending = invalid.clone();
        assert!(matches!(
            extract(&mut pending, &PacketOptions::new(PacketType::Asn1)),
            Err(BeamError::BadArg)
        ));
        assert_eq!(pending, invalid, "left untouched on failure");

        // The HTTP family has no decoder here: a typed `notsup`, never a wrong frame.
        let mut pending = b"GET / HTTP/1.0\r\n\r\n".to_vec();
        let before = pending.clone();
        let err = extract(&mut pending, &PacketOptions::new(PacketType::Http))
            .expect_err("HTTP is not decoded");
        assert_eq!(err.as_atom(), Some("notsup"));
        assert_eq!(pending, before, "left untouched");
    }

    #[test]
    fn extract_keeps_the_line_delimiter_exactly_like_the_oracle() {
        // The C's `packet_get_body` returns a line "as is", and so the oracle's
        // `gen_tcp:recv` hands back the delimiter-included line.
        let framing = PacketOptions::new(PacketType::LineLf);
        let mut pending = b"abc\ndef\n".to_vec();
        assert_eq!(
            extract(&mut pending, &framing).expect("a decode").expect("a frame"),
            b"abc\n"
        );
        assert_eq!(pending, b"def\n");
    }

    #[test]
    fn extract_honours_a_custom_line_delimiter() {
        let framing = PacketOptions::new(PacketType::LineLf).with_delimiter(b'|');
        let mut pending = b"hello|world|".to_vec();
        assert_eq!(
            extract(&mut pending, &framing).expect("a decode").expect("a frame"),
            b"hello|"
        );
        assert_eq!(pending, b"world|");
    }
}
