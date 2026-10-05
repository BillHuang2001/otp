//! The distribution-header envelope (`'D'`) plus the atom-cache refs and the
//! fragment tags.
//!
//! Ground truth: `erts/emulator/beam/external.h` (the `'D'`/`'E'`/`'F'`/`'H'`/
//! `'R'`/`'N'` tags) and `erts/emulator/beam/external.c` (the header layout:
//! `'D' + len(4) + flags(1) + refs`).
//!
//! Fidelity note (M1 model): the tag bytes are the frozen C ones, but this module
//! models the envelope as `'D' + len(4) + flags(1) + refs` with a fixed 3-byte ref.
//! The wire image produced by `erts_encode_ext_dist_header_finalize`
//! (`external.c`) is in fact `VERSION_MAGIC + 'D' + NumberOfAtomCacheRefs(1) +
//! half-byte flag field(s) + refs`, where a ref is a `CachedAtomRef` (one index
//! byte) or a `NewAtomCacheRef` (index + length + atom text) and the segment lives
//! in the flag half-bytes; the `'R'` + segment + index shape below is the *term*
//! level `ATOM_CACHE_REF` tag (`external.c` `case ATOM_CACHE_REF:`). Byte-exact
//! rendering of the real header is therefore deferred (see the crate
//! `CONTEXT.md`, "Known Issues").

use beam_common::{BeamError, Result};
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
    ///
    /// The atom index is written as a single byte (its low 8 bits), matching the
    /// frozen 1-byte index field of the wire format.
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        w.write_u8(ATOM_CACHE_REF)?;
        w.write_u8((self.segment << 1) | u8::from(self.internal))?;
        w.write_u8(self.atom.raw() as u8)
    }

    /// Decodes a reference; a wrong/short tag is `BeamError::BadArg`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        if body.len() < 3 || body[0] != ATOM_CACHE_REF {
            return Err(BeamError::BadArg);
        }
        Ok(AtomCacheRef {
            segment: body[1] >> 1,
            internal: (body[1] & 1) != 0,
            atom: Atom::from_raw(u32::from(body[2])),
        })
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
    ///
    /// The 4-byte big-endian `len` field counts the bytes that *follow* it — the
    /// `flags` byte plus every encoded ref (`1 + 3 * cache_refs.len()`). The
    /// `cache_refs` are written exactly as held, so `flags` and `cache_refs` must
    /// be consistent: [`DIST_HEADER_FLAG_ATOM_CACHE`] in `flags` whenever the vec
    /// is non-empty (a header that violates this is rejected by [`Self::decode`]).
    pub fn encode<W: Write>(&self, w: &mut W) -> Result<()> {
        let len = self
            .cache_refs
            .len()
            .checked_mul(3)
            .and_then(|refs_bytes| refs_bytes.checked_add(1))
            .ok_or(BeamError::SystemLimit)?;
        let len = u32::try_from(len).map_err(|_| BeamError::SystemLimit)?;

        w.write_u8(DIST_HEADER)?;
        w.write_u32be(len)?;
        w.write_u8(self.flags)?;
        for cache_ref in &self.cache_refs {
            cache_ref.encode(w)?;
        }
        Ok(())
    }

    /// Decodes the header; a wrong/short tag is `BeamError::BadArg`.
    ///
    /// The body must cover the declared `len` (which counts the `flags` byte plus
    /// the refs); any further bytes — e.g. the control message that follows the
    /// header in a distribution packet — are left to the caller. With
    /// [`DIST_HEADER_FLAG_ATOM_CACHE`] set, `(len - 1) / 3` refs are read, so a
    /// ref field whose size is not a multiple of 3 is `BeamError::BadArg`; without
    /// the flag, trailing ref bytes are `BeamError::BadArg` (never dropped
    /// silently).
    pub fn decode(body: &[u8]) -> Result<Self> {
        // `'D' + len(4) + flags(1) + refs`
        if body.len() < 6 || body[0] != DIST_HEADER {
            return Err(BeamError::BadArg);
        }
        let declared = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
        // `declared` counts the flags byte plus the refs; the whole envelope
        // (`tag + len + declared`) must fit in the body.
        let end = declared.checked_add(5).filter(|end| *end <= body.len());
        let end = match end {
            Some(end) if declared >= 1 => end,
            _ => return Err(BeamError::BadArg),
        };
        let flags = body[5];
        let ref_bytes = &body[6..end];

        let mut cache_refs = Vec::new();
        if flags & DIST_HEADER_FLAG_ATOM_CACHE != 0 {
            if ref_bytes.len() % 3 != 0 {
                return Err(BeamError::BadArg);
            }
            cache_refs.reserve(ref_bytes.len() / 3);
            for chunk in ref_bytes.chunks_exact(3) {
                cache_refs.push(AtomCacheRef::decode(chunk)?);
            }
        } else if !ref_bytes.is_empty() {
            return Err(BeamError::BadArg);
        }

        Ok(DistHeader { flags, cache_refs })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `Write` sink: the codecs are pure and in-memory, so collecting
    /// the bytes in a `Vec` is all the tests need.
    #[derive(Default)]
    struct Sink(Vec<u8>);

    impl Write for Sink {
        fn write_all(&mut self, buf: &[u8]) -> Result<()> {
            self.0.extend_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    fn encode_ref(r: &AtomCacheRef) -> Vec<u8> {
        let mut sink = Sink::default();
        r.encode(&mut sink).expect("encoding into a Vec cannot fail");
        sink.0
    }

    fn encode_header(h: &DistHeader) -> Vec<u8> {
        let mut sink = Sink::default();
        h.encode(&mut sink).expect("encoding into a Vec cannot fail");
        sink.0
    }

    fn bad_arg<T: std::fmt::Debug>(res: Result<T>) -> bool {
        matches!(res, Err(BeamError::BadArg))
    }

    fn cache_ref(segment: u8, internal: bool, atom: u32) -> AtomCacheRef {
        AtomCacheRef { segment, internal, atom: Atom::from_raw(atom) }
    }

    #[test]
    fn atom_cache_ref_round_trip() {
        let r = cache_ref(2, true, 0x2A);
        let bytes = encode_ref(&r);
        assert_eq!(bytes, vec![ATOM_CACHE_REF, (2 << 1) | 1, 0x2A]);
        assert_eq!(bytes, vec![82, 5, 42]);
        assert_eq!(AtomCacheRef::decode(&bytes).expect("valid ref"), r);
    }

    #[test]
    fn atom_cache_ref_external_bit_is_bit_zero() {
        let external = cache_ref(2, false, 0x2A);
        assert_eq!(encode_ref(&external)[1], 4);
        assert_eq!(AtomCacheRef::decode(&encode_ref(&external)).expect("valid ref"), external);

        // The bit is read back, not inferred: only bit 0 is "internal".
        assert_eq!(AtomCacheRef::decode(&[ATOM_CACHE_REF, 0x05, 7]).expect("valid ref").segment, 2);
        assert!(AtomCacheRef::decode(&[ATOM_CACHE_REF, 0x05, 7]).expect("valid ref").internal);
    }

    #[test]
    fn atom_cache_ref_index_is_one_byte() {
        // The wire index is 8 bits wide: a wider raw index is masked on the wire.
        let bytes = encode_ref(&cache_ref(0, false, 0x1_2A));
        assert_eq!(bytes, vec![ATOM_CACHE_REF, 0, 0x2A]);
        assert_eq!(AtomCacheRef::decode(&bytes).expect("valid ref").atom, Atom::from_raw(0x2A));
    }

    #[test]
    fn atom_cache_ref_negatives() {
        assert!(bad_arg(AtomCacheRef::decode(&[])));
        assert!(bad_arg(AtomCacheRef::decode(&[ATOM_CACHE_REF, 0x00])));
        // Wrong tag (`'D'` is the header tag, not the ref tag).
        assert!(bad_arg(AtomCacheRef::decode(&[DIST_HEADER, 0x00, 0x00])));
        // Trailing bytes are tolerated: a ref may be decoded out of a larger body.
        assert_eq!(
            AtomCacheRef::decode(&[ATOM_CACHE_REF, (3 << 1), 9, 0xFF]).expect("valid ref"),
            cache_ref(3, false, 9)
        );
    }

    #[test]
    fn dist_header_without_refs_round_trip() {
        let h = DistHeader { flags: 0, cache_refs: Vec::new() };
        let bytes = encode_header(&h);
        assert_eq!(bytes, vec![DIST_HEADER, 0, 0, 0, 1, 0]);
        assert_eq!(DistHeader::decode(&bytes).expect("valid header"), h);
    }

    #[test]
    fn dist_header_with_one_ref_round_trip() {
        let r = cache_ref(5, false, 0x07);
        let h = DistHeader { flags: DIST_HEADER_FLAG_ATOM_CACHE, cache_refs: vec![r.clone()] };
        let bytes = encode_header(&h);

        // `'D' + len(4) + flags(1) + refs`; len counts the flags byte + refs.
        assert_eq!(bytes[0], DIST_HEADER);
        let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        assert_eq!(len, 1 + 3 * h.cache_refs.len());
        assert_eq!(&bytes[1..5], &[0, 0, 0, 4]);
        assert_eq!(bytes[5], DIST_HEADER_FLAG_ATOM_CACHE);
        assert_eq!(&bytes[6..], &[ATOM_CACHE_REF, (5 << 1) | 0, 0x07]);
        assert_eq!(bytes.len(), 5 + len);

        assert_eq!(DistHeader::decode(&bytes).expect("valid header"), h);

        // Trailing bytes (the rest of the distribution packet) are ignored.
        let mut padded = bytes.clone();
        padded.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(DistHeader::decode(&padded).expect("valid header"), h);
    }

    #[test]
    fn dist_header_with_many_refs_round_trip() {
        let refs = vec![cache_ref(0, true, 1), cache_ref(1, false, 2), cache_ref(7, true, 0xFF)];
        let h = DistHeader { flags: DIST_HEADER_FLAG_ATOM_CACHE, cache_refs: refs };
        let bytes = encode_header(&h);
        let len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
        assert_eq!(len, 1 + 3 * h.cache_refs.len());
        assert_eq!(bytes.len(), 5 + len);
        // `(len - 1) / 3` refs are read back, in order.
        assert_eq!(DistHeader::decode(&bytes).expect("valid header"), h);
    }

    #[test]
    fn dist_header_negatives() {
        // Empty / too short to hold `tag + len + flags`.
        assert!(bad_arg(DistHeader::decode(&[])));
        assert!(bad_arg(DistHeader::decode(&[DIST_HEADER, 0, 0, 0])));
        // Wrong tag.
        assert!(bad_arg(DistHeader::decode(&[ATOM_CACHE_REF, 0, 0, 0, 1, 0])));
        // Declared length zero: the flags byte itself is always counted.
        assert!(bad_arg(DistHeader::decode(&[DIST_HEADER, 0, 0, 0, 0, 0])));
        // Declared length runs past the end of the body (short ref field).
        assert!(bad_arg(DistHeader::decode(&[DIST_HEADER, 0, 0, 0, 7, 0])));
        assert!(bad_arg(DistHeader::decode(&[
            DIST_HEADER,
            0,
            0,
            0,
            4,
            DIST_HEADER_FLAG_ATOM_CACHE,
            ATOM_CACHE_REF,
            0x00,
        ])));
        // Ref field size not a multiple of the 3-byte ref shape.
        assert!(bad_arg(DistHeader::decode(&[
            DIST_HEADER,
            0,
            0,
            0,
            3,
            DIST_HEADER_FLAG_ATOM_CACHE,
            ATOM_CACHE_REF,
            0x00,
        ])));
        // Atom-cache flag unset but ref bytes present: rejected, never dropped.
        assert!(bad_arg(DistHeader::decode(&[
            DIST_HEADER,
            0,
            0,
            0,
            4,
            0,
            ATOM_CACHE_REF,
            0x00,
            0x01,
        ])));
        // A ref inside the field must still carry the `'R'` tag.
        assert!(bad_arg(DistHeader::decode(&[
            DIST_HEADER,
            0,
            0,
            0,
            4,
            DIST_HEADER_FLAG_ATOM_CACHE,
            0x00,
            0x00,
            0x01,
        ])));
    }

    #[test]
    fn dist_fragment_from_tag() {
        assert_eq!(DistFragment::from_tag(DIST_FRAG_HEADER), Some(DistFragment::Header));
        assert_eq!(DistFragment::from_tag(b'E'), Some(DistFragment::Header));
        assert_eq!(DistFragment::from_tag(DIST_FRAG_CONT), Some(DistFragment::Continue));
        assert_eq!(DistFragment::from_tag(b'F'), Some(DistFragment::Continue));
        assert_eq!(DistFragment::from_tag(b'X'), None);
        assert_eq!(DistFragment::from_tag(DIST_HEADER), None);
        assert_eq!(DistFragment::from_tag(HOPEFUL_DATA), None);
        assert_eq!(DistFragment::from_tag(NEW_CACHE), None);
    }

    #[test]
    fn tag_constants_match_external_h() {
        // `erts/emulator/beam/external.h:65-69,80`.
        assert_eq!(DIST_HEADER, 68);
        assert_eq!(DIST_FRAG_HEADER, 69);
        assert_eq!(DIST_FRAG_CONT, 70);
        assert_eq!(HOPEFUL_DATA, 72);
        assert_eq!(ATOM_CACHE_REF, 82);
        assert_eq!(NEW_CACHE, 78);
        assert_eq!(DIST_HEADER_FLAG_ATOM_CACHE, 0x01);
    }
}
