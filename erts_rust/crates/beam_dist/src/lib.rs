#![doc = "Erlang distribution protocol v6: handshake, DFLAG_*/DOP_* control messages and epmd."]
#![deny(unsafe_code)]
#![warn(missing_docs)]

//! Distribution protocol v6 (`DFLAG_*` / `DOP_*`) and the `epmd` port-mapper
//! client — compatibility contract Part I §A.5 (the distribution wire) and the
//! Stage-0 foundations of Part II §H.
//!
//! The crate is **pure protocol/value code over an abstract transport**: it never
//! opens a socket (`beam_io` owns transports; `beam_rt` drives node startup). It
//! owns the *distribution-specific* wire — the flag/opcode vocabulary
//! ([`flags`]), the connect/challenge handshake ([`handshake`]), the
//! packet-length framing ([`framing`]), the distribution-header envelope
//! ([`dist_header`]) and the control-message codec ([`control`]). Term
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

pub mod control;
pub mod dist_header;
pub mod epmd;
pub mod flags;
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
