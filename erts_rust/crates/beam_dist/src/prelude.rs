//! The conventional glob-import surface for downstream crates.
//!
//! `use beam_dist::prelude::*;` brings the distribution vocabulary (flags,
//! opcodes, handshake/epmd tags), the message codecs, the framing helpers, the
//! transport seam and the `epmd` client into scope.
//!
//! [`crate::transport::Read`] and [`crate::transport::Write`] are deliberately
//! **not** glob-exported — they would shadow `std::io::{Read, Write}`.

pub use crate::control::{decode_control, encode_control, encode_control_cached, ControlMessage};
pub use crate::dist_header::*;
pub use crate::driver::{ConnectionDriver, DriverState, StepOutcome};
pub use crate::epmd::*;
pub use crate::epmd::{connect, lookup, lookup_port, EPMD_DEFAULT_ADDR};
pub use crate::flags::*;
pub use crate::framing::{is_tick, read_packet, write_packet, write_tick, PacketHeaderSize};
pub use crate::handshake::*;
pub use crate::transport::{MemoryTransport, Transport};

pub use crate::atom_cache::{OutboundAtomCache, ReceiveAtomCache};
pub use crate::auth::{gen_challenge, gen_digest, md5, Md5Digest};
pub use crate::connection::{
    accept_server, connect_client, connect_client_with_creation, connect_to_node,
    connect_to_node_with_creation, Connection, TcpListener, TcpTransport, CLIENT_CREATION,
    DEFAULT_READ_TIMEOUT,
};
pub use crate::fragment::{
    Assembly, FragmentAssembler, FragmentCont, FragmentHeader, DIST_FRAGMENT_SIZE,
};

pub use crate::flags::{DistFlags, Dop};
