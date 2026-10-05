#![doc = "Erlang distribution protocol v6: handshake, DFLAG_*/DOP_* control messages and epmd."]
#![deny(unsafe_code)]
#![warn(missing_docs)]

//! Distribution protocol v6 (`DFLAG_*` / `DOP_*`) and the `epmd` port-mapper
//! client — compatibility contract Part I §A.5 (the distribution wire) and the
//! Stage-0 foundations of Part II §H.
//!
//! The core is **pure protocol/value code over the abstract
//! [`transport`] seam** — the codecs never open a socket themselves. The
//! [`connection`] module adds a concrete `std::net::TcpStream` adapter and
//! drives a live distribution link end-to-end (handshake, `DOP_*` messages,
//! fragmentation), so the runtime has a usable API; the production transport is
//! still owned by `beam_io`. The crate owns the *distribution-specific* wire —
//! the flag/opcode vocabulary ([`flags`]), the connect/challenge handshake
//! ([`handshake`]) and its MD5 authentication ([`auth`]), the packet-length
//! framing ([`framing`]), the distribution-header envelope ([`dist_header`]),
//! message fragmentation ([`fragment`]), the atom-cache/segment machinery
//! ([`atom_cache`]) and the control-message codec ([`control`]). Term
//! encoding/decoding is **never re-implemented here**: every term crosses the
//! wire through `beam_codec` (ETF).
//!
//! The crate is **safe Rust** (`#![deny(unsafe_code)]`); every fallible API
//! returns `beam_common::Result` and no function panics on wire data.
//!
//! Ground truth for every byte is the legacy C (read, never modified):
//! `erts/emulator/beam/dist.h` · `dist.c` · `packet_parser.c` · `external.h`,
//! `lib/erl_interface/src/connect/ei_connect.c` + `ei_connect_int.h`, and
//! `lib/erl_interface/src/epmd/*.c` + `erts/epmd/src/epmd_srv.c`.

pub mod atom_cache;
pub mod auth;
pub mod connection;
pub mod control;
pub mod dist_header;
pub mod epmd;
pub mod flags;
pub mod fragment;
pub mod framing;
pub mod handshake;
pub mod prelude;
pub mod transport;

pub use crate::control::{decode_control, encode_control, ControlMessage};
pub use crate::dist_header::{AtomCacheRef, DistFragment, DistHeader};
pub use crate::epmd::{Alive2Req, Alive2Resp, EpmdClient, NamesResp, Port2Req, Port2Resp};
pub use crate::flags::{DistFlags, Dop};
pub use crate::framing::{
    is_tick, read_length, read_packet, write_packet, write_tick, PacketHeaderSize, MAX_PACKET_BODY,
};
pub use crate::handshake::{
    check_mandatory, preferred_flags, verify_digest, Challenge, ChallengeAck, ChallengeDigest,
    ChallengeReply, ChallengeSource, Complement, Name, Status,
};
pub use crate::transport::{MemoryTransport, Transport};

pub use crate::atom_cache::{OutboundAtomCache, ReceiveAtomCache, SegmentCache};
pub use crate::auth::{gen_challenge, gen_digest, md5, Md5Digest, SystemChallengeSource};
pub use crate::connection::{accept_server, connect_client, Connection, Handshake, TcpTransport};
pub use crate::fragment::{
    Assembly, FragmentAssembler, FragmentCont, FragmentHeader, DIST_FRAGMENT_SIZE,
};

/// The distribution protocol version implemented by this crate (the OTP 23+
/// handshake, `EI_DIST_6`).
pub const DIST_VERSION: u16 = 6;

/// The legacy distribution protocol version 5 (OTP R4 – 22).
pub const DIST_VERSION_5: u16 = 5;

/// The well-known `epmd` port-mapper TCP port.
pub const EPMD_PORT: u16 = 4369;

/// The external term format version byte (`131`); an alias of
/// [`beam_codec::VERSION_MAGIC`].
pub const VERSION_MAGIC: u8 = beam_codec::VERSION_MAGIC;
