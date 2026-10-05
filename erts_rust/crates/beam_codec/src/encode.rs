//! The `Encoder` seam: terms → external term format bytes.
//!
//! One encoder serves both `term_to_binary` bytes (with the leading
//! [`VERSION_MAGIC`](crate::VERSION_MAGIC)) and distribution payload/ETS bodies
//! (without it) — the constructor decides which. Term inspection goes through
//! [`crate::term`]; this module owns only the *bytes*.
//!
//! This module is safe Rust.

use beam_common::Result;
use beam_term::{AtomTable, Eterm};

use crate::writer::Writer;

/// Which atom tag the encoder emits.
///
/// Atoms are interned, never compared by name: the encoder renders each name
/// from the caller's [`AtomTable`] in the selected encoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AtomEncoding {
    /// Emit `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT` — exactly what `term_to_binary`
    /// emits. This is the default.
    Utf8,
    /// Emit `SMALL_ATOM_EXT`/`ATOM_EXT` (Latin-1) for nodes that did not
    /// negotiate `DFLAG_UTF8_ATOMS` (used by `beam_dist`).
    Latin1,
}

/// Incremental external term format encoder.
///
/// Holds a borrowed atom table (the sole source of atom names) and an output
/// buffer; `new` emits the leading version byte, `new_body` does not.
pub struct Encoder<'a> {
    /// The atom table atom names are rendered from.
    atoms: &'a AtomTable,
    /// The output buffer.
    buf: Writer,
    /// The selected atom tag family.
    atom_encoding: AtomEncoding,
}

impl<'a> Encoder<'a> {
    /// Creates an encoder whose output begins with the version byte `131`.
    pub fn new(atoms: &'a AtomTable) -> Encoder<'a> {
        unimplemented!()
    }

    /// Creates an encoder for a body: **no** leading version byte.
    ///
    /// Used for the distribution payload that follows a distribution header and
    /// for ETS term blobs.
    pub fn new_body(atoms: &'a AtomTable) -> Encoder<'a> {
        unimplemented!()
    }

    /// Selects the atom tag family; defaults to [`AtomEncoding::Utf8`].
    pub fn set_atom_encoding(&mut self, encoding: AtomEncoding) {
        unimplemented!()
    }

    /// Encodes `term`, appending it to the output.
    ///
    /// A tag the M1 term layer does not support yields
    /// [`crate::error::unsupported`]; a value beyond a wire bound yields
    /// `BeamError::SystemLimit`. The encoder never silently mis-encodes.
    pub fn encode(&mut self, term: Eterm) -> Result<()> {
        unimplemented!()
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        unimplemented!()
    }

    /// The number of bytes written.
    pub fn len(&self) -> usize {
        unimplemented!()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        unimplemented!()
    }

    /// Consumes the encoder, returning the written bytes.
    pub fn finish(self) -> Vec<u8> {
        unimplemented!()
    }
}

/// Encodes `term` as a `term_to_binary` image: version byte plus term.
///
/// Convenience over [`Encoder::new`] + [`Encoder::encode`] + [`Encoder::finish`].
pub fn encode_term(term: Eterm, atoms: &AtomTable) -> Result<Vec<u8>> {
    unimplemented!()
}

/// Encodes `term` as a body (no version byte): distribution payload / ETS blob.
///
/// Convenience over [`Encoder::new_body`] + [`Encoder::encode`] +
/// [`Encoder::finish`].
pub fn encode_term_body(term: Eterm, atoms: &AtomTable) -> Result<Vec<u8>> {
    unimplemented!()
}
