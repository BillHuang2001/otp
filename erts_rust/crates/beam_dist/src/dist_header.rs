//! The distribution-header envelope (`'D'`) plus the atom-cache refs and the
//! fragment tags.
//!
//! Ground truth: `erts/emulator/beam/external.h` (the `'D'`/`'E'`/`'F'`/`'H'`/
//! `'R'`/`'N'` tags) and `erts/emulator/beam/external.c` (the header layout:
//! `'D' + len(4) + flags(1) + refs`).

use beam_common::Result;
use beam_term::Atom;

use crate::transport::Write;

/// `DIST_HEADER` (`'D'`): the distribution-header tag.
pub const DIST_HEADER: u8 = b'D';
/// `DIST_FRAG_HEADER` (`'E'`): the first fragment of a fragmented message.
pub const DIST_FRAG_HEADER: u8 = b'E';
/// `DIST_FRAG_CONT` (`'F'`): a continuation fragment.
pub const DIST_FRAG_CONT: u8 = b'F';
/// `HOPEFUL_DATA` (`'H'`): a message sent optimistically before the handshake
/// completed.
pub const HOPEFUL_DATA: u8 = b'H';
/// `ATOM_CACHE_REF` (`'R'`): a single atom-cache reference.
pub const ATOM_CACHE_REF: u8 = b'R';
/// `NEW_CACHE` (`'N'`): an atom-cache segment update.
pub const NEW_CACHE: u8 = b'N';

/// `DIST_HEADER_FLAG_ATOM_CACHE`: the distribution-header flag bit announcing
/// that atom-cache references follow.
pub const DIST_HEADER_FLAG_ATOM_CACHE: u8 = 0x01;

/// A single atom-cache reference.
///
/// Layout: `'R' + (segment << 1 | internal) + atom_index`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct AtomCacheRef {
    /// The cache segment (0-7).
    pub segment: u8,
    /// Whether the reference is "internal" (bit 0 of the segment byte).
    pub internal: bool,
    /// The referenced atom.
    pub atom: Atom,
}

impl AtomCacheRef {
    /// Encodes the reference (tag + fields).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes a reference; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The `'D'` distribution-header envelope.
///
/// Layout: `'D' + len(4) + flags(1) + refs`. The `flags` bit
/// [`DIST_HEADER_FLAG_ATOM_CACHE`] announces that `cache_refs` follow.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DistHeader {
    /// The header flag byte.
    pub flags: u8,
    /// The atom-cache references that follow the flags.
    pub cache_refs: Vec<AtomCacheRef>,
}

impl DistHeader {
    /// Encodes the header (tag + fields, no length prefix).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        todo!("M1")
    }

    /// Decodes the header; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        todo!("M1")
    }
}

/// The kind of a distribution fragment, recognised from its tag byte (`'E'`/`'F'`).
///
/// Reassembly of fragmented payloads is out of Milestone 1 (see the crate
/// `CONTEXT.md`, "Known Issues").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DistFragment {
    /// `'E'` — the first fragment, carrying the fragment header.
    Header,
    /// `'F'` — a continuation fragment.
    Continue,
}

impl DistFragment {
    /// Recognises a fragment tag byte; `None` for any other byte.
    pub fn from_tag(tag: u8) -> Option<DistFragment> {
        match tag {
            DIST_FRAG_HEADER => Some(DistFragment::Header),
            DIST_FRAG_CONT => Some(DistFragment::Continue),
            _ => None,
        }
    }
}
