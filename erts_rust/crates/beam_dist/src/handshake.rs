//! The connect/challenge handshake messages (distribution v6).
//!
//! Each message is a **body codec** — a tag byte plus big-endian fields, with no
//! length prefix; the caller wraps it in [`crate::framing`] (2- or 4-byte
//! framing as negotiated). Ground truth for every layout is
//! `lib/erl_interface/src/connect/ei_connect.c` (`send_name`/`send_challenge`/
//! `send_challenge_reply`/`send_challenge_ack`/`recv_*`) and `dist.c`.
//!
//! The v6 challenge digest is `MD5(cookie ++ decimal(challenge))`. The message
//! codecs carry the 16-byte digest **opaquely** and compare it through the
//! [`ChallengeDigest`] hook, so the digest algorithm stays pluggable;
//! [`crate::auth::Md5Digest`] is the concrete (RFC 1321) provider used on the wire.

use beam_common::{BeamError, Result};

use crate::flags::{
    DistFlags, DFLAG_DIST_DEFAULT, DFLAG_DIST_MANDATORY, DFLAG_DIST_MANDATORY_25,
    DFLAG_MANDATORY_25_DIGEST,
};
use crate::transport::Write;

pub use crate::flags::{
    CHALLENGE_ACK_TAG, CHALLENGE_REPLY_TAG, CHALLENGE_TAG, COMPLEMENT_TAG, DIST_VER_HIGH,
    DIST_VER_LOW, NAME_TAG, STATUS_TAG,
};

/// `"snamed:"` minus its leading `'s'` tag: the prefix of the `Status::Named`
/// wire text (the C reads it as `strncmp(buf, "snamed:", 7)` with `buf` pointing
/// at the tag).
const NAMED_PREFIX: &[u8] = b"named:";

/// The `Status::Ok` wire text.
const STATUS_OK: &[u8] = b"ok";

/// The `Status::Simultaneous` wire text (`dist_util.erl` sends
/// `[$s | atom_to_list(ok_simultaneous)]`).
const STATUS_OK_SIMULTANEOUS: &[u8] = b"ok_simultaneous";

/// A minimal big-endian cursor over a handshake message body.
///
/// Every read is bounds-checked: a body shorter than the requested field yields
/// [`BeamError::BadArg`]. The decoders never index past the slice and never
/// `unwrap()`.
struct Cursor<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `body`.
    fn new(body: &'a [u8]) -> Cursor<'a> {
        Cursor { body, pos: 0 }
    }

    /// Consumes exactly `n` bytes, or fails with `BadArg` when fewer remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(BeamError::BadArg)?;
        if end > self.body.len() {
            return Err(BeamError::BadArg);
        }
        let slice = &self.body[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    /// Reads one byte.
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Reads a big-endian `u16`.
    fn u16be(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// Reads a big-endian `u32`.
    fn u32be(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a big-endian `u64`.
    fn u64be(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    /// Reads a 16-byte digest.
    fn digest16(&mut self) -> Result<[u8; 16]> {
        let mut digest = [0u8; 16];
        digest.copy_from_slice(self.take(16)?);
        Ok(digest)
    }

    /// The bytes not yet consumed.
    fn rest(&self) -> &'a [u8] {
        &self.body[self.pos..]
    }
}

/// Writes a `u16` name length followed by the name bytes.
///
/// A name longer than `u16::MAX` cannot be framed on the wire and is rejected
/// with [`BeamError::SystemLimit`] (a real node name is capped at `MAXNODELEN`,
/// 255).
fn write_name<W: Write>(w: &mut W, name: &[u8]) -> Result<()> {
    let len = u16::try_from(name.len()).map_err(|_| BeamError::SystemLimit)?;
    w.write_u16be(len)?;
    w.write_all(name)
}

/// Reads a length-prefixed (`u16`) name; a length beyond the body is `BadArg`.
fn read_name(cursor: &mut Cursor<'_>) -> Result<Vec<u8>> {
    let len = cursor.u16be()? as usize;
    Ok(cursor.take(len)?.to_vec())
}

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
        w.write_u8(NAME_TAG)?;
        w.write_u64be(self.flags.bits())?;
        w.write_u32be(self.creation)?;
        write_name(w, &self.name)
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(body);
        if cursor.u8()? != NAME_TAG {
            return Err(BeamError::BadArg);
        }
        let flags = DistFlags::from_bits(cursor.u64be()?);
        let creation = cursor.u32be()?;
        let name = read_name(&mut cursor)?;
        Ok(Name { flags, creation, name })
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
        w.write_u8(STATUS_TAG)?;
        match self {
            Status::Ok => w.write_all(STATUS_OK),
            Status::Simultaneous => w.write_all(STATUS_OK_SIMULTANEOUS),
            Status::Named { name, creation } => {
                w.write_all(NAMED_PREFIX)?;
                write_name(w, name)?;
                w.write_u32be(*creation)
            }
            Status::Error(text) => w.write_all(text),
        }
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    ///
    /// The text is matched by prefix, exactly as `recv_status` does: `"ok"`,
    /// `"ok_simultaneous"` and `"named:…"` have dedicated variants; every other
    /// status text (`"nok"`, `"not_allowed"`, `"alive"`, …) is carried verbatim
    /// in [`Status::Error`].
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(body);
        if cursor.u8()? != STATUS_TAG {
            return Err(BeamError::BadArg);
        }
        let text = cursor.rest();
        if text == STATUS_OK {
            return Ok(Status::Ok);
        }
        if text == STATUS_OK_SIMULTANEOUS {
            return Ok(Status::Simultaneous);
        }
        if let Some(rest) = text.strip_prefix(NAMED_PREFIX) {
            let mut named = Cursor::new(rest);
            let name = read_name(&mut named)?;
            let creation = named.u32be()?;
            return Ok(Status::Named { name, creation });
        }
        Ok(Status::Error(text.to_vec()))
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
        w.write_u8(CHALLENGE_TAG)?;
        w.write_u64be(self.flags.bits())?;
        w.write_u32be(self.challenge)?;
        w.write_u32be(self.creation)?;
        write_name(w, &self.name)
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(body);
        if cursor.u8()? != CHALLENGE_TAG {
            return Err(BeamError::BadArg);
        }
        let flags = DistFlags::from_bits(cursor.u64be()?);
        let challenge = cursor.u32be()?;
        let creation = cursor.u32be()?;
        let name = read_name(&mut cursor)?;
        Ok(Challenge { flags, challenge, creation, name })
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
        w.write_u8(CHALLENGE_REPLY_TAG)?;
        w.write_u32be(self.challenge)?;
        w.write_all(&self.digest)
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(body);
        if cursor.u8()? != CHALLENGE_REPLY_TAG {
            return Err(BeamError::BadArg);
        }
        let challenge = cursor.u32be()?;
        let digest = cursor.digest16()?;
        Ok(ChallengeReply { challenge, digest })
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
        w.write_u8(CHALLENGE_ACK_TAG)?;
        w.write_all(&self.digest)
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(body);
        if cursor.u8()? != CHALLENGE_ACK_TAG {
            return Err(BeamError::BadArg);
        }
        let digest = cursor.digest16()?;
        Ok(ChallengeAck { digest })
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
        w.write_u8(COMPLEMENT_TAG)?;
        w.write_u32be(self.flags_high)?;
        w.write_u32be(self.creation)
    }

    /// Decodes the message body; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(body);
        if cursor.u8()? != COMPLEMENT_TAG {
            return Err(BeamError::BadArg);
        }
        let flags_high = cursor.u32be()?;
        let creation = cursor.u32be()?;
        Ok(Complement { flags_high, creation })
    }
}

/// The `DFLAG_*` set this node offers in the connect handshake
/// (`DFLAG_DIST_DEFAULT`).
pub fn preferred_flags() -> DistFlags {
    DistFlags::from_bits(DFLAG_DIST_DEFAULT)
}

/// Validates a peer's flags: applies the `MANDATORY_25_DIGEST ⇒ DIST_MANDATORY_25`
/// rule and rejects a peer missing any of `MANDATORY_DIST_FLAGS`, exactly as
/// `recv_challenge` does. Returns the (possibly adjusted) negotiated flags.
///
/// Mirror of `dist.c`/`recv_challenge` (`ei_connect.c:2439`):
/// ```c
/// if (*flags & DFLAG_MANDATORY_25_DIGEST) *flags |= DFLAG_DIST_MANDATORY_25;
/// if ((*flags & DFLAG_DIST_MANDATORY) != DFLAG_DIST_MANDATORY) { error; }
/// ```
pub fn check_mandatory(flags: DistFlags) -> Result<DistFlags> {
    let mut flags = flags;
    if flags.contains(DistFlags::from_bits(DFLAG_MANDATORY_25_DIGEST)) {
        flags.insert(DistFlags::from_bits(DFLAG_DIST_MANDATORY_25));
    }
    if !flags.contains(DistFlags::from_bits(DFLAG_DIST_MANDATORY)) {
        return Err(BeamError::BadArg);
    }
    Ok(flags)
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
    got == &d.digest(cookie, our_challenge)
}

/// The challenge nonce generator.
pub trait ChallengeSource {
    /// Returns the next challenge nonce.
    fn next_challenge(&self) -> u32;
}
