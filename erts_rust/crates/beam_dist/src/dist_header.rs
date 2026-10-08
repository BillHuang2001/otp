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

//! The distribution header (`'D'`): its atom-cache refs and the fragment tags.
//!
//! # The wire format (byte-exact, big-endian)
//!
//! A **normal** (non-fragmented) distribution header, exactly as documented in
//! `erts/doc/guides/erl_ext_dist.md` §"Normal Distribution Header" and produced
//! by `erts_encode_ext_dist_header_finalize` (`erts/emulator/beam/external.c`):
//!
//! ```text
//! byte   131            VERSION_MAGIC
//! byte   68 ('D')       DIST_HEADER
//! byte   N              NumberOfAtomCacheRefs           (0 <= N <= 255)
//! bytes  Flags          N/2 + 1 bytes when N > 0; OMITTED entirely when N == 0
//! bytes  AtomCacheRefs  N refs, in AtomCacheReferenceIndex order 0 .. N-1
//! ```
//!
//! ## Flags
//!
//! Flag byte `k` holds the half-byte flag of ref `2k` in its **low** nibble and of
//! ref `2k+1` in its **high** nibble. Each ref half-byte is a 1-bit
//! `NewCacheEntryFlag` in the MSB plus the 3-bit `SegmentIndex` in the low bits
//! (`(segment & 0x7) | (new << 3)`).
//!
//! One extra half-byte carries `LongAtoms`: when `N` is **even** it is the low
//! nibble of the byte that follows the ref-flag bytes (flag byte `N/2`, the extra
//! byte); when `N` is **odd** it is the high nibble of the last ref-flag byte
//! (flag byte `(N-1)/2`). `LongAtoms` is the LSB of that half-byte; the other
//! three bits are `CurrentlyUnused` (zero).
//!
//! ## AtomCacheRefs
//!
//! Each ref is one of:
//! * `CachedAtomRef` — `InternalSegmentIndex(1 byte)`.
//! * `NewAtomCacheRef` — `InternalSegmentIndex(1 byte)` + `Length` +
//!   `AtomText(Length bytes)`, where `Length` is 1 byte, or 2 bytes big-endian
//!   when `LongAtoms` is set. `AtomText` is UTF-8 when [`DFLAG_UTF8_ATOMS`] was
//!   negotiated in the handshake, otherwise Latin-1; this codec models that as a
//!   caller-selected [`AtomEncoding`].
//!
//! The 3-bit `SegmentIndex` lives in the flag half-byte; only the
//! `InternalSegmentIndex` byte is written alongside the ref. Together they
//! identify a slot in the 8-segment × 256-entry distribution atom cache.
//!
//! [`DFLAG_UTF8_ATOMS`]: crate::flags::DFLAG_UTF8_ATOMS
//!
//! # Pass-through: when there is no header at all
//!
//! With neither an atom cache (`DFLAG_DIST_HDR_ATOM_CACHE`) nor fragmentation
//! (`DFLAG_FRAGMENTS`) negotiated, ERTS writes a single `PASS_THROUGH` byte in
//! place of the header (`external.c` `erts_encode_ext_dist_header_setup`), and the
//! control term that follows carries its own `VERSION_MAGIC`. This codec models
//! **only the header-when-present** case; the pass-through form is a wire-level
//! decision made by the distribution sender/receiver, not a header codec.
//!
//! # Fragmented header (`'E'`/`'F'`)
//!
//! A fragmented message's first frame extends the normal header with an 8-byte
//! `SequenceId` and an 8-byte fragment count (`FragmentId`) **before**
//! `NumberOfAtomCacheRefs`:
//!
//! ```text
//! byte   131            VERSION_MAGIC
//! byte   69 ('E')       DIST_FRAG_HEADER
//! bytes  SequenceId     8 bytes, big-endian
//! bytes  FragmentId     8 bytes, big-endian (the total fragment count)
//! bytes  <header tail>  identical to the tail of a normal `'D'` header:
//!                       NumberOfAtomCacheRefs + Flags + AtomCacheRefs
//! ```
//!
//! There is **no** separate `dist_hdr_flags` byte on the wire: `external.c`
//! `erts_encode_ext_dist_header_finalize` folds the `LongAtoms` flag into the
//! `Flags` half-bytes, and the receiver (`erts_prepare_dist_ext`) reads
//! `NumberOfAtomCacheRefs` immediately after the 16-byte fragment prefix. The tail
//! is therefore byte-identical to a `'D'` tail, which is why [`DistHeader::decode_tail`]
//! is shared by both. [`decode_fragmented_header`] parses a complete `'E'` frame
//! (prefix + tail) and reports where its payload begins.
//!
//! The `'F'` continuation (`VERSION_MAGIC + 'F'` + sequence id + fragment id) has
//! **no** tail at all; its codec and the sender-side splitting / receiver-side
//! reassembly live in [`crate::fragment`]. The tags are also recognised here
//! through [`DistFragment`].
//!
//! Ground truth (read, never modified): `erts/emulator/beam/external.c`
//! (`erts_encode_ext_dist_header_setup`/`_fragment`/`_finalize`,
//! `erts_prepare_dist_ext`), `erts/emulator/beam/external.h` and the
//! `erts/doc/guides/erl_ext_dist.md` guide.

use beam_codec::AtomEncoding;
use beam_common::{BeamError, Result};
use beam_term::{Atom, AtomTable};

use crate::fragment::{FragmentHeader, DIST_FRAGMENT_HEADER_SIZE};
use crate::transport::Write;

/// `DIST_HEADER` (`'D'`, 68): the normal distribution-header tag.
///
/// The value is re-exported from [`beam_codec::DIST_HEADER_EXT`] rather than
/// redefined, so the byte is pinned in exactly one place.
pub const DIST_HEADER: u8 = beam_codec::DIST_HEADER_EXT;
/// `DIST_FRAG_HEADER` (`'E'`, 69): the first fragment of a fragmented message.
///
/// Wire shape: `VERSION_MAGIC + 'E' + SequenceId(8) + FragmentId(8)` followed by
/// the normal header tail; see [`decode_fragmented_header`].
pub const DIST_FRAG_HEADER: u8 = 69;
/// `DIST_FRAG_CONT` (`'F'`, 70): a continuation fragment of a fragmented message.
///
/// Wire shape: `VERSION_MAGIC + 'F' + SequenceId(8) + FragmentId(8)` followed by
/// the raw payload chunk; see [`crate::fragment::FragmentCont`].
pub const DIST_FRAG_CONT: u8 = 70;

/// `ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES` (`erts/emulator/beam/external.h:120`):
/// the largest `NumberOfAtomCacheRefs` a single distribution header may carry.
///
/// The count field is one byte wide, so this is the natural ceiling (`255`); the
/// decoder additionally rejects any count whose refs do not fit the body.
pub const MAX_ATOM_CACHE_REFS: usize = 255;

/// The number of distribution atom-cache segments: 8 segments × 256 entries =
/// 2048 slots (`erl_ext_dist.md` §"Normal Distribution Header").
pub const ATOM_CACHE_SEGMENTS: u8 = 8;

/// One entry of a distribution header's `AtomCacheRefs` field.
///
/// The 3-bit [`segment`](AtomCacheRef::segment) is stored in the header's
/// half-byte flag field; the 8-bit [`index`](AtomCacheRef::index) is the
/// `InternalSegmentIndex` written next to the reference on the wire.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AtomCacheRef {
    /// A `CachedAtomRef`: a reference to an atom already present in the cache.
    ///
    /// Wire shape: `InternalSegmentIndex(1 byte)`.
    Cached {
        /// The 3-bit `SegmentIndex` (`0..=7`), carried in the flag half-byte.
        segment: u8,
        /// The 8-bit `InternalSegmentIndex`, carried next to the reference.
        index: u8,
    },
    /// A `NewAtomCacheRef`: this header installs the atom into the cache.
    ///
    /// Wire shape: `InternalSegmentIndex(1 byte)` + `Length` + `AtomText`.
    New {
        /// The 3-bit `SegmentIndex` (`0..=7`), carried in the flag half-byte.
        segment: u8,
        /// The 8-bit `InternalSegmentIndex`, carried next to the reference.
        index: u8,
        /// The atom identity; its name (rendered from the [`AtomTable`]) is the
        /// `AtomText`.
        atom: Atom,
    },
}

impl AtomCacheRef {
    /// The ref's 3-bit `SegmentIndex` (the high part of the cache slot address).
    pub fn segment(&self) -> u8 {
        match self {
            AtomCacheRef::Cached { segment, .. } | AtomCacheRef::New { segment, .. } => *segment,
        }
    }

    /// The ref's 8-bit `InternalSegmentIndex` (the low part of the cache slot
    /// address).
    pub fn index(&self) -> u8 {
        match self {
            AtomCacheRef::Cached { index, .. } | AtomCacheRef::New { index, .. } => *index,
        }
    }

    /// Whether the half-byte's `NewCacheEntryFlag` is set (a
    /// `NewAtomCacheRef` rather than a `CachedAtomRef`).
    pub fn is_new(&self) -> bool {
        matches!(self, AtomCacheRef::New { .. })
    }
}

/// A decoded normal distribution header: `VERSION_MAGIC + 'D' + N + Flags + refs`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct DistHeader {
    /// The `AtomCacheRefs`, in `AtomCacheReferenceIndex` order (`0 .. N-1`).
    pub refs: Vec<AtomCacheRef>,
    /// The `LongAtoms` flag: when set, every `NewAtomCacheRef` length field is a
    /// 2-byte big-endian integer instead of 1 byte, and each ref-flag byte has
    /// one fewer usable half-byte.
    ///
    /// The flag is only present on the wire when there is at least one ref; it is
    /// reported as `false` for an empty header.
    pub long_atoms: bool,
}

impl DistHeader {
    /// An empty header (`N == 0`): exactly `[VERSION_MAGIC, 'D', 0]` on the wire.
    pub fn empty() -> DistHeader {
        DistHeader { refs: Vec::new(), long_atoms: false }
    }

    /// Encodes the header as `VERSION_MAGIC + 'D' + N + Flags + refs`.
    ///
    /// `atoms` renders each [`AtomCacheRef::New`]'s `AtomText`, encoded as UTF-8
    /// or Latin-1 per `encoding` (the [`DFLAG_UTF8_ATOMS`] negotiation).
    ///
    /// Errors: more than [`MAX_ATOM_CACHE_REFS`] refs, or a `NewAtomCacheRef`
    /// whose atom text overflows the selected length field (255 bytes, or 65535
    /// with [`long_atoms`](DistHeader::long_atoms)), yield `BeamError::SystemLimit`;
    /// an atom missing from `atoms`, or a name that cannot be represented in the
    /// selected `encoding` (a Latin-1 code point above `U+00FF`), yields
    /// `BeamError::BadArg`.
    ///
    /// [`DFLAG_UTF8_ATOMS`]: crate::flags::DFLAG_UTF8_ATOMS
    pub fn encode<W: Write>(
        &self,
        w: &mut W,
        atoms: &AtomTable,
        encoding: AtomEncoding,
    ) -> Result<()> {
        let n = self.refs.len();
        if n > MAX_ATOM_CACHE_REFS {
            return Err(BeamError::SystemLimit);
        }

        w.write_u8(beam_codec::VERSION_MAGIC)?;
        w.write_u8(DIST_HEADER)?;
        w.write_u8(n as u8)?;

        if n == 0 {
            // No refs: `Flags` and `AtomCacheRefs` are omitted entirely.
            return Ok(());
        }

        // Build the dynamic flag field: `N/2 + 1` bytes, the last half-byte of
        // which is the extra `LongAtoms` half-byte.
        let mut flags = vec![0u8; n / 2 + 1];
        for (i, cache_ref) in self.refs.iter().enumerate() {
            let half = (cache_ref.segment() & 0x7) | if cache_ref.is_new() { 0x8 } else { 0 };
            if i % 2 == 0 {
                flags[i / 2] |= half;
            } else {
                flags[i / 2] |= half << 4;
            }
        }
        if self.long_atoms {
            // N even: low nibble of the extra byte `N/2`.
            // N odd:  high nibble of the last ref-flag byte `(N-1)/2 == N/2`.
            let extra = flags[n / 2];
            flags[n / 2] = if n % 2 == 0 { extra | 0x1 } else { extra | (0x1 << 4) };
        }
        w.write_all(&flags)?;

        for cache_ref in &self.refs {
            w.write_u8(cache_ref.index())?;
            if let AtomCacheRef::New { atom, .. } = cache_ref {
                let name = atoms.name(*atom).ok_or(BeamError::BadArg)?;
                let text = encode_atom_text(name, encoding)?;
                let len = text.len();
                if self.long_atoms {
                    // 2-byte big-endian length (`u16` field).
                    let len = u16::try_from(len).map_err(|_| BeamError::SystemLimit)?;
                    w.write_u16be(len)?;
                } else {
                    // 1-byte length (`u8` field).
                    let len = u8::try_from(len).map_err(|_| BeamError::SystemLimit)?;
                    w.write_u8(len)?;
                }
                w.write_all(&text)?;
            }
        }
        Ok(())
    }

    /// Decodes a normal distribution header from the start of `body`.
    ///
    /// `atoms` interns the `AtomText` of every [`AtomCacheRef::New`]; `encoding`
    /// selects how those bytes are rendered (UTF-8 or Latin-1).
    ///
    /// Bytes after the header — the control term / message that follows it in a
    /// distribution packet — are left to the caller.
    ///
    /// Errors: a body that is too short, does not start with `VERSION_MAGIC` +
    /// `'D'`, or whose UTF-8 atom text is malformed, yields `BeamError::BadArg`.
    pub fn decode(
        body: &[u8],
        atoms: &mut AtomTable,
        encoding: AtomEncoding,
    ) -> Result<DistHeader> {
        // `VERSION_MAGIC + 'D'`
        if body.len() < 2 || body[0] != beam_codec::VERSION_MAGIC || body[1] != DIST_HEADER {
            return Err(BeamError::BadArg);
        }
        let (header, _end) = DistHeader::decode_tail(body, 2, atoms, encoding)?;
        Ok(header)
    }

    /// Decodes the distribution-header **tail** — `NumberOfAtomCacheRefs(1 byte)
    /// + Flags + AtomCacheRefs` — beginning at `body[offset]`.
    ///
    /// This is the tail shared by the normal `'D'` header (where it starts at
    /// offset 2) and the `'E'` fragment header (where it starts at offset
    /// [`DIST_FRAGMENT_HEADER_SIZE`]); [`decode_fragmented_header`] uses it for the
    /// latter. Returns the decoded header and the offset just past the tail, where
    /// the control message / payload of the packet begins.
    ///
    /// `atoms` interns the `AtomText` of every [`AtomCacheRef::New`] and
    /// `encoding` selects how those bytes are rendered. Bytes after the tail are
    /// left to the caller.
    ///
    /// Errors: a missing/short `NumberOfAtomCacheRefs`/`Flags`/ref field, or
    /// malformed UTF-8 atom text, yields `BeamError::BadArg`.
    pub fn decode_tail(
        body: &[u8],
        offset: usize,
        atoms: &mut AtomTable,
        encoding: AtomEncoding,
    ) -> Result<(DistHeader, usize)> {
        let n = usize::from(*body.get(offset).ok_or(BeamError::BadArg)?);
        let pos = offset + 1;
        if n == 0 {
            return Ok((DistHeader::empty(), pos));
        }

        // The dynamic flag field: `N/2 + 1` bytes.
        let flags_bytes = n / 2 + 1;
        let flags_end = pos.checked_add(flags_bytes).ok_or(BeamError::BadArg)?;
        if body.len() < flags_end {
            return Err(BeamError::BadArg);
        }
        let flags = &body[pos..flags_end];

        // The extra `LongAtoms` half-byte: low nibble of byte `N/2` when N is
        // even, high nibble of byte `N/2` when N is odd.
        let long_half = if n % 2 == 0 { flags[n / 2] & 0x0F } else { flags[n / 2] >> 4 };
        let long_atoms = long_half & 0x1 != 0;

        let mut p = flags_end;
        let mut refs = Vec::with_capacity(n);
        for i in 0..n {
            let half = if i % 2 == 0 { flags[i / 2] & 0x0F } else { flags[i / 2] >> 4 };
            let segment = half & 0x7;
            let is_new = half & 0x8 != 0;

            let index = *body.get(p).ok_or(BeamError::BadArg)?;
            p += 1;

            if !is_new {
                refs.push(AtomCacheRef::Cached { segment, index });
                continue;
            }

            let len = if long_atoms {
                let hi = *body.get(p).ok_or(BeamError::BadArg)?;
                let lo = *body.get(p + 1).ok_or(BeamError::BadArg)?;
                p += 2;
                usize::from(u16::from_be_bytes([hi, lo]))
            } else {
                let len = usize::from(*body.get(p).ok_or(BeamError::BadArg)?);
                p += 1;
                len
            };

            let text_end = p.checked_add(len).ok_or(BeamError::BadArg)?;
            let text = body.get(p..text_end).ok_or(BeamError::BadArg)?;
            p = text_end;

            let name = decode_atom_text(text, encoding)?;
            let atom = atoms.intern(&name);
            refs.push(AtomCacheRef::New { segment, index, atom });
        }

        Ok((DistHeader { refs, long_atoms }, p))
    }
}

/// Renders an atom name as the `AtomText` for the selected [`AtomEncoding`].
///
/// UTF-8 is the raw name bytes; Latin-1 is one byte per code point and only covers
/// `U+0000..=U+00FF`, so a wider character is reported as `BeamError::BadArg`
/// rather than silently mis-encoded (the same rule as `beam_codec`'s encoder).
fn encode_atom_text(name: &str, encoding: AtomEncoding) -> Result<Vec<u8>> {
    match encoding {
        AtomEncoding::Utf8 => Ok(name.as_bytes().to_vec()),
        AtomEncoding::Latin1 => {
            let mut bytes = Vec::with_capacity(name.len());
            for ch in name.chars() {
                let code = ch as u32;
                if code > 0xFF {
                    return Err(BeamError::BadArg);
                }
                bytes.push(code as u8);
            }
            Ok(bytes)
        }
    }
}

/// Renders `AtomText` back into an atom name for the selected [`AtomEncoding`].
///
/// Latin-1 maps each byte `b` to the code point `U+00b` (a total, infallible
/// conversion); UTF-8 is validated, and malformed bytes are `BeamError::BadArg`.
fn decode_atom_text(text: &[u8], encoding: AtomEncoding) -> Result<String> {
    match encoding {
        AtomEncoding::Utf8 => {
            let name = std::str::from_utf8(text).map_err(|_| BeamError::BadArg)?;
            Ok(name.to_string())
        }
        AtomEncoding::Latin1 => Ok(text.iter().map(|&b| char::from(b)).collect()),
    }
}

/// The kind of a distribution fragment, recognised from its tag byte (`'E'`/`'F'`).
///
/// The tag also selects which codec parses the frame: [`decode_fragmented_header`]
/// for `'E'`, and [`crate::fragment::FragmentCont::decode`] for `'F'`. This enum
/// only names the tag byte.
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

/// A decoded `'E'` fragment-header frame: the fragment prefix, the distribution
/// header tail it carries, and where this fragment's payload begins.
///
/// Produced by [`decode_fragmented_header`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FragmentedHeader {
    /// The `'E'` prefix: the sequence id and the total fragment count.
    pub fragment: FragmentHeader,
    /// The distribution-header tail (`NumberOfAtomCacheRefs + Flags +
    /// AtomCacheRefs`), identical in shape to a `'D'` header's tail.
    pub header: DistHeader,
    /// The offset of this fragment's payload within the packet body — just past
    /// the atom-cache tail.
    pub payload_offset: usize,
}

/// Decodes an `'E'` fragment-header frame from the start of `body`.
///
/// Layout: `VERSION_MAGIC + 'E' + SequenceId(8, big-endian) + FragmentId(8,
/// big-endian) + <the normal header tail>`. The tail is parsed by
/// [`DistHeader::decode_tail`], so the `'D'` and `'E'` codecs share one
/// implementation of the `NumberOfAtomCacheRefs`/`Flags`/`AtomCacheRefs` format.
///
/// The returned [`FragmentedHeader::payload_offset`] is the index of the first
/// payload byte of this fragment within `body`.
///
/// Errors: a body that is too short, does not start with `VERSION_MAGIC` + `'E'`,
/// or whose tail is malformed, yields `BeamError::BadArg`.
pub fn decode_fragmented_header(
    body: &[u8],
    atoms: &mut AtomTable,
    encoding: AtomEncoding,
) -> Result<FragmentedHeader> {
    let fragment = FragmentHeader::decode(body)?;
    let (header, payload_offset) =
        DistHeader::decode_tail(body, DIST_FRAGMENT_HEADER_SIZE, atoms, encoding)?;
    Ok(FragmentedHeader { fragment, header, payload_offset })
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

    fn encode_result(h: &DistHeader, atoms: &AtomTable, encoding: AtomEncoding) -> Result<Vec<u8>> {
        let mut sink = Sink::default();
        h.encode(&mut sink, atoms, encoding)?;
        Ok(sink.0)
    }

    fn encode(h: &DistHeader, atoms: &AtomTable, encoding: AtomEncoding) -> Vec<u8> {
        encode_result(h, atoms, encoding).expect("encoding into a Vec cannot fail")
    }

    fn bad_arg<T: std::fmt::Debug>(res: Result<T>) -> bool {
        matches!(res, Err(BeamError::BadArg))
    }

    /// An atom table with a few ASCII names, plus the `Atom` handles for them.
    fn table_with(names: &[&str]) -> (AtomTable, Vec<Atom>) {
        let mut atoms = AtomTable::new();
        let handles = names.iter().map(|name| atoms.intern(name)).collect();
        (atoms, handles)
    }

    #[test]
    fn empty_header_is_exactly_three_bytes() {
        let atoms = AtomTable::new();
        let h = DistHeader::empty();
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        assert_eq!(bytes, vec![131, 68, 0]);
        assert_eq!(bytes, vec![beam_codec::VERSION_MAGIC, DIST_HEADER, 0]);

        let mut decode_atoms = AtomTable::new();
        let decoded =
            DistHeader::decode(&bytes, &mut decode_atoms, AtomEncoding::Utf8).expect("valid header");
        assert_eq!(decoded, h);
        assert!(!decoded.long_atoms);
    }

    #[test]
    fn one_cached_ref_places_the_flag_in_the_low_nibble() {
        // N = 1 (odd) => the LongAtoms half-byte is the HIGH nibble of the only
        // flag byte; ref 0's flags are the LOW nibble. `long_atoms = false`.
        let atoms = AtomTable::new();
        let h = DistHeader { refs: vec![AtomCacheRef::Cached { segment: 1, index: 7 }], long_atoms: false };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        // flags = (segment 1 | not-new) => low nibble 0x1, high nibble LongAtoms=0.
        assert_eq!(bytes, vec![131, 68, 1, 0x01, 7]);

        let mut decode_atoms = AtomTable::new();
        let decoded =
            DistHeader::decode(&bytes, &mut decode_atoms, AtomEncoding::Utf8).expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn one_new_ref_short_atom() {
        // Ref 0: New, segment = 3, index = 0x2A, atom "ab".
        let (atoms, handles) = table_with(&["ab"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 3, index: 0x2A, atom: handles[0] }],
            long_atoms: false,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        // flags low nibble = segment 3 | new 0x8 = 0x0B, high nibble = 0.
        assert_eq!(bytes, vec![131, 68, 1, 0x0B, 0x2A, 2, b'a', b'b']);

        let (mut round_atoms, _) = table_with(&["ab"]);
        let decoded = DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8)
            .expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn one_new_ref_long_atoms_uses_a_two_byte_length() {
        let (atoms, handles) = table_with(&["ab"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 3, index: 0x2A, atom: handles[0] }],
            long_atoms: true,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        // flags: low nibble = 0x0B, high nibble = LongAtoms 0x1 => 0x1B.
        // length is a 2-byte big-endian 2.
        assert_eq!(bytes, vec![131, 68, 1, 0x1B, 0x2A, 0x00, 0x02, b'a', b'b']);

        let (mut round_atoms, _) = table_with(&["ab"]);
        let decoded = DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8)
            .expect("valid header");
        assert_eq!(decoded, h);
        assert!(decoded.long_atoms);
    }

    #[test]
    fn two_refs_pack_into_one_flag_byte_and_have_a_separate_long_atoms_byte() {
        // N = 2 (even): flag byte 0 holds ref 0 (low nibble) + ref 1 (high
        // nibble); flag byte 1 is the extra LongAtoms byte (low nibble).
        let (atoms, handles) = table_with(&["x"]);
        let h = DistHeader {
            refs: vec![
                AtomCacheRef::Cached { segment: 0, index: 5 },
                AtomCacheRef::New { segment: 1, index: 9, atom: handles[0] },
            ],
            long_atoms: false,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        // byte0 = ref0 (0x0) | ref1 (segment 1 | new 0x8 = 0x9) << 4 = 0x90
        // byte1 = LongAtoms low nibble = 0x00
        assert_eq!(bytes, vec![131, 68, 2, 0x90, 0x00, 5, 9, 1, b'x']);

        let (mut round_atoms, _) = table_with(&["x"]);
        let decoded = DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8)
            .expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn two_refs_long_atoms_is_the_low_nibble_of_the_following_byte() {
        let (atoms, handles) = table_with(&["x"]);
        let h = DistHeader {
            refs: vec![
                AtomCacheRef::Cached { segment: 0, index: 5 },
                AtomCacheRef::New { segment: 1, index: 9, atom: handles[0] },
            ],
            long_atoms: true,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        // byte1 = 0x01: LongAtoms sits in the LOW nibble of the byte after the
        // ref-flag bytes; the New ref's length is the 2-byte big-endian 1.
        assert_eq!(bytes, vec![131, 68, 2, 0x90, 0x01, 5, 9, 0x00, 0x01, b'x']);

        let (mut round_atoms, _) = table_with(&["x"]);
        let decoded = DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8)
            .expect("valid header");
        assert_eq!(decoded, h);
        assert!(decoded.long_atoms);
    }

    #[test]
    fn odd_ref_count_puts_long_atoms_in_the_high_nibble_of_the_last_flag_byte() {
        // N = 3 (odd): flag byte 1 holds ref 2 (low) + LongAtoms (high).
        let (atoms, handles) = table_with(&["abcd"]);
        let h = DistHeader {
            refs: vec![
                AtomCacheRef::Cached { segment: 0, index: 1 },
                AtomCacheRef::Cached { segment: 2, index: 2 },
                AtomCacheRef::New { segment: 3, index: 3, atom: handles[0] },
            ],
            long_atoms: true,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        assert_eq!(
            bytes,
            vec![
                131, 68, 3, // header
                0x20, // ref0 low = 0x0, ref1 high = (2|0) << 4 = 0x20
                0x1B, // ref2 low = 3|8 = 0x0B, LongAtoms high = 0x10
                1, 2, 3, // the Index byte of each ref
                0x00, 0x04, b'a', b'b', b'c', b'd', // ref2's 2-byte length + text
            ]
        );

        let (mut round_atoms, _) = table_with(&["abcd"]);
        let decoded = DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8)
            .expect("valid header");
        assert_eq!(decoded, h);
        assert!(decoded.long_atoms);
    }

    #[test]
    fn empty_atom_text_round_trips() {
        let (atoms, handles) = table_with(&[""]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom: handles[0] }],
            long_atoms: false,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        assert_eq!(bytes, vec![131, 68, 1, 0x08, 0, 0]);

        let mut round_atoms = AtomTable::new();
        let decoded =
            DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8).expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn latin1_round_trips_a_non_ascii_name() {
        // U+00E5 (å) is one byte 0xE5 in Latin-1 (and two bytes in UTF-8).
        let (atoms, handles) = table_with(&["\u{00E5}"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom: handles[0] }],
            long_atoms: false,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Latin1);
        assert_eq!(bytes, vec![131, 68, 1, 0x08, 0, 1, 0xE5]);

        let mut round_atoms = AtomTable::new();
        let decoded =
            DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Latin1).expect("valid header");
        assert_eq!(decoded, h);
        assert_eq!(round_atoms.name(handles[0]), Some("\u{00E5}"));
    }

    #[test]
    fn utf8_round_trips_the_same_non_ascii_name_with_two_bytes() {
        let (atoms, handles) = table_with(&["\u{00E5}"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom: handles[0] }],
            long_atoms: false,
        };
        let bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        assert_eq!(bytes, vec![131, 68, 1, 0x08, 0, 2, 0xC3, 0xA5]);

        let mut round_atoms = AtomTable::new();
        let decoded =
            DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8).expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn trailing_payload_bytes_are_left_to_the_caller() {
        // The control term / message follows the header inside a dist packet.
        let (atoms, handles) = table_with(&["x"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 1, index: 9, atom: handles[0] }],
            long_atoms: false,
        };
        let mut bytes = encode(&h, &atoms, AtomEncoding::Utf8);
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);

        let (mut round_atoms, _) = table_with(&["x"]);
        let decoded = DistHeader::decode(&bytes, &mut round_atoms, AtomEncoding::Utf8)
            .expect("valid header");
        assert_eq!(decoded, h);
    }

    #[test]
    fn decode_rejects_short_and_wrong_prefix_bodies() {
        let mut atoms = AtomTable::new();
        // Empty / too short for `VERSION_MAGIC + 'D' + N`.
        assert!(bad_arg(DistHeader::decode(&[], &mut atoms, AtomEncoding::Utf8)));
        assert!(bad_arg(DistHeader::decode(&[131], &mut atoms, AtomEncoding::Utf8)));
        assert!(bad_arg(DistHeader::decode(&[131, 68], &mut atoms, AtomEncoding::Utf8)));
        // Not `VERSION_MAGIC`.
        assert!(bad_arg(DistHeader::decode(&[130, 68, 0], &mut atoms, AtomEncoding::Utf8)));
        // Not `'D'`.
        assert!(bad_arg(DistHeader::decode(&[131, 69, 0], &mut atoms, AtomEncoding::Utf8)));
        // A `'D'` term-level tag alone is not a header.
        assert!(bad_arg(DistHeader::decode(&[68], &mut atoms, AtomEncoding::Utf8)));
    }

    #[test]
    fn decode_rejects_truncated_flag_and_ref_fields() {
        let mut atoms = AtomTable::new();
        // N = 1 declares 1 flag byte; only N is present.
        assert!(bad_arg(DistHeader::decode(&[131, 68, 1], &mut atoms, AtomEncoding::Utf8)));
        // N = 2 declares 2 flag bytes; only 1 is present.
        assert!(bad_arg(DistHeader::decode(&[131, 68, 2, 0x00], &mut atoms, AtomEncoding::Utf8)));
        // Flags present but the CachedAtomRef index byte is missing.
        assert!(bad_arg(DistHeader::decode(&[131, 68, 1, 0x01], &mut atoms, AtomEncoding::Utf8)));
        // NewAtomCacheRef with a length field that runs past the body.
        assert!(bad_arg(DistHeader::decode(
            &[131, 68, 1, 0x0B, 0x2A, 5, b'a'],
            &mut atoms,
            AtomEncoding::Utf8
        )));
        // LongAtoms length field itself truncated.
        assert!(bad_arg(DistHeader::decode(
            &[131, 68, 1, 0x1B, 0x2A, 0x00],
            &mut atoms,
            AtomEncoding::Utf8
        )));
        // A second ref declared by N = 2 is missing (only one index byte present).
        assert!(bad_arg(DistHeader::decode(
            &[131, 68, 2, 0x00, 0x00, 5],
            &mut atoms,
            AtomEncoding::Utf8
        )));
    }

    #[test]
    fn decode_rejects_malformed_utf8_atom_text() {
        let mut atoms = AtomTable::new();
        // 0xFF is not valid UTF-8.
        assert!(bad_arg(DistHeader::decode(
            &[131, 68, 1, 0x08, 0, 1, 0xFF],
            &mut atoms,
            AtomEncoding::Utf8
        )));
        // The same bytes are a valid Latin-1 name (U+00FF).
        let mut latin_atoms = AtomTable::new();
        let decoded = DistHeader::decode(&[131, 68, 1, 0x08, 0, 1, 0xFF], &mut latin_atoms, AtomEncoding::Latin1)
            .expect("valid latin-1 header");
        assert_eq!(
            decoded.refs,
            vec![AtomCacheRef::New { segment: 0, index: 0, atom: latin_atoms.lookup("\u{00FF}").unwrap() }]
        );
    }

    #[test]
    fn encode_rejects_unrepresentable_latin1_and_oversize_short_length() {
        // U+0141 (Ł) is above U+00FF: not representable in Latin-1.
        let (atoms, handles) = table_with(&["\u{0141}"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom: handles[0] }],
            long_atoms: false,
        };
        assert!(bad_arg(encode_result(&h, &atoms, AtomEncoding::Latin1)));

        // A 300-byte name cannot fit the 1-byte length field.
        let long = "a".repeat(300);
        let mut big_atoms = AtomTable::new();
        let atom = big_atoms.intern(&long);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom }],
            long_atoms: false,
        };
        assert!(matches!(
            encode_result(&h, &big_atoms, AtomEncoding::Utf8),
            Err(BeamError::SystemLimit)
        ));
        // ... but it fits a 2-byte (LongAtoms) length field.
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 0, index: 0, atom }],
            long_atoms: true,
        };
        assert!(encode_result(&h, &big_atoms, AtomEncoding::Utf8).is_ok());
    }

    #[test]
    fn encode_rejects_more_than_the_count_field_can_hold() {
        let atoms = AtomTable::new();
        let refs = (0..=MAX_ATOM_CACHE_REFS as u8)
            .map(|i| AtomCacheRef::Cached { segment: i % 8, index: i })
            .collect();
        let h = DistHeader { refs, long_atoms: false };
        assert!(matches!(
            encode_result(&h, &atoms, AtomEncoding::Utf8),
            Err(BeamError::SystemLimit)
        ));
    }

    #[test]
    fn dist_fragment_from_tag() {
        assert_eq!(DistFragment::from_tag(DIST_FRAG_HEADER), Some(DistFragment::Header));
        assert_eq!(DistFragment::from_tag(b'E'), Some(DistFragment::Header));
        assert_eq!(DistFragment::from_tag(DIST_FRAG_CONT), Some(DistFragment::Continue));
        assert_eq!(DistFragment::from_tag(b'F'), Some(DistFragment::Continue));
        assert_eq!(DistFragment::from_tag(b'X'), None);
        assert_eq!(DistFragment::from_tag(DIST_HEADER), None);
    }

    #[test]
    fn tag_constants_match_external_h() {
        // `erts/emulator/beam/external.h`.
        assert_eq!(beam_codec::VERSION_MAGIC, 131);
        assert_eq!(DIST_HEADER, 68);
        assert_eq!(DIST_FRAG_HEADER, 69);
        assert_eq!(DIST_FRAG_CONT, 70);
    }

    #[test]
    fn ref_accessors_mirror_the_wire_fields() {
        let cached = AtomCacheRef::Cached { segment: 5, index: 0x1F };
        assert_eq!(cached.segment(), 5);
        assert_eq!(cached.index(), 0x1F);
        assert!(!cached.is_new());

        let new = AtomCacheRef::New { segment: 6, index: 0x2A, atom: Atom::from_raw(1) };
        assert_eq!(new.segment(), 6);
        assert_eq!(new.index(), 0x2A);
        assert!(new.is_new());
    }

    #[test]
    fn decode_tail_matches_decode_for_a_normal_header() {
        // For a `'D'` body the tail starts at offset 2.
        let (atoms, handles) = table_with(&["x"]);
        let h = DistHeader {
            refs: vec![AtomCacheRef::New { segment: 1, index: 9, atom: handles[0] }],
            long_atoms: false,
        };
        let body = encode(&h, &atoms, AtomEncoding::Utf8);
        let mut tail_atoms = AtomTable::new();
        let (decoded, end) =
            DistHeader::decode_tail(&body, 2, &mut tail_atoms, AtomEncoding::Utf8)
                .expect("valid tail");
        assert_eq!(decoded, h);
        // `[131, 'D', N, flags, index, len, 'x']` -> the tail ends at byte 7.
        assert_eq!(end, body.len());
    }

    #[test]
    fn decode_tail_reads_the_empty_tail() {
        let mut atoms = AtomTable::new();
        let (header, end) = DistHeader::decode_tail(&[0, 0xFF], 0, &mut atoms, AtomEncoding::Utf8)
            .expect("N = 0 tail");
        assert_eq!(header, DistHeader::empty());
        assert_eq!(end, 1);
        // An out-of-range offset is a short body.
        assert!(bad_arg(DistHeader::decode_tail(&[0], 5, &mut atoms, AtomEncoding::Utf8)));
    }

    #[test]
    fn decode_fragmented_header_is_byte_exact_with_the_guide_example() {
        // The worked example from `erl_ext_dist.md` §"Distribution Header for
        // fragmented messages", decoded through the shared tail logic: the first
        // 5 atom cache refs (`reg`, `call`, `set_get_state`, two cached).
        let body = [
            131, 69, // VERSION_MAGIC 'E'
            0, 0, 2, 168, 0, 0, 5, 83, // SequenceId(8)
            0, 0, 0, 0, 0, 0, 0, 2, // FragmentId(8) = 2
            5, 4, 137, 9, // NumberOfAtomCacheRefs = 5, 3 flags bytes
            10, 5, // two CachedAtomRefs
            236, 3, 114, 101, 103, // NewAtomCacheRef 'reg'
            9, 4, 99, 97, 108, 108, // NewAtomCacheRef 'call'
            238, 13, 115, 101, 116, 95, 103, 101, 116, 95, 115, 116, 97, 116, 101, // 'set_get_state'
            104, 4, 97, 6, // control message follows the header tail
        ];
        let mut atoms = AtomTable::new();
        let frame = decode_fragmented_header(&body, &mut atoms, AtomEncoding::Utf8)
            .expect("valid `'E'` frame");
        assert_eq!(frame.fragment.seq_id, 0x0000_02A8_0000_0553);
        assert_eq!(frame.fragment.fragment_count, 2);
        assert!(!frame.header.long_atoms);
        assert_eq!(frame.header.refs.len(), 5);
        // Flag bytes `4, 137, 9` -> half-bytes (low nibble of even refs, high
        // nibble of odd refs) `0100, 0000, 1001, 1000, 1001`: refs 0 & 1 are
        // cached (MSB 0, segments 4 and 0), refs 2/3/4 are new (segments 1, 0, 1).
        assert_eq!(frame.header.refs[0], AtomCacheRef::Cached { segment: 4, index: 10 });
        assert_eq!(frame.header.refs[1], AtomCacheRef::Cached { segment: 0, index: 5 });
        let name_of = |r: &AtomCacheRef| match r {
            AtomCacheRef::New { atom, .. } => atoms.name(*atom).map(str::to_string),
            AtomCacheRef::Cached { .. } => None,
        };
        assert_eq!(name_of(&frame.header.refs[2]).as_deref(), Some("reg"));
        assert_eq!(name_of(&frame.header.refs[3]).as_deref(), Some("call"));
        assert_eq!(name_of(&frame.header.refs[4]).as_deref(), Some("set_get_state"));
        // The payload (the control message) starts right after the atom-cache tail.
        assert_eq!(frame.payload_offset, 50);
        assert_eq!(&body[frame.payload_offset..], &[104, 4, 97, 6]);
    }

    #[test]
    fn decode_fragmented_header_accepts_an_empty_tail() {
        let body = [
            131, 69, // VERSION_MAGIC 'E'
            0, 0, 0, 0, 0, 0, 0, 42, // SequenceId(8)
            0, 0, 0, 0, 0, 0, 0, 1, // FragmentId(8)
            0, // NumberOfAtomCacheRefs = 0
            0xAA, 0xBB, // first payload bytes
        ];
        let mut atoms = AtomTable::new();
        let frame = decode_fragmented_header(&body, &mut atoms, AtomEncoding::Utf8)
            .expect("valid `'E'` frame");
        assert_eq!(frame.fragment.seq_id, 42);
        assert_eq!(frame.fragment.fragment_count, 1);
        assert_eq!(frame.header, DistHeader::empty());
        assert_eq!(frame.payload_offset, 19);
        assert_eq!(&body[frame.payload_offset..], &[0xAA, 0xBB]);
    }

    #[test]
    fn decode_fragmented_header_rejects_bad_frames() {
        let mut atoms = AtomTable::new();
        // Too short for the 18-byte fragment prefix.
        assert!(bad_arg(decode_fragmented_header(&[131, 69], &mut atoms, AtomEncoding::Utf8)));
        // A `'D'` header is not an `'E'` fragment header.
        assert!(bad_arg(decode_fragmented_header(&[131, 68, 0], &mut atoms, AtomEncoding::Utf8)));
        // Prefix present but the tail's `NumberOfAtomCacheRefs` is missing.
        let mut prefix = vec![131, 69];
        prefix.extend_from_slice(&[0u8; 16]);
        assert!(bad_arg(decode_fragmented_header(&prefix, &mut atoms, AtomEncoding::Utf8)));
        // Prefix + N = 1 but no flag byte / ref.
        let mut truncated = vec![131, 69];
        truncated.extend_from_slice(&[0u8; 16]);
        truncated.push(1);
        assert!(bad_arg(decode_fragmented_header(&truncated, &mut atoms, AtomEncoding::Utf8)));
    }
}
