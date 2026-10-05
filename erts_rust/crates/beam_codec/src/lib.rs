#![doc = "External Term Format (ETF) encode/decode for the Rust ERTS rewrite."]

//! External Term Format codec.
//!
//! Implements compatibility contract Part I §A.5 (the distribution/ETF wire format,
//! `VERSION_MAGIC 131`, all tags including compressed terms) and the Stage-0
//! foundations of Part II §H. Encoding/decoding is allocation-aware: decoders write
//! into a caller-supplied heap so the same code path serves distribution, ETS and
//! `term_to_binary`.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_codec() -> &'static str {
    "beam_codec"
}
