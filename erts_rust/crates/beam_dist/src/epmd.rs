//! The `epmd` port-mapper client protocol.
//!
//! Ground truth: `lib/erl_interface/src/epmd/epmd_publish.c` + `epmd_port.c` +
//! `ei_epmd.h` (client wire) and `erts/epmd/src/epmd_srv.c` + `epmd_int.h`
//! (server side). All messages are 2-byte big-endian length-framed
//! ([`PacketHeaderSize::Two`](crate::framing::PacketHeaderSize::Two)); the client
//! drives them over any [`Transport`], so the whole protocol is exercised against
//! [`MemoryTransport`](crate::transport::MemoryTransport) with no sockets.

use beam_common::Result;

use crate::flags::{DIST_VER_HIGH, DIST_VER_LOW};
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
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes a request; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
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
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes a response; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
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
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes a request; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
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
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes a response; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
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
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes a reply; a malformed body is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
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
        todo!("M1")
    }

    /// Looks up a node name (`PORT2_REQ`/`PORT2_RESP`).
    pub fn port_please(&mut self, name: &[u8]) -> Result<Port2Resp> {
        todo!("M1")
    }

    /// Lists the registered node names (`NAMES_REQ`).
    pub fn names(&mut self) -> Result<NamesResp> {
        todo!("M1")
    }
}
