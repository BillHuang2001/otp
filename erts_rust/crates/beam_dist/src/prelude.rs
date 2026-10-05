//! The conventional glob-import surface for downstream crates.
//!
//! `use beam_dist::prelude::*;` brings the distribution vocabulary (flags,
//! opcodes, handshake/epmd tags), the message codecs, the framing helpers, the
//! transport seam and the `epmd` client into scope.
//!
//! [`crate::transport::Read`] and [`crate::transport::Write`] are deliberately
//! **not** glob-exported — they would shadow `std::io::{Read, Write}`.

pub use crate::control::{decode_control, encode_control, ControlMessage};
pub use crate::dist_header::*;
pub use crate::epmd::*;
pub use crate::flags::*;
pub use crate::framing::{is_tick, read_packet, write_packet, write_tick, PacketHeaderSize};
pub use crate::handshake::*;
pub use crate::transport::{MemoryTransport, Transport};

pub use crate::flags::{DistFlags, Dop};
