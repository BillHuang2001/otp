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
    DFLAG_MANDATORY_25_DIGEST, DFLAG_PUBLISHED,
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

/// The `"nok"` refusal text (`dist_util.erl:343`: `send_status(HSData, nok)`).
const STATUS_NOK: &[u8] = b"nok";

/// The `"not_allowed"` refusal text (`dist_util.erl:286` and `:851`:
/// `send_status(HSData, not_allowed)`).
const STATUS_NOT_ALLOWED: &[u8] = b"not_allowed";

/// The `"alive"` status text (`dist_util.erl:386`: `send_status(HSData, alive)`).
const STATUS_ALIVE: &[u8] = b"alive";

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

/// The kind of a peer's [`Status`] reply — the status codes the reference
/// implementations distinguish.
///
/// Ground truth: `erts/doc/guides/erl_dist_protocol.md:533-575` (the defined
/// status codes), `lib/kernel/src/dist_util.erl` (`send_status`/`recv_status` and
/// `check_dflags`) and `lib/erl_interface/src/connect/ei_connect.c:2182`
/// (`recv_status`). Every variant's wire text is verified against those sources.
///
/// The `no_creation` and `system_limit` codes named in some descriptions are
/// **not** sent as status text by this OTP release: `system_limit` is a local
/// error thrown by `erlang:setnode/3` (`dist_util.erl:593`) and `no_creation`
/// does not appear at all — both therefore classify as [`StatusKind::Other`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StatusKind {
    /// `"ok"` — the handshake continues (`dist_util.erl:326`, `:1081`).
    Ok,
    /// `"ok_simultaneous"` — a simultaneous connect was detected and the
    /// handshake continues (`dist_util.erl:338`, `:1082`; the guide's
    /// `ok_simultaneous`).
    Simultaneous,
    /// `"named:" + Nlen(2) + Name + Creation(4)` — the peer assigned a *dynamic*
    /// node name (`dist_util.erl:333`, `send_status(HSData1, named)`; the guide's
    /// `named:` table). `ei_connect.c:2212` reads the name/creation back out of it.
    Named,
    /// `"nok"` — the peer refuses because it already has an ongoing handshake it
    /// itself initiated (`dist_util.erl:343`; the guide's `nok`).
    Nok,
    /// `"not_allowed"` — the peer refuses for a security/flag reason: the
    /// connecting node is disallowed (`dist_util.erl:851`) or its flags fail the
    /// mandatory check (`dist_util.erl:286`; the guide's `not_allowed`).
    NotAllowed,
    /// `"alive"` — the peer believes a connection to this node is already active
    /// (`dist_util.erl:386`; the guide's `alive`; step 3B replies `"true"`/`"false"`).
    Alive,
    /// Any other status text — the guide's step-3B `"true"`/`"false"` replies, an
    /// unknown code, or protocol garbage.
    Other,
}

/// The `'s'` status message.
///
/// The wire text is one of `"ok"`/`"sok"`/`"sok_simultaneous"`/`"nok…"` or
/// `"snamed:" + namelen(2) + name + creation(4)`; it is parsed by prefix.
///
/// Every status text other than the three that have a dedicated variant is
/// carried verbatim in [`Status::Error`] (so nothing is lost). Use
/// [`Status::kind`] to classify any status — including the ones the reference
/// implementations distinguish (`"nok"`, `"not_allowed"`, `"alive"`) — and
/// [`Status::error_kind`] to ask whether it is a refusal at all.
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

    /// Classifies this status into the code the reference implementations
    /// distinguish (see [`StatusKind`]).
    ///
    /// This is purely additive: it never changes the carried text, so a caller
    /// that wants the raw bytes still reads them out of [`Status::Error`].
    pub fn kind(&self) -> StatusKind {
        match self {
            Status::Ok => StatusKind::Ok,
            Status::Simultaneous => StatusKind::Simultaneous,
            Status::Named { .. } => StatusKind::Named,
            Status::Error(text) if text.as_slice() == STATUS_NOK => StatusKind::Nok,
            Status::Error(text) if text.as_slice() == STATUS_NOT_ALLOWED => StatusKind::NotAllowed,
            Status::Error(text) if text.as_slice() == STATUS_ALIVE => StatusKind::Alive,
            Status::Error(_) => StatusKind::Other,
        }
    }

    /// The refusal kind, or `None` when this status means the handshake
    /// continues.
    ///
    /// Per the ground truth (`dist_util.erl:1081-1082`, the guide's "the handshake
    /// will continue") `"ok"`, `"ok_simultaneous"` and `"named:"` are **not**
    /// refusals — a real node proceeds after each. Everything else
    /// ([`StatusKind::Nok`], [`StatusKind::NotAllowed`], [`StatusKind::Alive`],
    /// [`StatusKind::Other`]) aborts the handshake one way or another.
    pub fn error_kind(&self) -> Option<StatusKind> {
        match self.kind() {
            StatusKind::Ok | StatusKind::Simultaneous | StatusKind::Named => None,
            kind => Some(kind),
        }
    }

    /// The `"not_allowed"` refusal.
    ///
    /// This is exactly what a real accepting node replies with when the
    /// connecting node's flags fail the mandatory check (`dist_util.erl:286`:
    /// `check_dflags` → `send_status(HSData, not_allowed)`) or the node is
    /// disallowed (`dist_util.erl:851`).
    pub fn not_allowed() -> Status {
        Status::Error(STATUS_NOT_ALLOWED.to_vec())
    }

    /// The `"nok"` refusal: a real accepting node replies with it when it already
    /// has an ongoing handshake it itself initiated (`dist_util.erl:343`).
    pub fn nok() -> Status {
        Status::Error(STATUS_NOK.to_vec())
    }
}

/// Maps a peer's non-`Ok` [`Status`] onto a typed [`BeamError`].
///
/// This is the **single** implementation of the status -> error mapping: the
/// blocking [`crate::connection`] API and the non-blocking [`crate::driver`]
/// both delegate here, so a caller observes exactly the same
/// [`std::io::ErrorKind`] (and the same message shape) whichever API it used.
///
/// The [`Status::kind`] classification selects the `io::ErrorKind`:
///
/// | status ([`StatusKind`]) | wire text (ground truth) | `io::ErrorKind` |
/// |---|---|---|
/// | `Nok` | `"nok"` (`dist_util.erl:343`) | `ConnectionRefused` |
/// | `NotAllowed` | `"not_allowed"` (`dist_util.erl:286`) | `PermissionDenied` |
/// | `Alive` | `"alive"` (`dist_util.erl:386`) | `ConnectionAborted` |
/// | `Named` | `"named:…"` (`dist_util.erl:333`) | `AlreadyExists` |
/// | `Simultaneous` | `"ok_simultaneous"` (`dist_util.erl:338`) | `ConnectionAborted` |
/// | `Ok` / `Other` | `"ok"` / unknown | `InvalidData` |
///
/// `"ok"`/`"ok_simultaneous"`/`"named:"` all mean *the handshake continues* to a
/// real node, but this crate's client can continue none of them: it has no
/// dynamic-node-name support (`"named:"` assigns one) and no connection registry
/// with which to resolve a simultaneous-connect race. Returning a typed error is
/// the honest outcome — never a wrong result and never a hang. `Ok`/`Other` here
/// are unreachable in practice (the callers only report a non-`Ok` status).
pub fn status_error(status: &Status) -> BeamError {
    let kind = status.kind();
    let io_kind = match kind {
        StatusKind::Nok => std::io::ErrorKind::ConnectionRefused,
        StatusKind::NotAllowed => std::io::ErrorKind::PermissionDenied,
        StatusKind::Alive => std::io::ErrorKind::ConnectionAborted,
        StatusKind::Named => std::io::ErrorKind::AlreadyExists,
        StatusKind::Simultaneous => std::io::ErrorKind::ConnectionAborted,
        StatusKind::Ok | StatusKind::Other => std::io::ErrorKind::InvalidData,
    };
    let detail = match status {
        Status::Named { name, creation } => format!(
            "peer reports node name '{}' (creation {creation}) as already assigned (named)",
            String::from_utf8_lossy(name)
        ),
        Status::Simultaneous => "peer sent status 'ok_simultaneous'".to_string(),
        Status::Ok => "peer sent status 'ok'".to_string(),
        Status::Error(text) => format!("peer sent status '{}'", String::from_utf8_lossy(text)),
    };
    BeamError::Io(std::io::Error::new(
        io_kind,
        format!("distribution handshake ({kind:?}): {detail}"),
    ))
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
///
/// This mirrors the emulator's `erts_dflags.default` (`dist.h:118-129`) **exactly**,
/// which deliberately omits `DFLAG_PUBLISHED` (`0x01`, `dist.h:30`): a real node
/// adds that bit separately, *per peer*, in its Erlang-level handshake driver
/// (`dist_util.erl:199-200`). Use [`publish_flag`] to derive the per-connection
/// published bit and the opt-in constructors (e.g.
/// [`crate::driver::ConnectionDriver::server_with_flags`]) to advertise it.
pub fn preferred_flags() -> DistFlags {
    DistFlags::from_bits(DFLAG_DIST_DEFAULT)
}

/// The node-visibility classification `dist_util.erl:publish_type/1` assigns from
/// a **peer's** advertised flags.
///
/// Ground truth (`lib/kernel/src/dist_util.erl:917-923`):
/// ```erlang
/// publish_type(Flags) ->
///     case Flags band ?DFLAG_PUBLISHED of
///         0 -> hidden;
///         _ -> normal
///     end.
/// ```
/// `DFLAG_PUBLISHED` is `0x01` (`dist.h:30`). A **hidden** node stays reachable
/// but is excluded from a peer's `nodes()`/`net_adm:nodes()` (only *normal*
/// connections are reported there); `nodes(connected)` still lists it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PeerVisibility {
    /// The peer advertised `DFLAG_PUBLISHED`: a **normal** (visible) node.
    Normal,
    /// The peer did not advertise `DFLAG_PUBLISHED`: a **hidden** node.
    Hidden,
}

/// Classifies a peer's advertised flags as [`PeerVisibility::Normal`] or
/// [`PeerVisibility::Hidden`] — the direct mirror of `dist_util.erl:publish_type/1`
/// (`dist_util.erl:917-923`), keyed **only** on `DFLAG_PUBLISHED` (`dist.h:30`).
pub fn publish_type(flags: DistFlags) -> PeerVisibility {
    if flags.contains(DistFlags::from_bits(DFLAG_PUBLISHED)) {
        PeerVisibility::Normal
    } else {
        PeerVisibility::Hidden
    }
}

/// The distribution request type of a connection, mirroring the `hidden`/`normal`
/// values `dist_util.erl` threads through `publish_flag/3`.
///
/// The `DFLAG_NAME_ME` branch of `publish_flag/3` (dynamic node names,
/// `dist_util.erl:150-151`) is deliberately **not** modelled: this crate speaks
/// only the static OTP-23 `'N'` name and has no dynamic-node-name support, so the
/// `NameMeFlg` case is out of scope (modelling it would invent a capability the
/// rest of the crate cannot honor).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PublishKind {
    /// A normal (non-hidden) distribution connection.
    Normal,
    /// A `-hidden` distribution connection.
    Hidden,
}

impl PublishKind {
    /// The extra flag this node must OR into its advertised set for a connection of
    /// this kind — the mirror of `dist_util.erl:publish_flag/3`
    /// (`dist_util.erl:150-160`) with the `DFLAG_NAME_ME` branch omitted (see
    /// [`PublishKind`]):
    ///
    /// ```erlang
    /// publish_flag(hidden, _, _) -> 0;
    /// publish_flag(_, _, OtherNode) ->
    ///     case net_kernel:publish_on_node(OtherNode) of
    ///         true -> ?DFLAG_PUBLISHED;
    ///         _    -> 0
    ///     end.
    /// ```
    ///
    /// `published` is `net_kernel:publish_on_node(OtherNode)` for the peer
    /// `OtherNode`: a **normal** connection to a peer we publish to yields
    /// `DFLAG_PUBLISHED` (`0x01`, `dist.h:30`); a **hidden** connection — or a
    /// normal one to a peer we do not publish to — yields no flag at all.
    pub fn publish_flag(self, published: bool) -> DistFlags {
        match self {
            PublishKind::Hidden => DistFlags::EMPTY,
            PublishKind::Normal if published => DistFlags::from_bits(DFLAG_PUBLISHED),
            PublishKind::Normal => DistFlags::EMPTY,
        }
    }
}

/// [`PublishKind::publish_flag`] for a caller that only has the boolean form: the
/// direct mirror of `dist_util.erl:publish_flag/3` with the `DFLAG_NAME_ME` branch
/// omitted (see [`PublishKind`]).
///
/// `hidden` is the request type (`hidden`/`normal`) and `published` is
/// `net_kernel:publish_on_node(OtherNode)` for the peer.
pub fn publish_flag(hidden: bool, published: bool) -> DistFlags {
    let kind = if hidden { PublishKind::Hidden } else { PublishKind::Normal };
    kind.publish_flag(published)
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
///
/// # What this crate can detect about the protocol version
///
/// The distribution *version* (`EI_DIST_6`, the `DIST_VER_HIGH`/`DIST_VER_LOW`
/// pair) is negotiated out of band: `epmd`'s `PORT2_RESP` reports the peer's
/// high/low version (`ei_connect.c:1398`, `epmd_srv.c:904-936`) before the socket
/// is opened, and this crate's connect path uses only the *port* from that reply —
/// no version comparison happens there (`connection::connect_to_node`). What this
/// function enforces is the in-band equivalent the reference relies on: a peer
/// whose flags miss any of `DFLAG_DIST_MANDATORY` — which includes
/// `DFLAG_HANDSHAKE_23`, the OTP 23+ `'N'` handshake marker — is rejected with
/// [`BeamError::BadArg`] (`dist_util.erl:278`, `check_dflags`). A pre-OTP-23 peer
/// opening with the legacy `'n'` name, and any frame with an unexpected tag, fail
/// the codecs' tag check with `BeamError::BadArg` instead.
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
