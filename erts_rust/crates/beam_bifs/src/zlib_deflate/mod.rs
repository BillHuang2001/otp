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

//! zlib's **level-6 `deflate`**, ported from the C this repository already ships
//! (`../../erts/emulator/zlib/`, zlib 1.3.2), so that `zlib:compress/1` — and with it
//! `zip/1`/`gzip/1`, whose payload is the same deflate stream — emits the oracle's exact
//! bytes.
//!
//! # Why a port instead of a backend switch
//!
//! `beam_codec` (the ETF `COMPRESSED` path) uses `flate2`'s `rust_backend`, i.e.
//! miniz_oxide, and that is the right encoder for *its* job: the ETF codec only has to
//! produce a decodable stream. A `zlib` BIF has a stronger contract — `zlib:compress/1`
//! is a documented wrapper around `deflateInit(Z, default)` + `deflate(Z, Data, finish)`,
//! so its byte string is observable and must equal the oracle's. miniz_oxide differs from
//! zlib on exactly that: the two agree on short bodies (one static block, literals only)
//! and diverge as soon as a match is emitted or a block has to be split, because the
//! match-search heuristics and the block-split threshold are not the same algorithm.
//! `flate2`'s `zlib-rs` backend is zlib's *interface*, not its output — it picked
//! `deflate_medium` for levels 3–6 where zlib uses `deflate_slow`, so it diverges too.
//! A faithful port is the only route to byte identity that does not need a C toolchain.
//!
//! # Scope
//!
//! Level 6 only, which is the whole of what the six `zlib` convenience functions request
//! (`deflateInit(Z, Z_DEFAULT_COMPRESSION)` is level 6, `MAX_WBITS` 15,
//! `Z_DEFAULT_STRATEGY`); any other level keeps using `beam_codec`'s encoder and is
//! documented as not byte-exact. The two framings (`zip/1`'s raw RFC 1951 body and
//! `gzip/1`'s member) are derived from the one zlib stream by `crate::zlib`, because the
//! deflate body itself does not depend on the `windowBits` sign — only the framing does.
//!
//! # Layout
//!
//! * [`deflate`] — `longest_match`, `deflate_slow`, the header/trailer driver.
//! * [`state`] — the sliding window, hash chains, symbol buffer and bit sink
//!   (`deflate_state`).
//! * [`trees`] — Huffman construction and the three-way block emitter (`trees.c`).
//! * [`tables`] — the constant tables from `trees.h`.

mod deflate;
mod state;
mod tables;
mod trees;

use beam_common::Result;

/// zlib's `Z_DEFAULT_COMPRESSION` (`deflate.c:configuration_table[6]`): the level
/// `deflateInit(Z, default)` selects, and the one this port implements.
const DEFAULT_LEVEL: u8 = 6;

/// The zlib-wrapped deflate stream for `input` at `level` — what `zlib:compress/1`
/// returns.
///
/// Level [`DEFAULT_LEVEL`] (6) is byte-identical to the oracle's `zlib:compress/1`; any
/// other level is delegated to `beam_codec`'s encoder, which is a correct but
/// not-byte-identical compressor. `zlib:compress/1` itself never asks for another level,
/// so the delegation is a documented boundary rather than a supported alternative.
pub(crate) fn compress(input: &[u8], level: u8) -> Result<Vec<u8>> {
    if level == DEFAULT_LEVEL {
        return Ok(deflate::compress(input));
    }
    beam_codec::compress::compress(input, level)
}

#[cfg(test)]
mod corpus {
    //! The manual byte-identity sweep over the on-disk corpus.
    //!
    //! `tests/*.rs` cannot reach [`super::compress`] (it is `pub(crate)`), so the sweep
    //! lives here behind `#[ignore]`. Both directions are driven through the **public**
    //! `BifRegistry` (`zlib:compress/1`, `zlib:zip/1`, `zlib:gzip/1` and their inverses),
    //! so what is measured is the shipped path, not an in-crate shortcut.
    //!
    //! * `corpus_sweep` reads `ZC_CORPUS` (`*.bin`) and writes `<name>.cand` (the
    //!   `zlib:compress/1` image), `<name>.rzip` (`zlib:zip/1`) and `<name>.rgzip`
    //!   (`zlib:gzip/1`) into `ZC_OUT`.
    //! * `corpus_roundtrip` reads the oracle's own images from `ZC_ORACLE_OUT`
    //!   (`<name>.oracle`/`.zip`/`.gzip`) plus the original `<name>.bin` from
    //!   `ZC_ORACLE_IN`, and checks that `zlib:uncompress/1`/`unzip/1`/`gunzip/1` recover
    //!   every one of them byte-for-byte.
    //!
    //! ```text
    //! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
    //!     -p beam_bifs --lib 'zlib_deflate::corpus' -- --ignored --nocapture
    //! ```
    //!
    //! Compare the encode side against the oracle with a shell `cmp` loop. Each test
    //! skips with a printed diagnostic when its environment is unset, so a plain
    //! `cargo test` is unaffected.

    use std::path::{Path, PathBuf};

    use beam_alloc::Heap;
    use beam_codec::term::{bitstring_bytes, make_binary};
    use beam_process::ProcessId;
    use beam_term::{AtomTable, Eterm};

    use crate::{default_registry, BifContext, BifRegistry};

    /// `zlib:compress/1`'s level (`deflateInit(Z, Z_DEFAULT_COMPRESSION)` = 6). The
    /// sweeps here go through the registry, which always uses this level.
    struct Fixture {
        registry: BifRegistry,
        heap: Heap,
        atoms: AtomTable,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                registry: default_registry(),
                // The largest corpus input is ~4 MiB; the heap holds one input plus one
                // output binary at a time.
                heap: Heap::new(1 << 25).expect("test heap"),
                atoms: AtomTable::new(),
            }
        }

        /// `zlib:F(Data)` for a byte slice, returning the result's bytes.
        fn call_bytes(&mut self, function: &str, data: &[u8]) -> Vec<u8> {
            let arg = make_binary(&mut self.heap, data).expect("the binary fits");
            let out = self.call_term(function, arg);
            let (bytes, offset, bits) = bitstring_bytes(out).expect("a binary result");
            assert_eq!(offset, 0, "zlib:{function} returns a whole-byte binary");
            assert_eq!(bits % 8, 0, "zlib:{function} returns a whole-byte binary");
            bytes[..bits / 8].to_vec()
        }

        fn call_term(&mut self, function: &str, arg: Eterm) -> Eterm {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            self.registry
                .call("zlib", function, 1, &mut ctx, &[arg])
                .unwrap_or_else(|e| panic!("zlib:{function}/1 failed: {e:?}"))
        }
    }

    fn inputs(dir: &Path) -> Vec<PathBuf> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "bin"))
            .collect();
        names.sort();
        names
    }

    /// The encode direction: the port's three framings for every corpus input.
    #[test]
    #[ignore = "manual corpus sweep; set ZC_CORPUS and ZC_OUT"]
    fn corpus_sweep() {
        let Ok(corpus) = std::env::var("ZC_CORPUS") else {
            println!("SKIP corpus_sweep: ZC_CORPUS is not set");
            return;
        };
        let out_dir = PathBuf::from(std::env::var("ZC_OUT").expect("ZC_OUT with ZC_CORPUS"));
        std::fs::create_dir_all(&out_dir).expect("create ZC_OUT");

        let (mut total, mut bytes) = (0usize, 0usize);
        let mut fixture = Fixture::new();
        for path in inputs(Path::new(&corpus)) {
            let input = std::fs::read(&path).expect("read input");
            let stem = path.file_stem().expect("file stem");
            for (function, ext) in [
                ("compress", "cand"),
                ("zip", "rzip"),
                ("gzip", "rgzip"),
            ] {
                let image = fixture.call_bytes(function, &input);
                bytes += image.len();
                let mut dest = out_dir.join(stem);
                dest.set_extension(ext);
                std::fs::write(dest, image).expect("write candidate");
            }
            total += 1;
        }
        println!(
            "corpus_sweep: {total} inputs ({bytes} bytes of candidates) -> {}",
            out_dir.display()
        );
    }

    /// The decode direction: every oracle image must inflate back to its input.
    #[test]
    #[ignore = "manual corpus sweep; set ZC_ORACLE_IN and ZC_ORACLE_OUT"]
    fn corpus_roundtrip() {
        let (Ok(originals), Ok(images)) =
            (std::env::var("ZC_ORACLE_IN"), std::env::var("ZC_ORACLE_OUT"))
        else {
            println!("SKIP corpus_roundtrip: ZC_ORACLE_IN / ZC_ORACLE_OUT are not set");
            return;
        };

        let mut fixture = Fixture::new();
        let mut total = 0usize;
        let mut failures = Vec::new();
        for path in inputs(Path::new(&originals)) {
            let input = std::fs::read(&path).expect("read input");
            let stem = path.file_stem().expect("file stem");
            for (function, ext) in [
                ("uncompress", "oracle"),
                ("unzip", "zip"),
                ("gunzip", "gzip"),
            ] {
                let mut image = PathBuf::from(&images).join(stem);
                image.set_extension(ext);
                let bytes = std::fs::read(&image).expect("read oracle image");
                let out = fixture.call_bytes(function, &bytes);
                if out != input {
                    failures.push(format!(
                        "zlib:{function}/1 of {} ({} bytes) gave {} bytes, not the original {}",
                        image.display(),
                        bytes.len(),
                        out.len(),
                        input.len()
                    ));
                }
                total += 1;
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {total} round trips failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
        println!("corpus_roundtrip: {total} oracle images inflated back to their inputs");
    }
}
