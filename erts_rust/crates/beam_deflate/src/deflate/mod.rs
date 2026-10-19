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
//! The compression direction — zlib's `deflate.c`/`deflate.h`/`trees.c`.
//!
//! [`DeflateConfig`] is `deflateInit2`'s argument tuple and [`Deflate`] is the stream it
//! returns: [`Deflate::deflate`] is one `deflate()` pump step and
//! [`Deflate::compress_with`] is the one-shot `Z_FINISH` form.
//!
//! not-implemented: the port lands here.

use beam_common::{BeamError, Result};

use crate::{Flush, Method, Pump, Strategy, DEFAULT_COMPRESSION, DEFAULT_MEM_LEVEL, MAX_WBITS};

/// `deflateInit2`'s argument tuple.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeflateConfig {
    /// `level` — [`DEFAULT_COMPRESSION`] (-1, zlib resolves it to 6) or `0..=9`.
    pub level: i8,
    /// `method` — `Z_DEFLATED`.
    pub method: Method,
    /// `window_bits` — `8..=15` (zlib framing), `-15..=-8` (raw RFC 1951) or
    /// `16 + 8 ..= 16 + 15` (gzip).
    pub window_bits: i8,
    /// `mem_level` — `1..=9`; 8 is `DEF_MEM_LEVEL`.
    pub mem_level: u8,
    /// `strategy`.
    pub strategy: Strategy,
}

impl Default for DeflateConfig {
    fn default() -> Self {
        DeflateConfig {
            level: DEFAULT_COMPRESSION,
            method: Method::Deflated,
            window_bits: MAX_WBITS,
            mem_level: DEFAULT_MEM_LEVEL,
            strategy: Strategy::Default,
        }
    }
}

impl DeflateConfig {
    /// `deflateInit2`'s full tuple.
    pub fn new(
        level: i8,
        method: Method,
        window_bits: i8,
        mem_level: u8,
        strategy: Strategy,
    ) -> Self {
        DeflateConfig {
            level,
            method,
            window_bits,
            mem_level,
            strategy,
        }
    }

    /// The zlib-wrapped shape (`windowBits` 15, `memLevel` 8, default strategy) —
    /// `deflateInit(Z, Level)`, i.e. what `zlib:compress/1` and ETF `COMPRESSED` use.
    pub fn zlib(level: i8) -> Self {
        DeflateConfig {
            level,
            ..DeflateConfig::default()
        }
    }

    /// The raw RFC 1951 shape (`windowBits` -15) — `zlib:zip/1`.
    pub fn raw(level: i8) -> Self {
        DeflateConfig {
            level,
            window_bits: -MAX_WBITS,
            ..DeflateConfig::default()
        }
    }

    /// The gzip member shape (`windowBits` 16 + 15) — `zlib:gzip/1`.
    pub fn gzip(level: i8) -> Self {
        DeflateConfig {
            level,
            window_bits: crate::GZIP_WBITS_OFFSET + MAX_WBITS,
            ..DeflateConfig::default()
        }
    }
}

/// A `deflate` stream (`z_stream` in the `INIT`/`BUSY`/`FINISH` state machine).
pub struct Deflate {
    config: DeflateConfig,
}

impl Deflate {
    /// `deflateInit2_` — allocate the window, hash chains and symbol buffer.
    pub fn new(config: DeflateConfig) -> Result<Deflate> {
        Err(BeamError::BadArg).map(|()| Deflate { config })
    }

    /// One-shot `deflateInit` + `deflate(Z_FINISH)` + `deflateEnd`: the whole stream.
    pub fn compress_with(config: DeflateConfig, input: &[u8]) -> Result<Vec<u8>> {
        let mut stream = Deflate::new(config)?;
        let mut out = vec![0u8; input.len() / 2 + 1024];
        let pump = stream.deflate(input, &mut out, Flush::Finish)?;
        let _ = pump;
        Ok(out)
    }

    /// One `deflate()` pump step: consume at most `input.len()` bytes, write at most
    /// `output.len()` bytes, and report zlib's return code with both counts.
    pub fn deflate(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: Flush,
    ) -> Result<Pump> {
        let _ = (input, output, flush);
        Err(BeamError::BadArg)
    }

    /// `deflateReset` — start a new stream with the same configuration.
    pub fn reset(&mut self) -> Result<()> {
        Err(BeamError::BadArg)
    }

    /// `deflateSetDictionary` — the Adler-32 of the dictionary is returned.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> Result<u32> {
        let _ = dictionary;
        Err(BeamError::BadArg)
    }

    /// `deflateParams` — change the level/strategy mid-stream, flushing the current block
    /// into `output` the way the C does.
    pub fn set_params(
        &mut self,
        level: i8,
        strategy: Strategy,
        output: &mut [u8],
    ) -> Result<Pump> {
        let _ = (level, strategy, output);
        Err(BeamError::BadArg)
    }

    /// The configuration this stream was created with.
    pub fn config(&self) -> DeflateConfig {
        self.config
    }
}
