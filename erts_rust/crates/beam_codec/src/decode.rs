//! The `Decoder` seam: external term format bytes → terms.
//!
//! Decoding is **allocation-aware**: the decoded term is materialized in the
//! caller-supplied [`beam_alloc::Heap`] (production heap, not the `beam_term`
//! test arena) and every atom is interned into the caller's [`AtomTable`] in wire
//! order. The same code path therefore serves `binary_to_term`, distribution
//! payloads and ETS blobs.
//!
//! This module is safe Rust; the raw heap writes it needs live in
//! [`crate::term`].

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::{AtomTable, Eterm};

use crate::atom_cache::AtomCache;
use crate::reader::Reader;

/// Incremental external term format decoder.
///
/// `new` requires and consumes the leading version byte `131`; `new_body`
/// decodes a body that has none (the distribution payload following a
/// distribution header, or an ETS term blob).
pub struct Decoder<'a> {
    /// The wire cursor.
    reader: Reader<'a>,
    /// The distribution atom cache, when an `ATOM_CACHE_REF` may appear.
    cache: Option<&'a AtomCache>,
}

impl<'a> Decoder<'a> {
    /// Creates a decoder for a `term_to_binary` image.
    ///
    /// Requires and consumes the leading [`VERSION_MAGIC`](crate::VERSION_MAGIC)
    /// byte; a missing or mismatched version byte yields `BeamError::BadArg`.
    pub fn new(bytes: &'a [u8]) -> Result<Decoder<'a>> {
        unimplemented!()
    }

    /// Creates a decoder for a body: **no** leading version byte.
    pub fn new_body(bytes: &'a [u8]) -> Decoder<'a> {
        unimplemented!()
    }

    /// Attaches the distribution atom cache used to resolve `ATOM_CACHE_REF` (`82`).
    pub fn with_atom_cache(self, cache: &'a AtomCache) -> Decoder<'a> {
        unimplemented!()
    }

    /// Decodes the next term into `heap`, interning its atoms into `atoms`.
    ///
    /// Malformed/truncated input, a bad tag, invalid UTF-8 in an atom name or an
    /// unset atom-cache slot yield `BeamError::BadArg`; a value beyond a wire
    /// bound yields `BeamError::SystemLimit`; heap exhaustion propagates from the
    /// heap as `BeamError::Alloc`; a tag the M1 term layer does not support yet
    /// yields [`crate::error::unsupported`].
    pub fn decode(&mut self, heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
        unimplemented!()
    }

    /// The current cursor offset into the input.
    pub fn position(&self) -> usize {
        unimplemented!()
    }

    /// The number of bytes not yet decoded.
    pub fn remaining(&self) -> usize {
        unimplemented!()
    }

    /// Consumes the decoder, asserting that every byte was consumed.
    ///
    /// Trailing bytes yield `BeamError::BadArg`.
    pub fn finish(self) -> Result<()> {
        unimplemented!()
    }
}

/// Decodes a `term_to_binary` image into `heap`, interning atoms into `atoms`.
///
/// Convenience over [`Decoder::new`] + [`Decoder::decode`] + [`Decoder::finish`].
pub fn decode_term(bytes: &[u8], heap: &mut Heap, atoms: &mut AtomTable) -> Result<Eterm> {
    unimplemented!()
}
