//! The `LitT` chunk: the static-literal table.
//!
//! Layout: `u32 uncompressed_size` (`0` ⇒ the payload is stored verbatim;
//! non-zero ⇒ zlib/deflate), then `i32 count` × `{ i32 ext_size, ext_size bytes }`
//! where each payload is Erlang external-term bytes (an ETF-encoded literal).
//!
//! **M1 scope:** the framing is parsed and the raw ETF bytes are retained; the
//! literals are **not** decoded into terms, and the **compressed** form
//! (`uncompressed_size != 0`) is unsupported (there is no zlib dependency) and
//! yields a clear `BeamError::BadArg` (see the crate `CONTEXT.md`, "Known Issues").
use beam_common::Result;

/// Parses a `LitT` chunk payload into its literal payloads (raw ETF bytes).
///
/// The returned vector has one entry per literal, in `Operand::Literal(idx)` index
/// order. A compressed chunk or a truncated entry yields `BeamError::BadArg`.
// TODO(beam_loader-impl): read `uncompressed_size` (rejecting the compressed form
// TODO(beam_loader-impl): with `BeamError::BadArg`), then read the literal count and
// TODO(beam_loader-impl): each `{ext_size, bytes}` pair.
pub fn parse(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}
