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
//! The decompression direction — zlib's `inflate.c`/`inftrees.c`/`inffast.c`.
//!
//! [`Inflate`] is `inflateInit2`'s stream: [`Inflate::inflate`] is one `inflate()` pump
//! step and [`decompress_all`] is the bounded one-shot loop the ETF `COMPRESSED` and
//! `zlib:uncompress/1` paths use.
//!
//! not-implemented: the port lands here.

use beam_common::{BeamError, Result};

use crate::{Flush, Pump};

/// An `inflate` stream (`z_stream` in `inflate.c`'s `HEAD`/`TYPE`/`LEN`/… state machine).
pub struct Inflate {
    window_bits: i8,
}

impl Inflate {
    /// `inflateInit2_` — `window_bits` is `8..=15` (zlib), `-15..=-8` (raw RFC 1951),
    /// `16 + 8 ..= 16 + 15` (gzip) or `32 + 8 ..= 32 + 15` (auto-detect zlib/gzip).
    pub fn new(window_bits: i8) -> Result<Inflate> {
        Err(BeamError::BadArg).map(|()| Inflate { window_bits })
    }

    /// One `inflate()` pump step.
    pub fn inflate(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: Flush,
    ) -> Result<Pump> {
        let _ = (input, output, flush);
        Err(BeamError::BadArg)
    }

    /// `inflateReset` — reuse the stream, keeping the window bits.
    pub fn reset(&mut self) -> Result<()> {
        Err(BeamError::BadArg)
    }

    /// `inflateReset2` — reuse the stream with new window bits.
    pub fn reset_with_window_bits(&mut self, window_bits: i8) -> Result<()> {
        let _ = window_bits;
        Err(BeamError::BadArg)
    }

    /// `inflateSetDictionary` — install the preset dictionary `NeedDict` asked for.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> Result<()> {
        let _ = dictionary;
        Err(BeamError::BadArg)
    }

    /// `inflateGetDictionary` — the bytes a following stream would need.
    pub fn get_dictionary(&self) -> &[u8] {
        &[]
    }

    /// Whether `Z_STREAM_END` has been reported (the C's `state->mode == CHECK`/`DONE`).
    pub fn at_stream_end(&self) -> bool {
        false
    }

    /// The window bits this stream was created with.
    pub fn window_bits(&self) -> i8 {
        self.window_bits
    }
}

/// Inflate `input` to the end of the stream, refusing to write more than `limit` bytes;
/// returns the body and the number of **input** bytes the stream consumed.
///
/// This is the bounded one-shot loop: `inflateInit2(window_bits)`, repeated
/// `inflate(…, Flush::None)` into a growing buffer until `StreamEnd`, then `inflateEnd`.
pub fn decompress_all(window_bits: i8, input: &[u8], limit: usize) -> Result<(Vec<u8>, usize)> {
    let _ = (window_bits, input, limit);
    Err(BeamError::BadArg)
}
