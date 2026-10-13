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

//! The `epmd` port-mapper client protocol.
//!
//! Ground truth: `lib/erl_interface/src/epmd/epmd_publish.c` + `epmd_port.c` +
//! `ei_epmd.h` (client wire), `erts/epmd/src/epmd.h` (the tag bytes),
//! `erts/epmd/src/epmd_cli.c` (the interactive `-dump`/`-kill`/`-stop` CLI) and
//! `erts/epmd/src/epmd_srv.c` + `epmd_int.h` (server side). The client drives the
//! protocol over any [`Transport`], so the whole codec layer is exercised against
//! [`MemoryTransport`](crate::transport::MemoryTransport) with no sockets;
//! [`EpmdClient::connect`] adds the concrete `std::net::TcpStream` path (and
//! [`lookup_at`]/[`lookup_port_at`] — with [`lookup`]/[`lookup_port`] as the
//! [`EPMD_DEFAULT_ADDR`]-pinned forms — the one-shot `PORT2_REQ` lookup) against
//! a real `epmd`.
//!
//! # The framing is asymmetric
//!
//! A **request** is 2-byte big-endian length-prefixed
//! ([`PacketHeaderSize::Two`](crate::framing::PacketHeaderSize::Two) — the server
//! reads exactly that many bytes, `epmd_srv.c:697`). A **response** is *not*
//! length-prefixed: the server writes the reply body straight to the socket
//! (`epmd_srv.c:1547` `reply()`), so the tag byte is the very first byte of the
//! reply (the C client reads the tag and the result byte first, `epmd_port.c:139`;
//! `erl_epmd.erl:546` matches `[$w, Result | Rest]`). A reply is therefore read
//! self-delimiting — by its tag for [`Alive2Resp`]/[`Port2Resp`], by the literal
//! 2/7-byte text for [`KillResp`]/[`StopResp`], and "until the transport runs out
//! of bytes" for the open-ended [`NamesResp`]/[`DumpResp`] line lists.
//!
//! # Interactive requests (`-dump`/`-kill`/`-stop`)
//!
//! Besides the node-to-node requests (`ALIVE2_REQ`, `PORT2_REQ`, `NAMES_REQ`) the
//! daemon answers the three interactive CLI codes: `DUMP_REQ` ([`DumpReq`]), which
//! is read-only and safe against a machine's live `epmd`; `KILL_REQ`
//! ([`KillReq`]) and `STOP_REQ` ([`StopReq`]), which are **destructive**
//! (they terminate the daemon / unregister a node) and are therefore covered only
//! by codec round-trips in the test suite.
//!
//! The whole port-mapper protocol lives in this one module — the tag
//! re-exports, the request/response codecs, the self-delimiting reply readers and
//! the [`EpmdClient`] methods — so the wire is reviewable in a single place. (No
//! sub-module is declared here: the crate's module tree in `lib.rs` is owned by
//! another part of the tree.)

use beam_common::{BeamError, Result};
use crate::connection::TcpTransport;
use crate::flags::{DIST_VER_HIGH, DIST_VER_LOW};
use crate::framing::{write_packet, PacketHeaderSize};
use crate::transport::{Read, Transport, Write};

pub use crate::flags::{
    EPMD_ALIVE2_REQ, EPMD_ALIVE2_RESP, EPMD_ALIVE2_X_RESP, EPMD_DUMP_REQ, EPMD_KILL_REQ,
    EPMD_NAMES_REQ, EPMD_NODE_HIDDEN, EPMD_NODE_NORMAL, EPMD_PORT2_REQ, EPMD_PORT2_RESP,
    EPMD_PORT_PLEASE2_REQ, EPMD_STOP_REQ,
};

/// `EI_MYPROTO`: the protocol field of an `ALIVE2_REQ` (`0` = tcp/ip).
pub const EPMD_PROTO_TCP: u8 = 0;
/// The highest distribution version advertised to `epmd` (`EI_DIST_HIGH`).
pub const EPMD_DIST_HIGH: u16 = DIST_VER_HIGH;

/// The lowest distribution version advertised to `epmd` (`EI_DIST_LOW`).
pub const EPMD_DIST_LOW: u16 = DIST_VER_LOW;

/// The `result` byte meaning "success" in an `ALIVE2_RESP`/`PORT2_RESP`.
const RESULT_OK: u8 = 0;

/// The length of a `STOP_RESP`: `"STOPPED"` / `"NOEXIST"` are both 7 bytes, and
/// the CLI client reads exactly seven (`epmd_cli.c::stop_cli` `read_fill(fd,
/// buf, 7)`).
const STOP_RESP_LEN: usize = 7;

/// The ceiling on a buffered [`DumpResp`] body (port + all node lines).
///
/// `epmd` bounds its own node tables — `max_conn` live connections plus at most
/// `MAX_UNREG_COUNT` remembered old/unused names (`epmd_int.h`), i.e. a few
/// hundred KiB of text in the worst case — so a real dump always fits. The
/// ceiling exists so a wedged or hostile peer can never drive unbounded
/// allocation: exceeding it surfaces as [`BeamError::SystemLimit`].
///
/// (The `DUMP_REQ`/`KILL_REQ` request body is the tag alone — `epmd_cli.c` frames
/// a one-byte body for both: `put_int16(1,buf); buf[2] = what;`.)
pub const MAX_DUMP_RESP_BODY: usize = 1 << 20;

/// The `DUMP_RESP` "active node" line prefix (`epmd_srv.c` `case EPMD_DUMP_REQ`
/// spells it with five spaces between `name` and the `<`).
const DUMP_ACTIVE_PREFIX: &str = "active name     <";

/// The `DUMP_RESP` "old/unused node" line prefix (`epmd_srv.c`, one space).
const DUMP_OLD_PREFIX: &str = "old/unused name <";

/// A cursor over an already-received message body.
///
/// Every reader is bounds-checked: running past the end of the body (a truncated
/// message) yields `BeamError::BadArg` — never a panic.
struct BodyReader<'a> {    /// The whole message body.
    body: &'a [u8],
    /// The read cursor into `body`.
    pos: usize,
}

impl<'a> BodyReader<'a> {
    /// Starts a cursor at the front of `body`.
    fn new(body: &'a [u8]) -> BodyReader<'a> {
        BodyReader { body, pos: 0 }
    }

    /// Reads one byte.
    fn u8(&mut self) -> Result<u8> {
        let byte = *self.body.get(self.pos).ok_or(BeamError::BadArg)?;
        self.pos += 1;
        Ok(byte)
    }

    /// Reads a big-endian `u16`.
    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Reads a big-endian `u32`.
    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads the next `n` bytes (all of them, or `BadArg`).
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(BeamError::BadArg)?;
        let slice = self.body.get(self.pos..end).ok_or(BeamError::BadArg)?;
        self.pos = end;
        Ok(slice)
    }

    /// The bytes not yet consumed.
    fn remaining(&self) -> &'a [u8] {
        self.body.get(self.pos..).unwrap_or(&[])
    }

    /// Asserts the whole body was consumed (no truncated or trailing bytes).
    fn finish(&self) -> Result<()> {
        if self.pos == self.body.len() {
            Ok(())
        } else {
            Err(BeamError::BadArg)
        }
    }
}

/// A minimal `Write` sink that collects the encoded message body into a `Vec`.
///
/// The codecs are written as `Write`-generic functions; this adapter lets the
/// client build a body to hand to `framing::write_packet` without depending on
/// any concrete transport.
#[derive(Debug, Default)]
struct BodyBuf {
    /// The bytes written so far.
    bytes: Vec<u8>,
}

impl BodyBuf {
    /// An empty body buffer.
    fn new() -> BodyBuf {
        BodyBuf { bytes: Vec::new() }
    }

    /// Consumes the buffer, yielding the collected bytes.
    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BodyBuf {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.bytes.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Writes a length-prefixed name/extra field, rejecting a field that cannot be
/// described by its 16-bit length (`SystemLimit`, never a silent truncation).
fn write_field_len<W: Write>(w: &mut W, len: usize) -> Result<()> {
    let len = u16::try_from(len).map_err(|_| BeamError::SystemLimit)?;
    w.write_u16be(len)
}

/// The `epmd` result byte: `0` = success, `1` = failure (`epmd_srv.c`).
fn result_byte(ok: bool) -> u8 {
    if ok {
        RESULT_OK
    } else {
        1
    }
}

/// The `ALIVE2_REQ` (`'x'`) node-registration request.
///
/// Layout: `'x' + port(2) + node_type(1) + protocol(1) + high_vsn(2) +
/// low_vsn(2) + namelen(2) + name + extralen(2) + extra`. A published tcp/ip
/// Erlang node sends `protocol = EPMD_PROTO_TCP` and
/// `node_type = EPMD_NODE_NORMAL`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Alive2Req {
    /// The node's listen port.
    pub port: u16,
    /// The node type ([`EPMD_NODE_NORMAL`]/[`EPMD_NODE_HIDDEN`]).
    pub node_type: u8,
    /// The protocol ([`EPMD_PROTO_TCP`]).
    pub protocol: u8,
    /// The highest distribution version understood.
    pub high_vsn: u16,
    /// The lowest distribution version understood.
    pub low_vsn: u16,
    /// The node name.
    pub name: Vec<u8>,
    /// The (usually empty) extra field.
    pub extra: Vec<u8>,
}

impl Alive2Req {
    /// Encodes the request body (tag + fields, no length prefix).
    ///
    /// Layout (from `epmd_srv.c` `case EPMD_ALIVE2_REQ`): `'x' + port(2) +
    /// node_type(1) + protocol(1) + high_vsn(2) + low_vsn(2) + namelen(2) + name
    /// + extralen(2) + extra`.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u8(EPMD_ALIVE2_REQ)?;
        w.write_u16be(self.port)?;
        w.write_u8(self.node_type)?;
        w.write_u8(self.protocol)?;
        w.write_u16be(self.high_vsn)?;
        w.write_u16be(self.low_vsn)?;
        write_field_len(w, self.name.len())?;
        w.write_all(&self.name)?;
        write_field_len(w, self.extra.len())?;
        w.write_all(&self.extra)?;
        Ok(())
    }

    /// Decodes a request; a wrong/short tag is `BeamError::BadArg`.
    ///
    /// `namelen`/`extralen` are validated against the bytes actually present, so
    /// a truncated body is rejected rather than mis-sliced.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        if r.u8()? != EPMD_ALIVE2_REQ {
            return Err(BeamError::BadArg);
        }
        let port = r.u16()?;
        let node_type = r.u8()?;
        let protocol = r.u8()?;
        let high_vsn = r.u16()?;
        let low_vsn = r.u16()?;
        let namelen = usize::from(r.u16()?);
        let name = r.take(namelen)?.to_vec();
        let extralen = usize::from(r.u16()?);
        let extra = r.take(extralen)?.to_vec();
        r.finish()?;
        Ok(Alive2Req { port, node_type, protocol, high_vsn, low_vsn, name, extra })
    }
}

/// The `ALIVE2_RESP` reply to an [`Alive2Req`].
///
/// Layout: `tag + result(1) + creation(2|4)`. `EPMD_ALIVE2_RESP` (`'y'`) carries a
/// 16-bit creation; `EPMD_ALIVE2_X_RESP` (`'v'`) carries a 32-bit creation.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Alive2Resp {
    /// The legacy (`'y'`) response, carrying a 16-bit creation.
    Legacy {
        /// Whether the registration succeeded (`result == 0`).
        ok: bool,
        /// The assigned node creation.
        creation: u16,
    },
    /// The extended (`'v'`) response, carrying a 32-bit creation.
    Extended {
        /// Whether the registration succeeded (`result == 0`).
        ok: bool,
        /// The assigned node creation.
        creation: u32,
    },
}

impl Alive2Resp {
    /// Encodes the response body (tag + fields, no length prefix).
    ///
    /// `Legacy` is `'y' + result(1) + creation(2)`; `Extended` is `'v' +
    /// result(1) + creation(4)`. `result == 0` means success (`epmd_srv.c`
    /// writes `wbuf[1] = node ? 0 : 1`).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        match self {
            Alive2Resp::Legacy { ok, creation } => {
                w.write_u8(EPMD_ALIVE2_RESP)?;
                w.write_u8(result_byte(*ok))?;
                w.write_u16be(*creation)
            }
            Alive2Resp::Extended { ok, creation } => {
                w.write_u8(EPMD_ALIVE2_X_RESP)?;
                w.write_u8(result_byte(*ok))?;
                w.write_u32be(*creation)
            }
        }
    }

    /// Decodes a response; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let tag = r.u8()?;
        let result = r.u8()?;
        let ok = result == RESULT_OK;
        match tag {
            EPMD_ALIVE2_RESP => {
                let creation = r.u16()?;
                r.finish()?;
                Ok(Alive2Resp::Legacy { ok, creation })
            }
            EPMD_ALIVE2_X_RESP => {
                let creation = r.u32()?;
                r.finish()?;
                Ok(Alive2Resp::Extended { ok, creation })
            }
            _ => Err(BeamError::BadArg),
        }
    }

    /// Whether `epmd` **accepted** the registration.
    ///
    /// `epmd_srv.c` `case EPMD_ALIVE2_REQ` writes `wbuf[1] = node ? 0 : 1`, so a
    /// zero result byte is an accepted registration and a non-zero one a refused
    /// registration; this accessor reads whichever creation width the reply used.
    pub fn is_accepted(&self) -> bool {
        match self {
            Alive2Resp::Legacy { ok, .. } | Alive2Resp::Extended { ok, .. } => *ok,
        }
    }

    /// Whether `epmd` **refused** the registration (the non-zero result byte).
    ///
    /// A refusal is overwhelmingly an *alive name conflict* — the name is already
    /// registered by another live node — see [`is_alive_conflict`](Self::is_alive_conflict).
    pub fn is_rejected(&self) -> bool {
        !self.is_accepted()
    }

    /// Whether the refusal is an **alive name conflict** (the name is already
    /// registered).
    ///
    /// This is a synonym of [`is_rejected`](Self::is_rejected): the wire carries a
    /// single non-zero "failure" byte with no distinct cause code. In `epmd`'s
    /// server it is set when `node_reg2` refuses the registration, which happens
    /// when the name is already occupied by a live node (`epmd_srv.c` "node name
    /// already occupied"), when the name is longer than 255 characters, or when it
    /// is not valid UTF-8 — the name-conflict case being by far the common one (the
    /// OTP suite pins it: a duplicate `register_node` yields `_Res = 1`).
    pub fn is_alive_conflict(&self) -> bool {
        self.is_rejected()
    }

    /// The node creation `epmd` assigned, widened to `u32` (a `'y'` reply carries
    /// a 16-bit creation, a `'v'` reply — what a v6 requester always gets — a
    /// 32-bit one).
    pub fn creation(&self) -> u32 {
        match self {
            Alive2Resp::Legacy { creation, .. } => u32::from(*creation),
            Alive2Resp::Extended { creation, .. } => *creation,
        }
    }
}

/// The `PORT2_REQ` (`'z'`) port-lookup request.///
/// Layout: `'z' + name`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Port2Req {
    /// The node name to look up.
    pub name: Vec<u8>,
}

impl Port2Req {
    /// Encodes the request body (tag + name, no length prefix).
    ///
    /// Layout (`epmd_srv.c` `case EPMD_PORT2_REQ`): `'z' + name` — the name is
    /// the remainder of the packet, with no length field of its own.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u8(EPMD_PORT2_REQ)?;
        w.write_all(&self.name)
    }

    /// Decodes a request; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        if r.u8()? != EPMD_PORT2_REQ {
            return Err(BeamError::BadArg);
        }
        Ok(Port2Req { name: r.remaining().to_vec() })
    }
}

/// The `PORT2_RESP` (`'w'`) reply to a [`Port2Req`].
///
/// Layout: `'w' + result(1)`; on `result == 0` the node fields follow
/// (`port(2) node_type(1) protocol(1) high_vsn(2) low_vsn(2) namelen(2) name
/// extralen(2) extra`), otherwise [`Port2Resp::Error`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Port2Resp {
    /// A successful lookup, carrying the peer's advertised details.
    Ok {
        /// The peer's listen port.
        port: u16,
        /// The peer's node type.
        node_type: u8,
        /// The peer's protocol.
        protocol: u8,
        /// The highest distribution version the peer understands.
        high_vsn: u16,
        /// The lowest distribution version the peer understands.
        low_vsn: u16,
        /// The peer's node name.
        name: Vec<u8>,
        /// The peer's extra field.
        extra: Vec<u8>,
    },
    /// The name is not registered (`result != 0`).
    Error,
}

impl Port2Resp {
    /// Whether the looked-up name **is** registered (`'w'` result byte `0`).
    pub fn is_registered(&self) -> bool {
        matches!(self, Port2Resp::Ok { .. })
    }

    /// Whether `epmd` reported the name as **not registered**.
    ///
    /// This is the [`Port2Resp::Error`] case: `epmd_srv.c` `case EPMD_PORT2_REQ`
    /// answers a name it does not know with exactly `wbuf[0] = 'w'; wbuf[1] = 1;`
    /// (`result != 0`, no node block). [`lookup_port`] maps it to
    /// [`BeamError::Io`] with [`std::io::ErrorKind::NotFound`] — never a silent
    /// port `0`.
    pub fn is_not_registered(&self) -> bool {
        matches!(self, Port2Resp::Error)
    }
}

impl Port2Resp {
    /// Encodes the response body (tag + fields, no length prefix).    ///
    /// `Error` is the minimal `'w' + result(1)` with `result != 0`; `Ok` carries
    /// the node fields exactly as `epmd_srv.c` lays them out.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        match self {
            Port2Resp::Ok {
                port,
                node_type,
                protocol,
                high_vsn,
                low_vsn,
                name,
                extra,
            } => {
                w.write_u8(EPMD_PORT2_RESP)?;
                w.write_u8(RESULT_OK)?;
                w.write_u16be(*port)?;
                w.write_u8(*node_type)?;
                w.write_u8(*protocol)?;
                w.write_u16be(*high_vsn)?;
                w.write_u16be(*low_vsn)?;
                write_field_len(w, name.len())?;
                w.write_all(name)?;
                write_field_len(w, extra.len())?;
                w.write_all(extra)
            }
            Port2Resp::Error => {
                w.write_u8(EPMD_PORT2_RESP)?;
                w.write_u8(1)
            }
        }
    }

    /// Decodes a response; a wrong/short tag is `BeamError::BadArg`.
    ///
    /// A non-zero result byte is [`Port2Resp::Error`] (the C server replies with
    /// exactly two bytes); a zero result byte is followed by the node fields,
    /// which must exactly fill the rest of the body.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        if r.u8()? != EPMD_PORT2_RESP {
            return Err(BeamError::BadArg);
        }
        if r.u8()? != RESULT_OK {
            r.finish()?;
            return Ok(Port2Resp::Error);
        }
        let port = r.u16()?;
        let node_type = r.u8()?;
        let protocol = r.u8()?;
        let high_vsn = r.u16()?;
        let low_vsn = r.u16()?;
        let namelen = usize::from(r.u16()?);
        let name = r.take(namelen)?.to_vec();
        let extralen = usize::from(r.u16()?);
        let extra = r.take(extralen)?.to_vec();
        r.finish()?;
        Ok(Port2Resp::Ok { port, node_type, protocol, high_vsn, low_vsn, name, extra })
    }
}

/// The `EPMD_NAMES_REQ` (`'n'`) reply: the epmd port followed by one line per
/// registered node.
///
/// Layout: `epmd_port(4)` then `"name <name> at port <port>\n"` lines (parsed
/// into [`names`](NamesResp::names)).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct NamesResp {
    /// The `epmd` port reported by the server.
    pub epmd_port: u32,
    /// The registered node names.
    pub names: Vec<String>,
}

impl NamesResp {
    /// Encodes the reply body (port + lines).
    ///
    /// The line format is exactly the one the C server emits
    /// (`epmd_srv.c` `case EPMD_NAMES_REQ`): `"name " + <name> + " at port " +
    /// <port> + "\n"`, preceded by the 4-byte big-endian epmd port.
    ///
    /// [`NamesResp`] deliberately retains only the node **names**, not their
    /// listen ports, so `encode` fills the per-line port with
    /// [`epmd_port`](NamesResp::epmd_port); the names round-trip, the ports do
    /// not.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u32be(self.epmd_port)?;
        for name in &self.names {
            let line = format!("name {} at port {}\n", name, self.epmd_port);
            w.write_all(line.as_bytes())?;
        }
        Ok(())
    }

    /// Decodes a reply; a malformed body is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let epmd_port = r.u32()?;
        let text = std::str::from_utf8(r.remaining()).map_err(|_| BeamError::BadArg)?;
        let mut names = Vec::new();
        for line in text.split_inclusive('\n') {
            names.push(parse_names_line(line)?);
        }
        Ok(NamesResp { epmd_port, names })
    }
}

/// Parses one `"name <name> at port <port>\n"` line into `<name>`.
///
/// Every part is mandatory: the trailing newline, the `"name "` prefix, the
/// `" at port "` separator and a non-empty run of ASCII digits for the port.
fn parse_names_line(line: &str) -> Result<String> {
    let line = line.strip_suffix('\n').ok_or(BeamError::BadArg)?;
    let rest = line.strip_prefix("name ").ok_or(BeamError::BadArg)?;
    let (name, port) = rest.rsplit_once(" at port ").ok_or(BeamError::BadArg)?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BeamError::BadArg);
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// The interactive requests/responses (`-dump`, `-kill`, `-stop`).
//
// `epmd` answers three codes that only the CLI (`epmd_cli.c`) uses. `DUMP_REQ`
// is read-only; `KILL_REQ` terminates the daemon and `STOP_REQ` unregisters a
// node, so both are destructive and only codec-covered by the tests.
// ---------------------------------------------------------------------------

/// The `DUMP_REQ` (`'d'`) request: dump `epmd`'s node table.
///
/// Layout: the tag alone — the CLI frames a one-byte body for `-dump`
/// (`epmd_cli.c::epmd_call`: `put_int16(1,buf); buf[2] = what;`). The reply is a
/// [`DumpResp`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct DumpReq;

impl DumpReq {
    /// Encodes the request body (the tag alone, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u8(EPMD_DUMP_REQ)
    }

    /// Decodes a request; the body must be exactly the `'d'` tag (`BadArg`
    /// otherwise — a wrong tag, a short body or trailing bytes).
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        if r.u8()? != EPMD_DUMP_REQ {
            return Err(BeamError::BadArg);
        }
        r.finish()?;
        Ok(DumpReq)
    }
}

/// The `KILL_REQ` (`'k'`) request: terminate the `epmd` daemon.
///
/// Layout: the tag alone (`epmd_cli.c::kill_epmd` frames a one-byte body). The
/// reply is a [`KillResp`]. **Destructive** — it shuts down the host's port
/// mapper.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct KillReq;

impl KillReq {
    /// Encodes the request body (the tag alone, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u8(EPMD_KILL_REQ)
    }

    /// Decodes a request; the body must be exactly the `'k'` tag (`BadArg`
    /// otherwise).
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        if r.u8()? != EPMD_KILL_REQ {
            return Err(BeamError::BadArg);
        }
        r.finish()?;
        Ok(KillReq)
    }
}

/// The `STOP_REQ` (`'s'`) request: unregister the node `name`.
///
/// Layout: `'s' + name` (the CLI frames `strlen(name) + 1` bytes; the name is the
/// packet remainder, `epmd_cli.c::stop_cli`). The server rejects a body whose
/// size is `<= 1` (`epmd_srv.c` "packet too small for request EPMD_STOP_REQ"),
/// i.e. the tag with no name, so such a body does not decode. The reply is a
/// [`StopResp`]. **Destructive** — it de-registers a node (and only in relaxed
/// mode).
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct StopReq {
    /// The node name to unregister (never empty on the wire).
    pub name: Vec<u8>,
}

impl StopReq {
    /// Encodes the request body (tag + name, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u8(EPMD_STOP_REQ)?;
        w.write_all(&self.name)
    }

    /// Decodes a request; a wrong/short tag **or an empty name** is
    /// `BeamError::BadArg`.
    ///
    /// The empty-name rejection mirrors `epmd_srv.c`'s `bsize <= 1` guard: a
    /// `STOP_REQ` carrying only its tag is not a legal request.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        if r.u8()? != EPMD_STOP_REQ {
            return Err(BeamError::BadArg);
        }
        let name = r.remaining().to_vec();
        if name.is_empty() {
            return Err(BeamError::BadArg);
        }
        Ok(StopReq { name })
    }
}

/// Whether a [`DumpLine`] describes a live node or one `epmd` remembers as
/// old/unused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DumpNodeKind {
    /// A currently registered node (`active name     <...>`).
    Active,
    /// A node `epmd` remembers only as old/unused (`old/unused name <...>`).
    OldUnused,
}

/// One node line of a [`DumpResp`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DumpLine {
    /// Whether the node is live or merely remembered.
    pub kind: DumpNodeKind,
    /// The node's symbolic name (the text between the `<` and `>`).
    pub name: Vec<u8>,
    /// The node's listen port.
    pub port: u16,
    /// The file descriptor `epmd` reports for the node's connection.
    pub fd: u32,
}

impl DumpLine {
    /// Whether this line is a currently registered (active) node.
    pub fn is_active(&self) -> bool {
        self.kind == DumpNodeKind::Active
    }
}

/// The `DUMP_REQ` reply: the `epmd` port followed by one diagnostic text line
/// per node.
///
/// Layout (`epmd_srv.c` `case EPMD_DUMP_REQ`): `port(4)` (big-endian), then per
/// node a line **plus the NUL terminator `erts_snprintf` leaves in the reply
/// buffer** (the server counts it in `len += r + 1`). An active node is
/// `active name     <symname> at port N, fd = N\n`; an old/unused one is
/// `old/unused name <symname>, port = N, fd = N \n` (with a space before the
/// newline). The text is, per the source comment, "parsed by erl_epmd.erl", so
/// the shapes are wire-frozen.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DumpResp {
    /// The `epmd` port reported by the server.
    pub port: u32,
    /// One entry per node `epmd` reports (active nodes first, then old/unused).
    pub lines: Vec<DumpLine>,
}

impl DumpResp {
    /// Encodes the reply body (port + lines), reproducing the server's bytes
    /// including the trailing NUL after each line.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u32be(self.port)?;
        for line in &self.lines {
            let suffix = match line.kind {
                DumpNodeKind::Active => format!("> at port {}, fd = {}\n", line.port, line.fd),
                DumpNodeKind::OldUnused => format!(">, port = {}, fd = {} \n", line.port, line.fd),
            };
            w.write_all(match line.kind {
                DumpNodeKind::Active => DUMP_ACTIVE_PREFIX.as_bytes(),
                DumpNodeKind::OldUnused => DUMP_OLD_PREFIX.as_bytes(),
            })?;
            w.write_all(&line.name)?;
            w.write_all(suffix.as_bytes())?;
            // The C server's `len += r + 1` also writes the snprintf NUL.
            w.write_u8(0)?;
        }
        Ok(())
    }

    /// Decodes a reply; a body that is not `port(4)` plus well-formed lines is
    /// `BeamError::BadArg`.
    ///
    /// Each line must end in `\n`; the single NUL the C server writes after it is
    /// consumed when present (it is an artefact of the reply buffer, not a field),
    /// so a reply from an implementation that omits it still parses.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut r = BodyReader::new(body);
        let port = r.u32()?;
        let mut rest = r.remaining();
        let mut lines = Vec::new();
        while !rest.is_empty() {
            let nl = rest.iter().position(|&b| b == b'\n').ok_or(BeamError::BadArg)?;
            lines.push(parse_dump_line(&rest[..=nl])?);
            let mut next = nl + 1;
            if rest.get(next) == Some(&0) {
                next += 1;
            }
            rest = &rest[next..];
        }
        Ok(DumpResp { port, lines })
    }

    /// The active (currently registered) node lines.
    pub fn active(&self) -> impl Iterator<Item = &DumpLine> {
        self.lines.iter().filter(|line| line.is_active())
    }
}

/// Parses one `DUMP_RESP` node line into its typed form.
///
/// The two shapes are exactly the C `copy_str`/`erts_snprintf` sequences of
/// `epmd_srv.c`; every part is mandatory (`BadArg` otherwise).
fn parse_dump_line(line: &[u8]) -> Result<DumpLine> {
    let text = std::str::from_utf8(line).map_err(|_| BeamError::BadArg)?;
    if let Some(rest) = text.strip_prefix(DUMP_ACTIVE_PREFIX) {
        let (name, rest) = rest.split_once('>').ok_or(BeamError::BadArg)?;
        let rest = rest.strip_prefix(" at port ").ok_or(BeamError::BadArg)?;
        let rest = rest.strip_suffix('\n').ok_or(BeamError::BadArg)?;
        let (port, fd) = rest.split_once(", fd = ").ok_or(BeamError::BadArg)?;
        Ok(DumpLine {
            kind: DumpNodeKind::Active,
            name: name.as_bytes().to_vec(),
            port: parse_decimal_field(port)?,
            fd: parse_decimal_field(fd)?,
        })
    } else if let Some(rest) = text.strip_prefix(DUMP_OLD_PREFIX) {
        let (name, rest) = rest.split_once('>').ok_or(BeamError::BadArg)?;
        let rest = rest.strip_prefix(", port = ").ok_or(BeamError::BadArg)?;
        let rest = rest.strip_suffix('\n').ok_or(BeamError::BadArg)?;
        // The old/unused line's format string ends in a space before the newline.
        let rest = rest.strip_suffix(' ').unwrap_or(rest);
        let (port, fd) = rest.split_once(", fd = ").ok_or(BeamError::BadArg)?;
        Ok(DumpLine {
            kind: DumpNodeKind::OldUnused,
            name: name.as_bytes().to_vec(),
            port: parse_decimal_field(port)?,
            fd: parse_decimal_field(fd)?,
        })
    } else {
        Err(BeamError::BadArg)
    }
}

/// Parses a non-empty run of ASCII decimal digits (`BadArg` otherwise, including
/// on overflow), used for a dump line's port and fd fields.
fn parse_decimal_field<T: std::str::FromStr>(text: &str) -> Result<T> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(BeamError::BadArg);
    }
    text.parse::<T>().map_err(|_| BeamError::BadArg)
}

/// The `KILL_REQ` reply: the literal 2-byte `"OK"`/`"NO"`.
///
/// `epmd_srv.c` `case EPMD_KILL_REQ` answers `"NO"` (2 bytes) while live nodes are
/// registered and brutal kill was not enabled, and `"OK"` (2 bytes) otherwise —
/// after which the daemon terminates. The OTP suite pins both
/// (`epmd_SUITE.erl::no_live_killing`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KillResp {
    /// `"OK"` — the kill is permitted (and `epmd` exits).
    Ok,
    /// `"NO"` — refused because nodes are still registered.
    Refused,
}

impl KillResp {
    /// Encodes the reply body (the literal 2-byte text, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_all(match self {
            KillResp::Ok => b"OK".as_slice(),
            KillResp::Refused => b"NO".as_slice(),
        })
    }

    /// Decodes a reply; anything but the literal `"OK"`/`"NO"` is
    /// `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body == b"OK" {
            Ok(KillResp::Ok)
        } else if body == b"NO" {
            Ok(KillResp::Refused)
        } else {
            Err(BeamError::BadArg)
        }
    }

    /// Whether the reply permits the kill.
    pub fn is_ok(&self) -> bool {
        matches!(self, KillResp::Ok)
    }
}

/// The `STOP_REQ` reply.
///
/// `epmd_srv.c` `case EPMD_STOP_REQ` answers `"STOPPED"` (7 bytes) when the node
/// was found and unregistered (its connection is closed), or `"NOEXIST"` (7
/// bytes) when no such node was registered; the CLI client reads exactly seven
/// bytes and both are pinned by the OTP suite
/// (`epmd_SUITE.erl::unregister_others_name_1/2`).
///
/// When `-relaxed_command_check` is not in effect — or the request arrives from a
/// non-loopback peer — the server replies with **nothing at all** and closes the
/// connection (`epmd_srv.c` returns early; the suite observes `closed =
/// recv(Sock2,1)`). That outcome is modelled honestly as [`StopResp::NoReply`]
/// rather than invented.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StopResp {
    /// `"STOPPED"` — the node was unregistered and its connection closed.
    Stopped,
    /// `"NOEXIST"` — no node with that name was registered.
    NoExist,
    /// No reply at all: `STOP_REQ` is only answered in relaxed mode from a local
    /// peer; otherwise `epmd` closes the connection without answering.
    NoReply,
}

impl StopResp {
    /// Encodes the reply body (`Stopped`/`NoExist` are the literal 7-byte text;
    /// `NoReply` encodes to **no** bytes, since the server sends nothing).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_all(match self {
            StopResp::Stopped => b"STOPPED".as_slice(),
            StopResp::NoExist => b"NOEXIST".as_slice(),
            StopResp::NoReply => &[],
        })
    }

    /// Decodes a reply; an empty body is [`StopResp::NoReply`] (the server sent
    /// nothing), and anything but the two literal texts is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.is_empty() {
            Ok(StopResp::NoReply)
        } else if body == b"STOPPED" {
            Ok(StopResp::Stopped)
        } else if body == b"NOEXIST" {
            Ok(StopResp::NoExist)
        } else {
            Err(BeamError::BadArg)
        }
    }

    /// Whether the node was actually unregistered.
    pub fn is_stopped(&self) -> bool {
        matches!(self, StopResp::Stopped)
    }
}/// The `epmd` client: drives request/response exchanges over a supplied
/// [`Transport`].
///
/// It performs no connection setup itself — the caller supplies an already
/// connected transport (produced by `beam_io`). Every request is 2-byte framed
/// and its reply is read back through the same transport.
#[derive(Debug)]
pub struct EpmdClient<T: Transport> {
    /// The transport the client reads and writes through.
    transport: T,
}

impl<T: Transport> EpmdClient<T> {
    /// Wraps an already connected transport.
    pub fn new(transport: T) -> EpmdClient<T> {
        EpmdClient { transport }
    }

    /// Sends an [`Alive2Req`] and reads the [`Alive2Resp`].
    ///
    /// The request is 2-byte framed; the reply is **unframed** (see the module
    /// docs) and self-delimiting by its `'y'`/`'v'` tag.
    pub fn publish(&mut self, req: &Alive2Req) -> Result<Alive2Resp> {
        let mut buf = BodyBuf::new();
        req.encode(&mut buf)?;
        write_packet(&mut self.transport, PacketHeaderSize::Two, &buf.into_bytes())?;
        read_alive2_resp(&mut self.transport)
    }

    /// Looks up a node name (`PORT2_REQ`/`PORT2_RESP`).
    ///
    /// The request is `'z' + name` — the name is the *remainder* of the packet, with
    /// no length field of its own (only the 2-byte frame length, which is
    /// `1 + name.len()`) — and the reply is **unframed** (see the module docs).
    pub fn port_please(&mut self, name: &[u8]) -> Result<Port2Resp> {
        let req = Port2Req { name: name.to_vec() };
        let mut buf = BodyBuf::new();
        req.encode(&mut buf)?;
        write_packet(&mut self.transport, PacketHeaderSize::Two, &buf.into_bytes())?;
        read_port2_resp(&mut self.transport)
    }

    /// Lists the registered node names (`NAMES_REQ`).
    ///
    /// The request is the single byte [`EPMD_NAMES_REQ`] (`'n'`), 2-byte framed;
    /// the reply is decoded as a [`NamesResp`]. The reply itself is **unframed** and
    /// open-ended, so the body is read until the transport has no more bytes (see
    /// [`read_names_resp`]).
    pub fn names(&mut self) -> Result<NamesResp> {
        let body = [EPMD_NAMES_REQ];
        write_packet(&mut self.transport, PacketHeaderSize::Two, &body)?;
        read_names_resp(&mut self.transport)
    }

    /// Dumps `epmd`'s node table (`DUMP_REQ`/`DumpResp`).
    ///
    /// The request is the single byte [`EPMD_DUMP_REQ`] (`'d'`), 2-byte framed.
    /// This is **read-only** and safe to issue against a machine's live `epmd`
    /// (the server only requires a loopback peer). The reply announces no length
    /// and `epmd` keeps the socket open, so the body is read until the transport
    /// runs out of bytes, bounded by [`MAX_DUMP_RESP_BODY`] (see
    /// [`read_dump_resp`]).
    pub fn dump(&mut self) -> Result<DumpResp> {
        let body = [EPMD_DUMP_REQ];
        write_packet(&mut self.transport, PacketHeaderSize::Two, &body)?;
        read_dump_resp(&mut self.transport)
    }

    /// Asks `epmd` to terminate (`KILL_REQ`/`KillResp`).
    ///
    /// The request is the single byte [`EPMD_KILL_REQ`] (`'k'`), 2-byte framed;
    /// the reply is the literal 2-byte `"OK"`/`"NO"`. **Destructive**: an `"OK"`
    /// shuts down the host's port mapper (which is why the test suite never sends
    /// it to a real `epmd`).
    pub fn kill(&mut self) -> Result<KillResp> {
        let body = [EPMD_KILL_REQ];
        write_packet(&mut self.transport, PacketHeaderSize::Two, &body)?;
        read_kill_resp(&mut self.transport)
    }
    /// Asks `epmd` to unregister the node `name` (`STOP_REQ`/`StopResp`).
    ///
    /// The request is `'s' + name` (the name is the packet remainder, so the frame
    /// length is `1 + name.len()`). **Destructive** — it de-registers a node — and
    /// only answered in relaxed mode from a local peer; otherwise the reply is
    /// [`StopResp::NoReply`] (see [`read_stop_resp`]).
    pub fn stop(&mut self, name: &[u8]) -> Result<StopResp> {
        let req = StopReq { name: name.to_vec() };
        let mut buf = BodyBuf::new();
        req.encode(&mut buf)?;
        write_packet(&mut self.transport, PacketHeaderSize::Two, &buf.into_bytes())?;
        read_stop_resp(&mut self.transport)
    }
}
// ---------------------------------------------------------------------------
// Response framing: an `epmd` reply is NOT length-prefixed.
// ---------------------------------------------------------------------------

/// Reads `n` more bytes from `r` and appends them to `body`.
fn read_more<R: Read>(r: &mut R, n: usize, body: &mut Vec<u8>) -> Result<()> {
    for _ in 0..n {
        body.push(r.read_u8()?);
    }
    Ok(())
}

/// Reads a big-endian `u16` and appends its two bytes to `body`.
fn read_u16be<R: Read>(r: &mut R, body: &mut Vec<u8>) -> Result<u16> {
    let value = r.read_be_u16()?;
    body.extend_from_slice(&value.to_be_bytes());
    Ok(value)
}

/// Reads one complete, **unframed** `ALIVE2_RESP`: `tag + result(1) + creation(2|4)`
/// (`epmd_srv.c:849-859`). The tag is read first because it *is* the width
/// selector: [`EPMD_ALIVE2_RESP`] (`'y'`) carries a 16-bit creation,
/// [`EPMD_ALIVE2_X_RESP`] (`'v'`) — what a v6 requester always gets — a 32-bit one.
fn read_alive2_resp<R: Read>(r: &mut R) -> Result<Alive2Resp> {
    let mut body = Vec::with_capacity(6);
    read_more(r, 2, &mut body)?; // tag + result
    let creation_len = match body.first().copied() {
        Some(EPMD_ALIVE2_RESP) => 2,
        Some(EPMD_ALIVE2_X_RESP) => 4,
        _ => return Err(BeamError::BadArg),
    };
    read_more(r, creation_len, &mut body)?;
    Alive2Resp::decode(&body)
}

/// Reads one complete, **unframed** `PORT2_RESP`: `'w' + result(1)` and, on success,
/// `port(2) node_type(1) protocol(1) high_vsn(2) low_vsn(2) namelen(2) name
/// extralen(2) extra` (`epmd_srv.c:904-936`).
///
/// A non-zero result is the two-byte failure reply — there is no name block to
/// read — while a zero result is followed by the self-delimiting node block (its
/// `namelen`/`extralen` fields fix the two variable parts).
fn read_port2_resp<R: Read>(r: &mut R) -> Result<Port2Resp> {
    let mut body = Vec::with_capacity(32);
    read_more(r, 2, &mut body)?; // 'w' + result
    match (body.first().copied(), body.get(1).copied()) {
        (Some(EPMD_PORT2_RESP), Some(RESULT_OK)) => {}
        (Some(EPMD_PORT2_RESP), Some(_)) => return Port2Resp::decode(&body),
        _ => return Err(BeamError::BadArg),
    }
    read_more(r, 4, &mut body)?; // port(2) node_type(1) protocol(1)
    read_more(r, 4, &mut body)?; // high_vsn(2) low_vsn(2)
    let namelen = usize::from(read_u16be(r, &mut body)?);
    read_more(r, namelen, &mut body)?;
    let extralen = usize::from(read_u16be(r, &mut body)?);
    read_more(r, extralen, &mut body)?;
    Port2Resp::decode(&body)
}

/// Whether an I/O failure just means "the peer has no more bytes *for now*" (as
/// opposed to a real failure): a non-blocking read with nothing available, or a
/// socket read timeout.
fn is_quiet(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::UnexpectedEof
    )
}

/// Reads a complete, **unframed** `NAMES_RESP`: `epmd_port(4)` followed by one
/// `"name <name> at port <port>\n"` line per registered node
/// (`epmd_srv.c:941-978`).
///
/// The reply announces no length and is answered on a non-`keep` connection that
/// `epmd` closes immediately afterwards (only a successful `ALIVE2_REQ` sets
/// `s->keep`; `epmd_srv.c::do_read` closes the connection when it is not set), so
/// the end of the body is normally EOF — a short read. A peer that keeps the
/// socket open instead ends it at the first quiet read ([`is_quiet`]).
fn read_names_resp<R: Read>(r: &mut R) -> Result<NamesResp> {    let mut body = Vec::new();
    loop {
        match r.read_u8() {
            Ok(byte) => body.push(byte),
            Err(BeamError::BadArg) => break,
            Err(BeamError::Io(err)) if is_quiet(err.kind()) => break,
            Err(err) => return Err(err),
        }
    }
    NamesResp::decode(&body)
}

/// Reads a complete, **unframed** `DUMP_RESP`: `port(4)` followed by the
/// diagnostic node lines (`epmd_srv.c:981-1045`).
///
/// Like the NAMES reply, the dump announces no length and is written on a
/// non-`keep` connection that `epmd` closes right after it, so the body normally
/// ends at EOF (a short read); a peer that holds the socket open instead ends it
/// at the first quiet read ([`is_quiet`]). Because the body is open-ended it is
/// **bounded**: buffering past [`MAX_DUMP_RESP_BODY`] yields
/// [`BeamError::SystemLimit`] instead of unbounded allocation.
fn read_dump_resp<R: Read>(r: &mut R) -> Result<DumpResp> {    let mut body = Vec::new();
    loop {
        match r.read_u8() {
            Ok(byte) => {
                if body.len() >= MAX_DUMP_RESP_BODY {
                    return Err(BeamError::SystemLimit);
                }
                body.push(byte);
            }
            Err(BeamError::BadArg) => break,
            Err(BeamError::Io(err)) if is_quiet(err.kind()) => break,
            Err(err) => return Err(err),
        }
    }
    DumpResp::decode(&body)
}

/// Reads a complete, **unframed** `KILL_RESP`: the literal two bytes
/// `"OK"`/`"NO"` (`epmd_srv.c` `case EPMD_KILL_REQ`; `epmd_cli.c::kill_epmd`
/// `read_fill(fd,buf,2)`).
///
/// A peer that is not local is answered with **no** bytes and a closed
/// connection (`epmd_srv.c` returns before replying), so a short read is a typed
/// [`BeamError::BadArg`] rather than a wrong verdict.
fn read_kill_resp<R: Read>(r: &mut R) -> Result<KillResp> {
    let mut body = Vec::with_capacity(2);
    read_more(r, 2, &mut body)?;
    KillResp::decode(&body)
}

/// Reads a complete, **unframed** `STOP_RESP`: the literal 7 bytes `"STOPPED"` /
/// `"NOEXIST"` (`epmd_srv.c` `case EPMD_STOP_REQ`; `epmd_cli.c::stop_cli`
/// `read_fill(fd,buf,7)`).
///
/// `STOP_REQ` is only answered in relaxed mode from a local peer; otherwise the
/// server sends **nothing** and closes the connection. End-of-stream (or a read
/// timeout) before the first byte is therefore [`StopResp::NoReply`] — the honest
/// "the daemon declined without answering" outcome, not an invented reply.
fn read_stop_resp<R: Read>(r: &mut R) -> Result<StopResp> {
    let mut body = Vec::with_capacity(STOP_RESP_LEN);
    match r.read_u8() {
        Ok(byte) => body.push(byte),
        Err(BeamError::BadArg) => return Ok(StopResp::NoReply),
        Err(BeamError::Io(err)) if is_quiet(err.kind()) => return Ok(StopResp::NoReply),
        Err(err) => return Err(err),
    }
    read_more(r, STOP_RESP_LEN - 1, &mut body)?;
    StopResp::decode(&body)
}
// ---------------------------------------------------------------------------
// The concrete `epmd` path: a `TcpStream` transport plus the one-shot lookups.
// ---------------------------------------------------------------------------

/// The default `epmd` TCP address: loopback and the well-known
/// [`EPMD_PORT`](crate::EPMD_PORT) (`4369`).
pub const EPMD_DEFAULT_ADDR: &str = "127.0.0.1:4369";

impl EpmdClient<TcpTransport> {
    /// Connects to `epmd` at `addr` (a `host:port` pair; the well-known address is
    /// [`EPMD_DEFAULT_ADDR`]), returning a client usable for
    /// [`publish`](EpmdClient::publish) / [`port_please`](EpmdClient::port_please) /
    /// [`names`](EpmdClient::names).
    ///
    /// [`TcpTransport::connect`] sets a read timeout
    /// ([`DEFAULT_READ_TIMEOUT`](crate::connection::DEFAULT_READ_TIMEOUT)) and
    /// `TCP_NODELAY`, so a stalled or absent `epmd` can never hang a caller: the
    /// failure surfaces as a typed [`BeamError::Io`].
    ///
    /// Keeping the returned client alive keeps the connection — and, for a
    /// [`publish`](EpmdClient::publish)ed node, its `epmd` registration — alive:
    /// `epmd` de-registers a node when its `ALIVE2` socket closes
    /// (`epmd_srv.c:641-645`).
    pub fn connect(addr: &str) -> Result<EpmdClient<TcpTransport>> {
        Ok(EpmdClient::new(TcpTransport::connect(addr)?))
    }
}

/// Connects to `epmd` at `addr` (a `host:port` pair; the well-known address is
/// [`EPMD_DEFAULT_ADDR`]) and returns a client for the lookup protocol.
///
/// The free-function form of [`EpmdClient::connect`], mirroring the [`lookup`] and
/// [`lookup_port`] helpers: callers that want the local `epmd` need not name the
/// [`crate::connection::TcpTransport`] transport themselves. The returned client's
/// lifetime is the connection's (and, for a [`publish`](EpmdClient::publish)ed
/// node, its `epmd` registration's) lifetime — see [`EpmdClient::connect`].
pub fn connect(addr: &str) -> Result<EpmdClient<TcpTransport>> {
    EpmdClient::connect(addr)
}

/// Looks a node's registration up on the local `epmd` ([`EPMD_DEFAULT_ADDR`]),
/// returning the raw [`Port2Resp`].
///
/// A fresh connection is opened for the lookup (and closed when it is dropped);
/// a name that is not registered yields [`Port2Resp::Error`] — the reply `epmd`
/// itself sends (`epmd_srv.c:930`) — rather than a transport error. Use
/// [`lookup_port`] when only the port is wanted.
///
/// This is [`lookup_at`] pinned to [`EPMD_DEFAULT_ADDR`]; use it to resolve a
/// name against an `epmd` listening elsewhere (e.g. one an `erl` node was pointed
/// at with `-epmd_port N`).
pub fn lookup(name: &[u8]) -> Result<Port2Resp> {
    lookup_at(EPMD_DEFAULT_ADDR, name)
}

/// The listen port of the node registered as `name` on the local `epmd`
/// ([`EPMD_DEFAULT_ADDR`]).
///
/// This is [`lookup`] reduced to its port (`port_please`), i.e. the first step of
/// connecting to a published node ([`crate::connection::connect_to_node`]). An
/// unregistered name is a typed [`BeamError::Io`] with
/// [`std::io::ErrorKind::NotFound`] — never a panic and never a silent `0`.
///
/// This is [`lookup_port_at`] pinned to [`EPMD_DEFAULT_ADDR`].
pub fn lookup_port(name: &[u8]) -> Result<u16> {
    lookup_port_at(EPMD_DEFAULT_ADDR, name)
}

/// Looks a node's registration up on the `epmd` at `addr` (a `host:port` pair),
/// returning the raw [`Port2Resp`].
///
/// The address-taking form of [`lookup`]: [`EpmdClient::connect`] reaches any
/// `epmd`, so a name can be resolved against a daemon that is **not** on the
/// well-known [`EPMD_DEFAULT_ADDR`] — for instance the private `epmd` a node was
/// started against with `erl -epmd_port N` (`erl_epmd:get_epmd_port/0`), or the
/// in-process stub in `tests/hermetic_epmd.rs`. A fresh connection is opened for
/// the lookup (and closed when it is dropped); a name that is not registered
/// yields [`Port2Resp::Error`] rather than a transport error. Use
/// [`lookup_port_at`] when only the port is wanted.
pub fn lookup_at(addr: &str, name: &[u8]) -> Result<Port2Resp> {
    EpmdClient::connect(addr)?.port_please(name)
}

/// The listen port of the node registered as `name` on the `epmd` at `addr` (a
/// `host:port` pair).
///
/// The address-taking form of [`lookup_port`]: [`lookup_at`] reduced to its port,
/// so a node published on a private `epmd` (e.g. `erl -epmd_port N`) can be
/// resolved. An unregistered name is a typed [`BeamError::Io`] with
/// [`std::io::ErrorKind::NotFound`] — never a panic and never a silent `0`.
pub fn lookup_port_at(addr: &str, name: &[u8]) -> Result<u16> {
    match lookup_at(addr, name)? {
        Port2Resp::Ok { port, .. } => Ok(port),
        Port2Resp::Error => Err(BeamError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("epmd: node \"{}\" is not registered", String::from_utf8_lossy(name)),
        ))),
    }
}
