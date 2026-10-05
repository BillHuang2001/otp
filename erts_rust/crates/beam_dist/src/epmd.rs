//! The `epmd` port-mapper client protocol.
//!
//! Ground truth: `lib/erl_interface/src/epmd/epmd_publish.c` + `epmd_port.c` +
//! `ei_epmd.h` (client wire) and `erts/epmd/src/epmd_srv.c` + `epmd_int.h`
//! (server side). All messages are 2-byte big-endian length-framed
//! ([`PacketHeaderSize::Two`](crate::framing::PacketHeaderSize::Two)); the client
//! drives them over any [`Transport`], so the whole protocol is exercised against
//! [`MemoryTransport`](crate::transport::MemoryTransport) with no sockets.

use beam_common::{BeamError, Result};

use crate::flags::{DIST_VER_HIGH, DIST_VER_LOW};
use crate::framing::{read_packet, write_packet, PacketHeaderSize};
use crate::transport::{Transport, Write};

pub use crate::flags::{
    EPMD_ALIVE2_REQ, EPMD_ALIVE2_RESP, EPMD_ALIVE2_X_RESP, EPMD_NAMES_REQ, EPMD_NODE_HIDDEN,
    EPMD_NODE_NORMAL, EPMD_PORT2_REQ, EPMD_PORT2_RESP,
};

/// `EI_MYPROTO`: the protocol field of an `ALIVE2_REQ` (`0` = tcp/ip).
pub const EPMD_PROTO_TCP: u8 = 0;

/// The highest distribution version advertised to `epmd` (`EI_DIST_HIGH`).
pub const EPMD_DIST_HIGH: u16 = DIST_VER_HIGH;

/// The lowest distribution version advertised to `epmd` (`EI_DIST_LOW`).
pub const EPMD_DIST_LOW: u16 = DIST_VER_LOW;

/// The `result` byte meaning "success" in an `ALIVE2_RESP`/`PORT2_RESP`.
const RESULT_OK: u8 = 0;

/// A cursor over an already-received message body.
///
/// Every reader is bounds-checked: running past the end of the body (a truncated
/// message) yields `BeamError::BadArg` — never a panic.
struct BodyReader<'a> {
    /// The whole message body.
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
}

/// The `PORT2_REQ` (`'z'`) port-lookup request.
///
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
    /// Encodes the response body (tag + fields, no length prefix).
    ///
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

/// The `epmd` client: drives request/response exchanges over a supplied
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
    pub fn publish(&mut self, req: &Alive2Req) -> Result<Alive2Resp> {
        let mut buf = BodyBuf::new();
        req.encode(&mut buf)?;
        write_packet(&mut self.transport, PacketHeaderSize::Two, &buf.into_bytes())?;
        let reply = read_packet(&mut self.transport, PacketHeaderSize::Two)?;
        Alive2Resp::decode(&reply)
    }

    /// Looks up a node name (`PORT2_REQ`/`PORT2_RESP`).
    pub fn port_please(&mut self, name: &[u8]) -> Result<Port2Resp> {
        let req = Port2Req { name: name.to_vec() };
        let mut buf = BodyBuf::new();
        req.encode(&mut buf)?;
        write_packet(&mut self.transport, PacketHeaderSize::Two, &buf.into_bytes())?;
        let reply = read_packet(&mut self.transport, PacketHeaderSize::Two)?;
        Port2Resp::decode(&reply)
    }

    /// Lists the registered node names (`NAMES_REQ`).
    ///
    /// The request is the single byte [`EPMD_NAMES_REQ`] (`'n'`), 2-byte framed;
    /// the reply is decoded as a [`NamesResp`].
    pub fn names(&mut self) -> Result<NamesResp> {
        let body = [EPMD_NAMES_REQ];
        write_packet(&mut self.transport, PacketHeaderSize::Two, &body)?;
        let reply = read_packet(&mut self.transport, PacketHeaderSize::Two)?;
        NamesResp::decode(&reply)
    }
}
