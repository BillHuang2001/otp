//! The connect/challenge handshake messages (distribution v6).
//!
//! Each message is a **body codec** — a tag byte plus big-endian fields, with no
//! length prefix; the caller wraps it in [`crate::framing`] (2- or 4-byte
//! framing as negotiated). Ground truth for every layout is
//! `lib/erl_interface/src/connect/ei_connect.c` (`send_name`/`send_challenge`/
//! `send_challenge_reply`/`send_challenge_ack`/`recv_*`) and `dist.c`.
//!
//! The v6 challenge digest is `MD5(cookie ++ decimal(challenge))`. No crypto
//! dependency is available in Milestone 1, so the digest is carried **opaquely**
//! and compared through the [`ChallengeDigest`] hook; only the MD5 *value* is
//! pluggable.

use beam_common::Result;

use crate::flags::DistFlags;
use crate::transport::Write;

pub use crate::flags::{
    CHALLENGE_ACK_TAG, CHALLENGE_REPLY_TAG, CHALLENGE_TAG, COMPLEMENT_TAG, DIST_VER_HIGH,
    DIST_VER_LOW, NAME_TAG, STATUS_TAG,
};

/// The `'N'` connect-name message.
///
/// Layout: `'N' + flags(8) + creation(4) + namelen(2) + name`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Name {
    /// The sender's capability flags.
    pub flags: DistFlags,
    /// The sender's node creation.
    pub creation: u32,
    /// The sender's node name (UTF-8).
    pub name: Vec<u8>,
}

impl Name {
    /// Encodes the message body (tag + fields, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `'s'` status message.
///
/// The wire text is one of `"ok"`/`"sok"`/`"sok_simultaneous"`/`"nok…"` or
/// `"snamed:" + namelen(2) + name + creation(4)`; it is parsed by prefix.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Status {
    /// `"ok"` — proceed.
    Ok,
    /// `"sok_simultaneous"`/`"sok"` — simultaneous connect.
    Simultaneous,
    /// `"snamed:" + name + creation` — the peer's name is already taken.
    Named {
        /// The already-registered name.
        name: Vec<u8>,
        /// Its creation.
        creation: u32,
    },
    /// `"nok…"` — an error, carrying the raw text.
    Error(Vec<u8>),
}

impl Status {
    /// Encodes the message body (tag + text, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `'N'` challenge message.
///
/// Layout: `'N' + flags(8) + challenge(4) + creation(4) + namelen(2) + name`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Challenge {
    /// The sender's capability flags.
    pub flags: DistFlags,
    /// The sender's challenge nonce.
    pub challenge: u32,
    /// The sender's node creation.
    pub creation: u32,
    /// The sender's node name (UTF-8).
    pub name: Vec<u8>,
}

impl Challenge {
    /// Encodes the message body (tag + fields, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `'r'` challenge-reply message.
///
/// Layout: `'r' + challenge(4) + digest(16)`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ChallengeReply {
    /// The responder's challenge nonce.
    pub challenge: u32,
    /// The 16-byte digest (opaque; see [`ChallengeDigest`]).
    pub digest: [u8; 16],
}

impl ChallengeReply {
    /// Encodes the message body (tag + fields, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `'a'` challenge-ack message.
///
/// Layout: `'a' + digest(16)`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ChallengeAck {
    /// The 16-byte digest (opaque; see [`ChallengeDigest`]).
    pub digest: [u8; 16],
}

impl ChallengeAck {
    /// Encodes the message body (tag + fields, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `'c'` OTP-23 high-flags complement message.
///
/// Layout: `'c' + flags-high(4) + creation(4)`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Complement {
    /// The high 32 bits of the peer's capability flags.
    pub flags_high: u32,
    /// The peer's node creation.
    pub creation: u32,
}

impl Complement {
    /// Encodes the message body (tag + fields, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `DFLAG_*` set this node offers in the connect handshake
/// (`DFLAG_DIST_DEFAULT`).
pub fn preferred_flags() -> DistFlags {
    todo!("M1")
}

/// Validates a peer's flags: applies the `MANDATORY_25_DIGEST ⇒ DIST_MANDATORY_25`
/// rule and rejects a peer missing any of `MANDATORY_DIST_FLAGS`, exactly as
/// `recv_challenge` does. Returns the (possibly adjusted) negotiated flags.
pub fn check_mandatory(flags: DistFlags) -> Result<DistFlags> {
    todo!("M1")
}

/// Computes the 16-byte challenge digest `MD5(cookie ++ decimal(challenge))`.
///
/// This is the pluggable seam that stands in for the missing in-tree MD5: the
/// message codecs treat the digest opaquely and comparison goes through this
/// trait.
pub trait ChallengeDigest {
    /// Returns the digest for `cookie` and `challenge`.
    fn digest(&self, cookie: &[u8], challenge: u32) -> [u8; 16];
}

/// Compares a received digest against the one computed over `our_challenge`.
pub fn verify_digest(
    got: &[u8; 16],
    our_challenge: u32,
    cookie: &[u8],
    d: &impl ChallengeDigest,
) -> bool {
    todo!("M1")
}

/// The challenge nonce generator.
pub trait ChallengeSource {
    /// Returns the next challenge nonce.
    fn next_challenge(&self) -> u32;
}