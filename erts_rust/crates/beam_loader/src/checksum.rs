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
//! `FunT'` is the one special case: the 4-byte entry count is digested as-is, then
//! each 24-byte entry contributes its first 20 bytes (`Function`/`Arity`/`Index`/
//! `NumFree`) followed by 4 zero bytes — the trailing `OldUniq` field is a
//! superseded, endian-dependent hash and must not perturb the digest
//! (`beam_file.c:1777-1780`, `beam_lib.erl:873-883`). A `FunT` that is not
//! `4 + 24n` bytes is the C's `BEAMFILE_READ_CORRUPT_LAMBDA_TABLE`
//! (`beam_file.c:1799-1801`) — a typed [`BeamError::BadArg`].
//!
//! ## Why an in-tree MD5
//!
//! Following the crate-layer layering contract, `beam_loader` must not gain a
//! `beam_dist` dependency edge (a same-wave edge) and no new external crate is
//! allowed, so this module carries a self-contained RFC 1321 MD5. It is byte-for-byte
//! the algorithm [`crate::checksum`]'s sibling implementations use (`beam_dist`'s
//! `auth::md5`, `beam_rt`'s `module_md5`) and is pinned by the RFC 1321 test vectors
//! below plus an oracle cross-check in `tests/checksum.rs`.
//!
//! **Structural debt:** an MD5 is needed by `beam_dist` (distribution auth) and
//! `beam_rt` (the fun-identity derivation) as well, so this implementation belongs at
//! a lowest-common-ancestor crate (`beam_common`) rather than duplicated per crate;
//! de-duplicating it is a repo-root-owned follow-up (never copy-paste a third copy).
//!
//! [`beam_lib:md5/1`]: https://www.erlang.org/doc/apps/stdlib/beam_lib.html#md5-1

use beam_common::{BeamError, Result};

use crate::container::{ChunkId, RawChunk};

/// The MD5 per-round left-rotation amounts (RFC 1321 §3.4), four 16-step rounds.
const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The MD5 additive constants `K[i] = floor(2^32 * abs(sin(i + 1)))` (RFC 1321 §3.4).
const MD5_K: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, //
    0xf57c_0faf, 0x4787_c62a, 0xa830_4613, 0xfd46_9501, //
    0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, //
    0x6b90_1122, 0xfd98_7193, 0xa679_438e, 0x49b4_0821, //
    0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, //
    0xd62f_105d, 0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, //
    0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed, //
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, //
    0xfffa_3942, 0x8771_f681, 0x6d9d_6122, 0xfde5_380c, //
    0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, //
    0x289b_7ec6, 0xeaa1_27fa, 0xd4ef_3085, 0x0488_1d05, //
    0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, //
    0xf429_2244, 0x432a_ff97, 0xab94_23a7, 0xfc93_a039, //
    0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1, //
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, //
    0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb, 0xeb86_d391,
];

/// The MD5 initial state words (RFC 1321 §3.3).
const MD5_INIT: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// Runs the 64-step MD5 compression function over one 512-bit block.
///
/// The block length is not re-checked: every caller passes exactly one `[u8; 64]`
/// chunk, so an out-of-range index would be a logic bug (never reachable from input
/// data), not an input error.
fn md5_compress(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut m = [0u32; 16];
    for (i, word) in m.iter_mut().enumerate() {
        let o = i * 4;
        *word = u32::from_le_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];

    for i in 0..64 {
        let (f, g) = match i {
            0..=15 => ((b & c) | (!b & d), i),
            16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(MD5_K[i])
            .wrapping_add(m[g])
            .rotate_left(MD5_SHIFTS[i]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(rotated);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// Computes the MD5 message digest (RFC 1321) of `data`.
///
/// Fully safe Rust; no allocation, no `unsafe`, and it never panics — any byte
/// string (including the empty one) is valid input.
///
/// ```
/// use beam_loader::checksum::md5;
/// assert_eq!(
///     md5(b"abc"),
///     [
///         0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0,
///         0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1, 0x7f, 0x72,
///     ]
/// );
/// ```
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut state = MD5_INIT;

    // Full 512-bit blocks straight from the input.
    let (blocks, remainder) = data.as_chunks::<64>();
    for block in blocks {
        md5_compress(&mut state, block);
    }

    // RFC 1321 §3.1/§3.2 padding: a single `1` bit, zeros up to the 56-byte
    // boundary, then the 64-bit little-endian *bit* length. A tail of 56..=63 bytes
    // needs a second block, hence the 128-byte scratch buffer.
    let rem = remainder;
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let tail_len = if rem.len() <= 55 { 64 } else { 128 };
    let bit_len = (data.len() as u64).wrapping_mul(8);
    tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_le_bytes());

    for block in tail[..tail_len].as_chunks::<64>().0 {
        md5_compress(&mut state, block);
    }

    let mut digest = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
}

/// The chunk ids digested in the fixed order described in the module docs, *excluding*
/// the specially-handled `FunT` (which is filtered through [`digest_funtab`]). The
/// obsolete Latin-1 `Atom` chunk leads the list — it is normally absent, so it normally
/// contributes nothing, but `beam_lib:md5/1` lists it first.
const DIGEST_CHUNKS_BEFORE_FUNT: [ChunkId; 6] = [
    ChunkId::OBSOLETE_ATOM,
    ChunkId::ATOM,
    ChunkId::CODE,
    ChunkId::STRT,
    ChunkId::IMPT,
    ChunkId::EXPT,
];

/// The chunk ids digested after `FunT`, in order.
const DIGEST_CHUNKS_AFTER_FUNT: [ChunkId; 4] =
    [ChunkId::LITT, ChunkId::META, ChunkId::RECS, ChunkId::DBGB];

/// Computes the module checksum — the 16-byte MD5 [`beam_lib:md5/1`] reports — over the
/// raw, unpadded chunk payloads of `raw`.
///
/// `raw` is a container's chunk directory in file order ([`RawChunk`]); duplicate ids
/// keep the first occurrence, matching [`crate::container::Container::find`]. Pass
/// [`crate::chunk::Chunks::raw`], which `chunk::parse` retains for exactly this purpose.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a `FunT` chunk that is not `4 + 24n` bytes (the C's
/// `BEAMFILE_READ_CORRUPT_LAMBDA_TABLE`, `beam_file.c:1799-1801`).
///
/// [`beam_lib:md5/1`]: https://www.erlang.org/doc/apps/stdlib/beam_lib.html#md5-1
pub fn module_md5(raw: &[RawChunk<'_>]) -> Result<[u8; 16]> {
    // An absent chunk is the empty slice — the same "hashes nothing" the C's
    // zero-initialized directory entry produces. Duplicate ids keep the first.
    let chunk = |id: ChunkId| -> &[u8] {
        raw.iter()
            .find(|chunk| chunk.id == id)
            .map(|chunk| chunk.data)
            .unwrap_or_default()
    };

    let mut input = Vec::new();
    for id in DIGEST_CHUNKS_BEFORE_FUNT {
        input.extend_from_slice(chunk(id));
    }
    digest_funtab(chunk(ChunkId::FUNT), &mut input)?;
    for id in DIGEST_CHUNKS_AFTER_FUNT {
        input.extend_from_slice(chunk(id));
    }
    Ok(md5(&input))
}

/// Appends a `FunT` chunk's digest input — the 4-byte entry count followed by each
/// 24-byte entry with its trailing `OldUniq` forced to zero — to `input`.
///
/// An empty `funtab` contributes nothing. A non-empty table whose length is not
/// `4 + 24n` is the C's corrupt-lambda-table error.
fn digest_funtab(funtab: &[u8], input: &mut Vec<u8>) -> Result<()> {
    if funtab.is_empty() {
        return Ok(());
    }
    // The C only enters the lambda walk when at least 4 bytes remain
    // (`beam_file.c:1780`); a shorter non-empty table falls straight through to the
    // "left-over bytes" error below.
    if funtab.len() < 4 {
        return Err(BeamError::BadArg);
    }
    input.extend_from_slice(&funtab[..4]);

    let mut rest = &funtab[4..];
    while let Some(entry) = rest.get(..24) {
        // `[function:u32][arity:u32][index:u32][num_free:u32]` are digested verbatim…
        input.extend_from_slice(&entry[..20]);
        // …and the entry's trailing `OldUniq` is forced to zero.
        input.extend_from_slice(&[0u8; 4]);
        rest = &rest[24..];
    }
    if !rest.is_empty() {
        return Err(BeamError::BadArg);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
            (b"abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
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
            assert_eq!(hex(&md5(input)), expected, "input {:?}", String::from_utf8_lossy(input));
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
            assert_eq!(hex(&md5(&data)), expected, "length {len}");
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
        assert_eq!(md5(&expected), module_md5(&chunks(&[(ChunkId::FUNT, &base)])).unwrap());
    }

    #[test]
    fn funtab_contributes_nothing_when_absent_or_empty() {
        let empty: &[u8] = &[];
        let absent = module_md5(&chunks(&[])).unwrap();
        let present_empty = module_md5(&chunks(&[(ChunkId::FUNT, empty)])).unwrap();
        assert_eq!(absent, present_empty);
        // Which is just the MD5 of nothing.
        assert_eq!(absent, md5(b""));
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
        assert_eq!(with, md5(&zero_entries));
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
                md5(b""),
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
        assert_eq!(module_md5(&forward).unwrap(), module_md5(&reversed).unwrap());

        let mut expected = Vec::new();
        expected.extend_from_slice(&atu8);
        expected.extend_from_slice(&code);
        assert_eq!(md5(&expected), module_md5(&forward).unwrap());
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
        assert_eq!(with, md5(&expected));
        assert_eq!(without, md5(&[0xAA]));
        assert_ne!(with, without);
    }

    #[test]
    fn duplicate_ids_keep_the_first() {
        // Matching `Container::find`: the first occurrence wins.
        let first = [0x11u8; 4];
        let second = [0x22u8; 4];
        let dup = chunks(&[(ChunkId::ATOM, &first), (ChunkId::ATOM, &second)]);
        assert_eq!(module_md5(&dup).unwrap(), md5(&first));
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
        assert_ne!(a, md5(b""));
    }
}
