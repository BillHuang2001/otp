//! Packet-length framing: the 2- and 4-byte big-endian length prefixes that wrap
//! every distribution and `epmd` packet.
//!
//! Ground truth: `erts/emulator/beam/packet_parser.c` and `dist.c` (a **tick** is
//! a *zero-length* packet). Every length field is big-endian; an advertised
//! length beyond the negotiated ceiling is a `BeamError::SystemLimit` (never a
//! blind allocation).

use beam_common::Result;

use crate::transport::{Read, Write};

/// The width of the packet-length prefix.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PacketHeaderSize {
    /// A 2-byte big-endian length (the distribution handshake and `epmd`).
    Two,
    /// A 4-byte big-endian length (established distribution connections).
    Four,
}

impl PacketHeaderSize {
    /// The prefix width in bytes (`2` or `4`).
    pub fn bytes(&self) -> usize {
        match self {
            PacketHeaderSize::Two => 2,
            PacketHeaderSize::Four => 4,
        }
    }

    /// Reads the big-endian length prefix.
    pub fn read<R: Read>(&self, r: &mut R) -> Result<usize> {
        match self {
            PacketHeaderSize::Two => Ok(usize::from(r.read_be_u16()?)),
            PacketHeaderSize::Four => Ok(r.read_be_u32()? as usize),
        }
    }

    /// Writes the big-endian length prefix for a body of `len` bytes.
    pub fn write<W: Write>(&self, w: &mut W, len: usize) -> Result<()> {
        match self {
            PacketHeaderSize::Two => w.write_u16be(len as u16),
            PacketHeaderSize::Four => w.write_u32be(len as u32),
        }
    }
}

/// The largest body a 4-byte length prefix can describe (`u32::MAX`); a 2-byte
/// prefix is limited to `0xffff`. A longer advertised length is a
/// `BeamError::SystemLimit`.
pub const MAX_PACKET_BODY: usize = u32::MAX as usize;

/// Reads a length prefix, rejecting an advertised length beyond the ceiling of
/// `hdr` with `BeamError::SystemLimit`.
pub fn read_length<R: Read>(r: &mut R, hdr: PacketHeaderSize) -> Result<usize> {
    todo!("M1")
}

/// Reads a length-framed packet body.
///
/// A zero-length packet is a **tick** and yields an empty `Vec`.
pub fn read_packet<R: Read>(r: &mut R, hdr: PacketHeaderSize) -> Result<Vec<u8>> {
    todo!("M1")
}

/// Writes `body` with its length prefix.
pub fn write_packet<W: Write>(w: &mut W, hdr: PacketHeaderSize, body: &[u8]) -> Result<()> {
    todo!("M1")
}

/// Writes a tick: a length prefix of `0` (a zero-length packet).
pub fn write_tick<W: Write>(w: &mut W, hdr: PacketHeaderSize) -> Result<()> {
    todo!("M1")
}

/// Whether `body` is a tick (an empty packet).
pub fn is_tick(body: &[u8]) -> bool {
    body.is_empty()
}
