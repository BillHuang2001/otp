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

use crate::error::bad_file;

/// Parses a `LitT` chunk payload into its literal payloads (raw ETF bytes).
///
/// The returned vector has one entry per literal, in `Operand::Literal(idx)` index
/// order. A compressed chunk or a truncated entry yields `BeamError::BadArg`.
pub fn parse(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut reader = beam_codec::Reader::new(data);

    // A non-zero `uncompressed_size` means the payload is zlib/deflate compressed,
    // which M1 does not support (there is no zlib dependency). Refuse it explicitly
    // rather than mis-parsing compressed bytes as literal records.
    let uncompressed_size = reader.u32be()?;
    if uncompressed_size != 0 {
        return Err(bad_file("compressed LitT chunk is unsupported"));
    }

    let raw_count = reader.u32be()? as i32;
    if raw_count < 0 {
        return Err(bad_file("negative literal count"));
    }
    let count = raw_count as usize;

    let mut literals = Vec::new();
    for _ in 0..count {
        // Each record is `{ i32 ext_size, ext_size bytes }` of ETF term bytes.
        let ext_size = reader.u32be()? as i32;
        if ext_size < 0 {
            return Err(bad_file("negative literal size"));
        }
        let bytes = reader.bytes(ext_size as usize)?;
        literals.push(bytes.to_vec());
    }

    Ok(literals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// Builds a `LitT` chunk from `uncompressed_size` and `(ext_size, payload)`
    /// records.
    fn lit_bytes(uncompressed_size: u32, records: &[(u32, &[u8])]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&uncompressed_size.to_be_bytes());
        data.extend_from_slice(&(records.len() as u32).to_be_bytes());
        for (size, payload) in records {
            data.extend_from_slice(&size.to_be_bytes());
            data.extend_from_slice(payload);
        }
        data
    }

    #[test]
    fn parses_literal_records() {
        let data = lit_bytes(0, &[(3, &[1, 2, 3]), (2, &[9, 8])]);
        let literals = parse(&data).unwrap();
        assert_eq!(literals, vec![vec![1, 2, 3], vec![9, 8]]);
    }

    #[test]
    fn zero_literals_is_allowed() {
        assert!(parse(&lit_bytes(0, &[])).unwrap().is_empty());
    }

    #[test]
    fn compressed_chunk_is_rejected() {
        let err = parse(&lit_bytes(42, &[])).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn negative_count_is_rejected() {
        let mut data = 0u32.to_be_bytes().to_vec();
        data.extend_from_slice(&(-1i32).to_be_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn negative_literal_size_is_rejected() {
        let mut data = 0u32.to_be_bytes().to_vec();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&(-1i32).to_be_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_literal_is_rejected() {
        // Claims a 4-byte literal but supplies only two bytes.
        let mut data = 0u32.to_be_bytes().to_vec();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&4i32.to_be_bytes());
        data.extend_from_slice(&[1, 2]);
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn missing_uncompressed_size_is_rejected() {
        let err = parse(&[0, 0]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
