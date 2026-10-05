//! The `AtU8` atom chunk: UTF-8 atom names in wire-index order.
//!
//! Layout: `i32 count` (negative ⇒ "long" varint lengths, real count = `-count`);
//! then per atom a length (varint if long, else `u8`) followed by the UTF-8 bytes.
//! Names are **UTF-8, not Latin-1** (`beam_file.c:291`). Wire **index 0 is reserved
//! for NIL**: index 1 is the module name and real atoms are `1..=count`
//! (`beam_file.c:262-297`), so the total number of entries is `count + 1`.
use beam_common::Result;

/// The decoded atom names, in wire-index order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Atoms {
    /// The names; `names[0]` is the reserved NIL placeholder (the empty string).
    pub names: Vec<String>,
}

/// Parses an `AtU8` chunk payload into its names, in wire-index order.
///
/// The returned vector is prefixed with the reserved NIL placeholder, so
/// `names[wire_index]` is the atom for that index and `names.len() == count + 1`.
// TODO(beam_loader-impl): read the signed `count` (negative ⇒ long varint lengths),
// TODO(beam_loader-impl): prepend the NIL placeholder, then read `count` length+bytes
// TODO(beam_loader-impl): pairs as UTF-8; reject a truncated chunk or a non-UTF-8 name
// TODO(beam_loader-impl): with `BeamError::BadArg`.
pub fn parse(data: &[u8]) -> Result<Vec<String>> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}
