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

//! The module checksum — the 16-byte MD5 over a fixed subset of the container.
//!
//! Compatibility contract **A.2** (the `.beam` binary surface): the loader computes
//! the same digest the C reader stores in `beam->checksum` (`beam_file.c:1751-1830`)
//! and that the standard library's [`beam_lib:md5/1`] reports. `Module::checksum`
//! exposes it; `beam_codec`'s `NEW_FUN_EXT` local-fun identity carries it as the
//! lambda's `uniq`.
//!
//! The digest is MD5 over the concatenation, **in this order**, of the *raw,
//! unpadded* payloads of the chunks that are present:
//!
//! `Atom` (obsolete Latin-1; only if present) · `AtU8` · `Code` (the full payload,
//! header included) · `StrT` · `ImpT` · `ExpT` · `FunT'` · `LitT` (the full raw
//! payload, its `uncompressed_size` framing included) · `Meta` · `Recs` · `DbgB`.
//!
//! `Line`/`LocT`/`Attr`/`CInf`/`Type`/`Docs`/`Dbgi` are deliberately **excluded**,
//! and an absent (or empty) chunk contributes nothing — the C hashes a
//! zero-initialized entry as zero bytes, which is exactly "skip it".
//!
//! The payloads are **streamed** into one incremental MD5 context, so the digest is
//! that conceptual concatenation while no copy of it is ever materialized: the chunk
//! directory is walked exactly once to resolve each digested id to its payload (in
//! file order, first occurrence wins — see [`module_md5`]), and each payload slice is
//! fed in place. The `FunT'` entry filter below feeds its 20-byte prefix and a 4-byte
//! zero word per entry directly, allocating nothing per entry.
//!
//! `FunT'` is the one special case: the 4-byte entry count is digested as-is, then
//! each 24-byte entry contributes its first 20 bytes (`Function`/`Arity`/`Index`/
//! `NumFree`) followed by 4 zero bytes — the trailing `OldUniq` field is a
//! superseded, endian-dependent hash and must not perturb the digest
//! (`beam_file.c:1777-1780`, `beam_lib.erl:873-883`). A `FunT` that is not
//! `4 + 24n` bytes is the C's `BEAMFILE_READ_CORRUPT_LAMBDA_TABLE`
//! (`beam_file.c:1799-1801`) — a typed [`BeamError::BadArg`].
//!
//! ## The MD5 implementation
//!
//! The digest is [`beam_common::md5::Md5`], the streaming form of the workspace's
//! **single** RFC 1321 implementation ([`beam_common::md5::digest`] is merely its
//! one-shot wrapper), shared by every contract that needs one (A.2 here, A.5 in
//! `beam_dist`, A.7's `erlang:md5/1` family). This module therefore owns only the
//! chunk walk and the `FunT'` filtering described above, never the algorithm, and it
//! keeps no buffer of its own: `Md5::update` folds whole blocks straight out of the
//! caller's slice and copies only the ≤ 64-byte tail block into its own context. That
//! per-block tail copy lives in `beam_common` and is out of scope here.
//!
//! The digest is pinned by the RFC 1321 test vectors below, by the streamed-vs-
//! concatenated equivalence tests, and by the live `beam_lib:md5/1` oracle cross-check
//! in `tests/checksum.rs`.
//!
//! [`beam_lib:md5/1`]: https://www.erlang.org/doc/apps/stdlib/beam_lib.html#md5-1

use beam_common::md5::Md5;
use beam_common::{BeamError, Result};

use crate::container::{ChunkId, RawChunk};

/// Every chunk id the digest covers, in the fixed digest order described in the module
/// docs: the ids before `FunT`, then `FunT` itself (whose payload is filtered through
/// [`digest_funtab`] rather than fed verbatim), then the ids after it.
///
/// The obsolete Latin-1 `Atom` chunk leads the list — it is normally absent, so it
/// normally contributes nothing, but `beam_lib:md5/1` lists it first.
const DIGEST_CHUNKS: [ChunkId; 11] = [
    ChunkId::OBSOLETE_ATOM,
    ChunkId::ATOM,
    ChunkId::CODE,
    ChunkId::STRT,
    ChunkId::IMPT,
    ChunkId::EXPT,
    ChunkId::FUNT,
    ChunkId::LITT,
    ChunkId::META,
    ChunkId::RECS,
    ChunkId::DBGB,
];

/// Computes the module checksum — the 16-byte MD5 [`beam_lib:md5/1`] reports — over the
/// raw, unpadded chunk payloads of `raw`.
///
/// `raw` is a container's chunk directory in file order ([`RawChunk`]); duplicate ids
/// keep the first occurrence, matching [`crate::container::Container::find`]. Pass
/// [`crate::chunk::Chunks::raw`], which `chunk::parse` retains for exactly this purpose.
///
/// The directory is walked **once**: each digested id ([`DIGEST_CHUNKS`]) is resolved to
/// its payload slice, which is then fed to a single incremental
/// [`beam_common::md5::Md5`] context in the fixed order documented above. Nothing is
/// copied — neither the concatenation nor a `FunT` entry table.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a `FunT` chunk that is not `4 + 24n` bytes (the C's
/// `BEAMFILE_READ_CORRUPT_LAMBDA_TABLE`, `beam_file.c:1799-1801`).
///
/// [`beam_lib:md5/1`]: https://www.erlang.org/doc/apps/stdlib/beam_lib.html#md5-1
pub fn module_md5(raw: &[RawChunk<'_>]) -> Result<[u8; 16]> {
    // An absent chunk stays `None` — the same "hashes nothing" the C's zero-initialized
    // directory entry produces. Duplicate ids keep the first (`get_or_insert` never
    // overwrites), matching `Container::find`.
    let mut payloads: [Option<&[u8]>; DIGEST_CHUNKS.len()] = [None; DIGEST_CHUNKS.len()];
    for chunk in raw {
        if let Some(slot) = DIGEST_CHUNKS.iter().position(|id| *id == chunk.id) {
            payloads[slot].get_or_insert(chunk.data);
        }
    }

    let mut md5 = Md5::new();
    for (id, payload) in DIGEST_CHUNKS.iter().zip(payloads) {
        let payload = payload.unwrap_or_default();
        if *id == ChunkId::FUNT {
            digest_funtab(payload, &mut md5)?;
        } else {
            md5.update(payload);
        }
    }
    Ok(md5.finish())
}

/// Feeds a `FunT` chunk's digest input — the 4-byte entry count followed by each
/// 24-byte entry with its trailing `OldUniq` forced to zero — into `md5`.
///
/// An empty `funtab` contributes nothing. A non-empty table whose length is not
/// `4 + 24n` is the C's corrupt-lambda-table error.
///
/// Each entry is fed as two borrowed slices (its 20-byte prefix and a shared 4-byte zero
/// word), so the filter allocates nothing per entry.
fn digest_funtab(funtab: &[u8], md5: &mut Md5) -> Result<()> {
    if funtab.is_empty() {
        return Ok(());
    }
    // The C only enters the lambda walk when at least 4 bytes remain
    // (`beam_file.c:1780`); a shorter non-empty table falls straight through to the
    // "left-over bytes" error below.
    if funtab.len() < 4 {
        return Err(BeamError::BadArg);
    }
    md5.update(&funtab[..4]);

    // The entry's trailing `OldUniq` is forced to zero from this one shared word.
    let zero_uniq = [0u8; 4];
    let mut rest = &funtab[4..];
    while let Some(entry) = rest.get(..24) {
        // `[function:u32][arity:u32][index:u32][num_free:u32]` are digested verbatim…
        md5.update(&entry[..20]);
        // …and its `OldUniq` is digested as zero.
        md5.update(&zero_uniq);
        rest = &rest[24..];
    }
    if !rest.is_empty() {
        return Err(BeamError::BadArg);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The workspace's single RFC 1321 MD5 — imported so the vectors/padding and
    /// chunk-order tests below can pin the exact bytes this module's digest feeds.
    use beam_common::md5::digest;

    use super::*;
    use crate::container::ChunkId;
    /// Renders a digest as lowercase hex (the form the oracle test compares against).
    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    // ---- RFC 1321 test vectors (RFC 1321 §A.5) ---------------------------

    #[test]
    fn rfc1321_vectors() {
        // (input, expected digest) — the seven published RFC 1321 §A.5 vectors.
        let vectors: [(&[u8], &str); 7] = [
            (b"", "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in vectors {
            assert_eq!(
                hex(&digest(input)),
                expected,
                "input {:?}",
                String::from_utf8_lossy(input)
            );
        }
    }

    #[test]
    fn md5_handles_every_padding_boundary() {
        // Inputs of `0..len` as `i as u8` cross every padding boundary a 512-bit block
        // has: 55 (last length that fits one block), 56 (needs a second block), 63 (a
        // full one-block tail), 64/128 (whole blocks) and 119/120 (one block + a
        // 55/56-byte tail). The expected digests are pinned from an independent MD5
        // (`md5sum`), so a padding regression cannot hide — the RFC 1321 vectors above
        // would still pass a padding bug that only shows off a block edge.
        let pinned: [(usize, &str); 10] = [
            (0, "d41d8cd98f00b204e9800998ecf8427e"),
            (1, "93b885adfe0da089cdf634904fd59f71"),
            (55, "6912ee65fff2d9f9ce2508cddf8bcda0"),
            (56, "51fdd1acda72405dfdfa03fcb85896d7"),
            (63, "48a6295221902e8e0938f773a7185e72"),
            (64, "b2d3f56bc197fd985d5965079b5e7148"),
            (119, "1c772251899a7ff007400b888d6b2042"),
            (120, "b7ba1efc6022e9ed272f00b8831e26e6"),
            (127, "8402b21e7bc7906493bae0dac017f1f9"),
            (128, "37eff01866ba3f538421b30b7cbefcac"),
        ];
        for (len, expected) in pinned {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(hex(&digest(&data)), expected, "length {len}");
        }
    }

    // ---- FunT' filtering -------------------------------------------------

    /// Builds a `FunT` chunk payload: a 4-byte entry count followed by `n` 24-byte
    /// entries whose bytes are `entry_index`-derived so the `OldUniq` region is
    /// distinguishable from the digest-relevant first 20 bytes.
    fn funtab(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 24 * n);
        out.extend_from_slice(&(n as u32).to_be_bytes());
        for i in 0..n {
            out.extend_from_slice(&[i as u8; 20]); // Function/Arity/Index/NumFree
            out.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]); // OldUniq (ignored)
        }
        out
    }

    /// A raw chunk directory holding the given `(id, data)` pairs.
    ///
    /// Only `data` is borrowed (the `pairs` slice itself is not), so a caller may pass a
    /// temporary array literal and keep the returned directory.
    fn chunks<'a>(pairs: &[(ChunkId, &'a [u8])]) -> Vec<RawChunk<'a>> {
        pairs
            .iter()
            .map(|(id, data)| RawChunk { id: *id, data })
            .collect()
    }

    #[test]
    fn funtab_olduniq_is_forced_to_zero() {
        // Two `FunT` chunks identical except for their `OldUniq` fields must digest
        // identically — the digest must ignore `OldUniq`.
        let mut a = funtab(2);
        let mut b = funtab(2);
        // Overwrite every `OldUniq` with different bytes in `b`.
        for i in 0..2 {
            let off = 4 + i * 24 + 20;
            b[off..off + 4].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        }
        // …and also perturb `a`'s, in a third way.
        for i in 0..2 {
            let off = 4 + i * 24 + 20;
            a[off..off + 4].copy_from_slice(&[0x55, 0x66, 0x77, 0x88]);
        }
        let base = funtab(2);
        assert_eq!(
            module_md5(&chunks(&[(ChunkId::FUNT, &base)])).unwrap(),
            module_md5(&chunks(&[(ChunkId::FUNT, &a)])).unwrap()
        );
        assert_eq!(
            module_md5(&chunks(&[(ChunkId::FUNT, &base)])).unwrap(),
            module_md5(&chunks(&[(ChunkId::FUNT, &b)])).unwrap()
        );

        // And the digest equals md5 over the zeroed form exactly.
        let mut expected = Vec::new();
        expected.extend_from_slice(&base[..4]);
        for i in 0..2 {
            let off = 4 + i * 24;
            expected.extend_from_slice(&base[off..off + 20]);
            expected.extend_from_slice(&[0u8; 4]);
        }
        assert_eq!(
            digest(&expected),
            module_md5(&chunks(&[(ChunkId::FUNT, &base)])).unwrap()
        );
    }

    #[test]
    fn funtab_contributes_nothing_when_absent_or_empty() {
        let empty: &[u8] = &[];
        let absent = module_md5(&chunks(&[])).unwrap();
        let present_empty = module_md5(&chunks(&[(ChunkId::FUNT, empty)])).unwrap();
        assert_eq!(absent, present_empty);
        // Which is just the MD5 of nothing.
        assert_eq!(absent, digest(b""));
    }

    #[test]
    fn corrupt_funtab_is_rejected() {
        // Length not congruent to 4 modulo 24 (a partial 24-byte entry).
        let mut bad = funtab(1);
        bad.push(0); // 4 + 24 + 1 bytes
        let err = module_md5(&chunks(&[(ChunkId::FUNT, &bad)])).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A non-empty table shorter than the 4-byte count.
        let short = [0u8; 3];
        let err = module_md5(&chunks(&[(ChunkId::FUNT, &short)])).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");

        // A 5-byte table: the count plus a single stray byte.
        let stray = [0u8; 5];
        let err = module_md5(&chunks(&[(ChunkId::FUNT, &stray)])).unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    #[test]
    fn funtab_zero_entries_is_valid_and_digests_its_count() {
        // `4 + 24*0` = 4 bytes: a valid empty lambda table whose count word is
        // digested. It must differ from the "no FunT" digest.
        let zero_entries = 0u32.to_be_bytes();
        let with = module_md5(&chunks(&[(ChunkId::FUNT, &zero_entries)])).unwrap();
        let without = module_md5(&chunks(&[])).unwrap();
        assert_eq!(with, digest(&zero_entries));
        assert_ne!(with, without);
    }

    // ---- the fixed chunk order / set -------------------------------------

    #[test]
    fn only_the_significant_chunks_are_digested() {
        // Every excluded chunk id, when present alone, must give the empty digest.
        let excluded: [ChunkId; 7] = [
            ChunkId::LINE,
            ChunkId::LOCT,
            ChunkId::ATTR,
            ChunkId::CINF,
            ChunkId::TYPE,
            ChunkId::DOCS,
            ChunkId::DBGI,
        ];
        for id in excluded {
            let data = [1u8, 2, 3, 4, 5];
            assert_eq!(
                module_md5(&chunks(&[(id, &data)])).unwrap(),
                digest(b""),
                "chunk {:?} must not be digested",
                id
            );
        }
    }

    #[test]
    fn chunk_order_is_fixed_and_concatenated() {
        // The digest is MD5 over the payloads concatenated in the *fixed* order, not
        // file order: two directories with the same chunks in different file orders
        // must digest identically.
        let atu8 = [0xAAu8; 7];
        let code = [0xBBu8; 5];
        let forward = chunks(&[(ChunkId::ATOM, &atu8), (ChunkId::CODE, &code)]);
        let reversed = chunks(&[(ChunkId::CODE, &code), (ChunkId::ATOM, &atu8)]);
        assert_eq!(
            module_md5(&forward).unwrap(),
            module_md5(&reversed).unwrap()
        );

        let mut expected = Vec::new();
        expected.extend_from_slice(&atu8);
        expected.extend_from_slice(&code);
        assert_eq!(digest(&expected), module_md5(&forward).unwrap());
    }

    #[test]
    fn the_obsolete_atom_chunk_is_digested_only_when_present() {
        // The obsolete Latin-1 `Atom` chunk leads the digest order (beam_lib lists it
        // first); when absent it contributes nothing.
        let obsolete = [0x01u8, 0x02, 0x03];
        let without = module_md5(&chunks(&[(ChunkId::ATOM, &[0xAA])])).unwrap();
        let with = module_md5(&chunks(&[
            (ChunkId::OBSOLETE_ATOM, &obsolete),
            (ChunkId::ATOM, &[0xAA]),
        ]))
        .unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&obsolete);
        expected.extend_from_slice(&[0xAA]);
        assert_eq!(with, digest(&expected));
        assert_eq!(without, digest(&[0xAA]));
        assert_ne!(with, without);
    }

    #[test]
    fn duplicate_ids_keep_the_first() {
        // Matching `Container::find`: the first occurrence wins.
        let first = [0x11u8; 4];
        let second = [0x22u8; 4];
        let dup = chunks(&[(ChunkId::ATOM, &first), (ChunkId::ATOM, &second)]);
        assert_eq!(module_md5(&dup).unwrap(), digest(&first));
    }

    #[test]
    fn streamed_digest_equals_an_independently_built_concatenation() {
        // The digest is streamed straight out of the container — no whole-module buffer
        // is assembled — so pin the stream against a concatenation built here by hand in
        // the fixed order. The directory mixes present, absent, present-but-empty,
        // duplicated and excluded ids, and carries a non-empty `FunT` whose `OldUniq`
        // words must be digested as zero.
        const EMPTY: &[u8] = &[];
        let obsolete = [0x01u8, 0x02, 0x03];
        let atu8_first = [0xA1u8; 6];
        let atu8_second = [0xA2u8; 6]; // duplicate `AtU8`: never digested
        let code = [0xC0u8; 33];
        let expt = [0xE1u8; 2];
        let table = funtab(3);
        let litt = [0x7Au8; 9];
        let recs = [0x5Eu8; 4];
        let line = [0xFFu8; 8]; // excluded from the digest
        let raw = chunks(&[
            (ChunkId::DBGB, EMPTY),
            (ChunkId::OBSOLETE_ATOM, &obsolete),
            (ChunkId::ATOM, &atu8_first),
            (ChunkId::ATOM, &atu8_second),
            (ChunkId::LINE, &line),
            (ChunkId::CODE, &code),
            (ChunkId::EXPT, &expt),
            (ChunkId::FUNT, &table),
            (ChunkId::LITT, &litt),
            (ChunkId::RECS, &recs),
        ]);

        // `Atom`(obsolete) · `AtU8` · `Code` · `StrT`(absent) · `ImpT`(absent) · `ExpT`
        // · `FunT'` · `LitT` · `Meta`(absent) · `Recs` · `DbgB`(empty).
        let mut expected = Vec::new();
        expected.extend_from_slice(&obsolete);
        expected.extend_from_slice(&atu8_first);
        expected.extend_from_slice(&code);
        expected.extend_from_slice(&expt);
        expected.extend_from_slice(&table[..4]);
        for entry in table[4..].chunks_exact(24) {
            expected.extend_from_slice(&entry[..20]);
            expected.extend_from_slice(&[0u8; 4]);
        }
        expected.extend_from_slice(&litt);
        expected.extend_from_slice(&recs);

        assert_eq!(module_md5(&raw).unwrap(), digest(&expected));
        // The ignored duplicate, the excluded `Line` payload and the entries' real
        // `OldUniq` words must all stay out of the digest.
        let mut with_extras = expected.clone();
        with_extras.extend_from_slice(&atu8_second);
        with_extras.extend_from_slice(&line);
        with_extras.extend_from_slice(&table[4..]);
        assert_ne!(module_md5(&raw).unwrap(), digest(&with_extras));
    }

    #[test]
    fn a_real_fixture_digests_deterministically() {
        // The committed fixture parses and digests to a stable value (pinned by the
        // oracle test in `tests/checksum.rs`); here we only assert determinism and
        // that the digest is not the empty-input value.
        const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/slice.beam");
        let container = crate::container::parse(FIXTURE).unwrap();
        let a = module_md5(&container.chunks).unwrap();
        let b = module_md5(&container.chunks).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, digest(b""));
    }
}
