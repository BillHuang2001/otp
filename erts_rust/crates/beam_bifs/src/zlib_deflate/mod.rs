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
