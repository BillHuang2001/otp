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

//! The `{packet, N}` **framing** of a port's stream (contract A.4): the length header the
//! emulator prepends on the way out and the packet reassembly it performs on the way in.
//!
//! `{packet, N}` is not a driver feature for a `spawn`/`spawn_executable`/`fd` port — it is
//! the **sys driver's** own framing (`erts/emulator/sys/unix/sys_drivers.c`), which is why it
//! lives here rather than in [`beam_io`](beam_io): the C's `output`/`outputv` prepend the
//! header (`sys_drivers.c:1144-1160`, `:1062-1077`) and its `ready_input` reassembles the
//! child's bytes into whole packets (`:1268-1450`).
//!
//! # What the oracle does (measured on erts-17.1, pinned by `packet_framing_tests.rs`)
//!
//! | Option | Outgoing | Incoming |
//! |---|---|---|
//! | `{packet,1}` | `<<Len:8, Payload/binary>>` | one message per 1-byte header |
//! | `{packet,2}` | `<<Len:16, Payload/binary>>` | one message per 2-byte header |
//! | `{packet,4}` | `<<Len:32, Payload/binary>>` | one message per 4-byte header |
//! | (none) / `stream` | the payload verbatim | one message per driver read |
//!
//! `Len` is the **payload's byte length** in big-endian order. The header is written even for
//! an empty payload (`port_command(P, <<>>)` on a `{packet,4}` port writes `<<0,0,0,0>>`), and
//! an **empty packet is a real message on the way in**: a child that writes `<<0,0,0,0>>`
//! makes the owner receive `{Port,{data,<<>>}}` — unlike the unframed stream, whose empty
//! reads are no message at all.
//!
//! Neither counter sees the header: `port_info(Port, input)` counts the bytes **delivered**
//! and `port_info(Port, output)` the bytes `port_command/2` was handed, so a 7-byte wire
//! packet reports `3` on both sides (measured).
//!
//! # What stays unframed, and why
//!
//! A `{spawn_driver, Name}` port's `packet` option belongs to the **driver**, exactly as in
//! the C: the emulator's `packet_bytes` is read only by the sys drivers, and an inet driver
//! does its own `packet_inet_*` framing. Measured: `open_port({spawn_driver,"ram_file_drv"},
//! [{packet,4}])` answers a bare `SIZE` command byte for byte as the unframed port does, i.e.
//! the emulator added no header. So [`crate::port::PortName::frames_packets`] admits the
//! spawn/fd family only, and a driver port is left to its driver (for the inet drivers that
//! is [`beam_io`]'s own packet decoder, reached through the option/`setopts` bridge).
//!
//! # The two refusals the C has, and what happens to them here
//!
//! * **A payload wider than the header can express.** `{packet,1}` refuses more than 255
//!   bytes and `{packet,2}` more than 65535 (`sys_drivers.c:1155-1163`, `:1145-1151`), and
//!   the refusal is `driver_failure_posix(ix, EINVAL)` — the port **dies** with the exit
//!   reason `einval` (measured: a trapping owner receives `{'EXIT',Port,einval}` and the
//!   child receives nothing). Port death with a reason other than `normal` is not something
//!   this runtime's close path can express (see [`super::exit`]), so the write is a **typed
//!   capability gap** instead — never a truncated header, never a silent drop.
//! * **A partial packet at end of stream.** The C's `port_inp_failure` drops it: nothing is
//!   delivered and the port closes. The bytes held in [`PacketFraming::buffer`] die with the
//!   port's [`PortMeta`](super::PortMeta), so the same holds here by construction.
//!
//! A header wider than the bytes that arrived is simply buffered until the rest shows up,
//! however many reads that takes (the C's `ifd.remain` arm): a child may write one packet in
//! several `write(2)`s.
use beam_common::{BeamError, Result};

use crate::error;

/// The header widths `open_port/2`'s `{packet, N}` accepts — the C's `switch
/// (opts.packet_bytes)` (`erts/emulator/beam/erl_bif_port.c:854-860`): `1`, `2` and `4`.
///
/// Everything else is `badarg`, `{packet,0}` included (the C's `default: goto bad_settings`;
/// turning framing *off* is the `stream` atom's job).
pub(super) const PACKET_WIDTHS: [usize; 3] = [1, 2, 4];

/// One port's `{packet, N}` framing state: the header width and the partial packet carried
/// across reads.
///
/// A port without `{packet, N}` has **no** `PacketFraming` at all (the field is an
/// `Option`), so the unframed path never consults this type and pays nothing for it.
#[derive(Debug, Clone)]
pub(super) struct PacketFraming {
    /// The header width in bytes — the C's `prt->opts.packet_bytes`, always one of
    /// [`PACKET_WIDTHS`] here (a `0` width means "no framing" and is an absent
    /// `PacketFraming`).
    width: usize,
    /// The bytes of a packet that has not been delivered yet: a partial header, or a complete
    /// header followed by fewer payload bytes than it announces. The C's `ifd->pbuf`/`ifd->buf`
    /// pair, folded into one buffer (a header is a prefix of its packet, so one buffer suffices).
    buffer: Vec<u8>,
}

impl PacketFraming {
    /// The framing of a `{packet, width_bytes}` port, or `None` for `0` (the option is off).
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a width outside [`PACKET_WIDTHS`] — a caller that reaches
    /// here with one has bypassed [`super::RuntimePorts::port_options`], which is the only
    /// producer.
    pub(super) fn new(width_bytes: usize) -> Result<Option<PacketFraming>> {
        if width_bytes == 0 {
            return Ok(None);
        }
        if !PACKET_WIDTHS.contains(&width_bytes) {
            return Err(BeamError::BadArg);
        }
        Ok(Some(PacketFraming {
            width: width_bytes,
            buffer: Vec::new(),
        }))
    }

    /// The bytes to hand the port for a `port_command/2` payload: the length header followed
    /// by the payload — the C's `output`/`outputv` `put_int32(len, lb); lbp = lb + (4-pb)`.
    ///
    /// # Errors
    ///
    /// The typed capability gap of a payload the header cannot express (see the module docs).
    pub(super) fn frame_output(&self, payload: &[u8]) -> Result<Vec<u8>> {
        if !self.width_fits(payload.len()) {
            return Err(error::unsupported(&format!(
                "packet framing: a {}-byte port_command payload does not fit a \
                 {{packet,{}}} header (the reference runtime kills the port with einval \
                 here, and this runtime does not deliver a port exit reason other than \
                 normal)",
                payload.len(),
                self.width
            )));
        }
        // `width_fits` guarantees the cast below for every accepted width.
        let length = payload.len() as u32;
        let header = length.to_be_bytes();
        let mut framed = Vec::with_capacity(self.width + payload.len());
        framed.extend_from_slice(&header[4 - self.width..]);
        framed.extend_from_slice(payload);
        Ok(framed)
    }

    /// Whether a payload of `len` bytes fits this width's header — the C's `pb == 1 &&
    /// len > 0xff` / `pb == 2 && len > 0xffff` refusals (`sys_drivers.c:1145-1151`); a
    /// `{packet,4}` header bounds the payload by its own 32-bit field.
    fn width_fits(&self, len: usize) -> bool {
        match self.width {
            1 => len <= usize::from(u8::MAX),
            2 => len <= usize::from(u16::MAX),
            _ => u32::try_from(len).is_ok(),
        }
    }

    /// Feeds one driver read into the reassembler and answers every packet it completed, in
    /// stream order.
    ///
    /// The returned payloads are exactly what the C hands `driver_output` — with the header
    /// consumed, and **including** a zero-length packet (a header of `0`), which the C
    /// delivers as `{Port,{data,<<>>}}` (`sys_drivers.c:1418-1420`). A read that completes no
    /// packet answers an empty vector and leaves its bytes buffered.
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`] when the buffer cannot grow: the C's `erts_alloc_fnf`
    /// refusal, which kills the port with `ENOMEM`. Reported rather than aborting the runtime.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        if !bytes.is_empty() {
            self.buffer
                .try_reserve(bytes.len())
                .map_err(|_| BeamError::SystemLimit)?;
            self.buffer.extend_from_slice(bytes);
        }
        let mut packets = Vec::new();
        // The bytes already consumed by the packets taken below; the buffer is drained of them
        // in one go at the end so a burst of packets from one read is not quadratic.
        let mut consumed = 0;
        loop {
            let rest = &self.buffer[consumed..];
            if rest.len() < self.width {
                break;
            }
            let length = packet_length(&rest[..self.width], self.width);
            // A split packet: the header is complete but the payload is not. Its bytes stay
            // buffered, exactly as the C's `ifd->remain` arm holds them.
            if rest.len() - self.width < length {
                break;
            }
            packets.push(rest[self.width..self.width + length].to_vec());
            consumed += self.width + length;
        }
        self.buffer.drain(..consumed);
        Ok(packets)
    }

    /// The bytes held for a packet that is not complete yet — `port_info`-free, test-only
    /// introspection of the reassembler's state.
    #[cfg(test)]
    pub(super) fn buffered(&self) -> &[u8] {
        &self.buffer
    }
}

/// The big-endian length of a packet from its first `width` bytes (the C's `get_int8`/
/// `get_int16`/`get_uint32` switch, `sys_drivers.c:1410-1414`).
fn packet_length(header: &[u8], width: usize) -> usize {
    let mut word = [0u8; 4];
    word[4 - width..].copy_from_slice(&header[..width]);
    u32::from_be_bytes(word) as usize
}
