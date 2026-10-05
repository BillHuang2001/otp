//! The conventional glob-import surface for downstream crates.
//!
//! `use beam_codec::prelude::*;` brings the codec seam (the reader/writer, the
//! atom cache, the encoder/decoder and the tag constants) into scope. The
//! term-layer helpers in [`crate::term`] are deliberately **not** re-exported:
//! they are an internal stop-gap, not part of the cross-crate contract.

pub use crate::atom_cache::AtomCache;
pub use crate::compress::{compress, decompress};
pub use crate::decode::{decode_term, Decoder};
pub use crate::encode::{
    encode_term, encode_term_body, encode_term_body_compressed, encode_term_compressed,
    AtomEncoding, Encoder,
};
pub use crate::error::{is_unsupported, unsupported, UNSUPPORTED_REASON};
pub use crate::reader::Reader;
pub use crate::tag::*;
pub use crate::writer::Writer;
