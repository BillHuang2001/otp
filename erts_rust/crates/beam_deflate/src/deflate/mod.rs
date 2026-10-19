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
//! ```text
//! deflateInit2(Z,Level,Method,WindowBits,MemLevel,Strat)   Deflate::new(DeflateConfig::new(..))
//! deflate(Z, In, Flush)                                    Deflate::deflate(in, out, flush)
//! deflateParams(Z, Level, Strat)                           Deflate::set_params(level, strat, out)
//! deflateSetDictionary(Z, Dict)                            Deflate::set_dictionary(dict)
//! deflateSetHeader(Z, Head)                                Deflate::set_header(head)
//! deflateReset(Z)                                          Deflate::reset()
//! ```
//!
//! # Shape
//!
//! The module is split the way the C is, so the two can be read side by side:
//!
//! | module | C |
//! |---|---|
//! | [`state`] | `deflate.h`'s `deflate_state`, `configuration_table`, `lm_init` |
//! | [`window`] | `read_buf`/`fill_window` and the `next_in`/`next_out` cursors |
//! | [`tables`] | `trees.h`'s static trees and length/distance tables |
//! | [`trees`] | `trees.c` — the bit writer, Huffman construction, block emission |
//! | [`lz77`] | `longest_match` and the five `block_state` strategy functions |
//! | [`driver`] | `deflate()` itself, `flush_pending`, the zlib/gzip headers and trailers |
//!
//! # What is exact
//!
//! Every parameter combination `deflateInit2` accepts is accepted here and produces the
//! C's bytes: levels `0..=9` (and `-1` for 6) against every strategy, `windowBits`
//! `8..=15` (zlib), `-15..=-8` (raw) and `16 + 8 ..= 16 + 15` (gzip), and `memLevel`
//! `1..=9`. A rejected combination is [`beam_common::BeamError::BadArg`] — the C's
//! `Z_STREAM_ERROR` from `deflateInit2_` — and never a merely-conforming stream.

use beam_common::{BeamError, Result};

use crate::checksum::adler32;
use crate::{Flush, Method, Pump, Status, Strategy, DEFAULT_COMPRESSION, DEFAULT_MEM_LEVEL, MAX_WBITS};

mod driver;
mod lz77;
mod state;
mod tables;
mod trees;
mod window;

use state::{DeflateState, INIT_STATE, MIN_MATCH, Z_DEFAULT_STRATEGY};
use window::Io;

/// `MAX_MEM_LEVEL` (`zconf.h:277`) — the largest `memLevel` `deflateInit2` accepts.
const MAX_MEM_LEVEL: i32 = 9;
/// `deflateInit2_`'s 8 → 9 window bump: `if (windowBits == 8) windowBits = 9;  /* until
/// 256-byte window bug fixed */`.
const MIN_DEFLATE_WBITS: i32 = 9;
/// `?DEFLATE_IN_CHUNKSIZE` (`zlib.erl:141`) — the input piece the `zlib` module hands the
/// NIF per `deflate` call (`8 bsl 10`).
const ZLIB_IN_CHUNK: usize = 8 << 10;
/// `?DEFLATE_OUT_CHUNKSIZE` (`zlib.erl:142`) — the output buffer the NIF allocates per
/// `zlib:deflate/3` call (`8 bsl 10`). `deflate_stored`'s block sizes depend on it.
const ZLIB_OUT_CHUNK: usize = 8 << 10;
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

/// A user-supplied gzip member header — `gz_header` (`zlib.h:1720`) as `deflateSetHeader`
/// takes it.
///
/// Only meaningful for a gzip-framed stream (`windowBits` `16 + 8 ..= 16 + 15`); setting
/// it on a zlib or raw stream is rejected by [`Deflate::set_header`], exactly as
/// `deflateSetHeader` refuses a `deflate_state` whose `wrap` is not 2.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct GzHeader {
    /// `text` — set the header's "text" flag, which tells the decoder the content is
    /// probably ASCII.
    pub text: bool,
    /// `hcrc` — also emit a CRC-16 of the header itself (`FHCRC`).
    pub hcrc: bool,
    /// `time` — the modification time, in seconds since the Unix epoch (0 means "no
    /// timestamp available").
    pub time: u32,
    /// `os` — the operating-system byte. The C writes `gzhead->os` here; without a header
    /// it writes `OS_CODE` (3 on Unix).
    pub os: u8,
    /// `extra` — the `FEXTRA` payload, emitted with its own 16-bit length prefix.
    pub extra: Option<Vec<u8>>,
    /// `name` — the original file name (`FNAME`). The C expects a NUL-terminated C
    /// string; this port stores the bytes **without** the terminator and appends it.
    pub name: Option<Vec<u8>>,
    /// `comment` — the `FCOMMENT` payload, same terminator convention as [`Self::name`].
    pub comment: Option<Vec<u8>>,
}

impl GzHeader {
    /// A header carrying only an `mtime` — the common case.
    pub fn with_time(time: u32) -> Self {
        GzHeader {
            time,
            ..GzHeader::default()
        }
    }
}

/// A `deflate` stream (`z_stream` in the `INIT`/`BUSY`/`FINISH` state machine).
pub struct Deflate {
    config: DeflateConfig,
    state: DeflateState,
}

impl Deflate {
    /// `deflateInit2_` — allocate the window, hash chains and symbol buffer.
    ///
    /// Parameter validation and normalization are `deflateInit2_`'s line for line:
    /// `-1` becomes 6, a negative `windowBits` selects wrap 0 (raw) and is negated, an
    /// over-15 one selects wrap 2 (gzip) and has 16 subtracted, then the C's combined
    /// range test runs, and finally `windowBits == 8` is bumped to 9 (the window is
    /// *two* 2^bits areas, so 8 would leave no room for the lookahead).
    pub fn new(config: DeflateConfig) -> Result<Deflate> {
        let mut level = i32::from(config.level);
        if level == DEFAULT_COMPRESSION as i32 {
            level = 6;
        }
        let mut window_bits = i32::from(config.window_bits);
        let wrap;
        if window_bits < 0 {
            // suppress the zlib wrapper
            wrap = 0;
            if window_bits < -15 {
                return Err(BeamError::BadArg);
            }
            window_bits = -window_bits;
        } else if window_bits > 15 {
            wrap = 2; // write a gzip wrapper instead
            window_bits -= 16;
        } else {
            wrap = 1;
        }
        let mem_level = i32::from(config.mem_level);
        if !(1..=MAX_MEM_LEVEL).contains(&mem_level)
            || !(8..=15).contains(&window_bits)
            || !(0..=9).contains(&level)
            || (window_bits == 8 && wrap != 1)
        {
            return Err(BeamError::BadArg);
        }
        let w_bits = if window_bits == 8 {
            MIN_DEFLATE_WBITS as u32
        } else {
            window_bits as u32
        };
        Ok(Deflate {
            config,
            state: DeflateState::new(
                level,
                config.strategy.code(),
                wrap,
                w_bits,
                mem_level as u32,
            ),
        })
    }

    /// One-shot `deflateInit` + `deflate(Z_FINISH)` + `deflateEnd`: the whole stream.
    ///
    /// Byte-identical to `zlib:deflate(Z, Data, finish)` — the image `zlib:compress/1`,
    /// `zlib:zip/1`, `zlib:gzip/1` and `zlib:deflate(Z, Data, finish)` return.
    ///
    /// The *pump schedule* is part of that contract, not an implementation detail:
    /// `zlib.erl` hands the NIF `?DEFLATE_IN_CHUNKSIZE`/`?DEFLATE_OUT_CHUNKSIZE` (8 KiB
    /// each), and the NIF (`zlib_nif.c:zlib_flush_queue`) calls `deflate(Z_NO_FLUSH)` once
    /// per input piece until the queue is drained, then calls `deflate(flush)` with an
    /// empty input in the *same* 8 KiB output buffer, and finally keeps re-entering with a
    /// fresh 8 KiB buffer while the last call produced a full one. `deflate_stored` sizes
    /// its blocks from `avail_in`/`avail_out` (`deflate.c:1668-1780`), so a level-0 stream
    /// carries that schedule in its bytes; reproducing it is what makes
    /// `Deflate::compress_with` agree with the oracle rather than merely inflate.
    pub fn compress_with(config: DeflateConfig, input: &[u8]) -> Result<Vec<u8>> {
        Deflate::compress_pumped(config, input, ZLIB_IN_CHUNK, ZLIB_OUT_CHUNK)
    }

    /// [`Deflate::compress_with`] with an explicit pump schedule — the same `deflate()`
    /// calls, with `in_chunk`/`out_chunk` standing in for the `zlib` module's 8 KiB.
    ///
    /// A smaller `out_chunk` makes the driver return `Z_OK` more often and a smaller
    /// `in_chunk` feeds `fill_window` different pieces; neither changes the *bytes* for
    /// levels 1..=9, but level 0's stored-block split is a function of both, exactly as in
    /// the C. Kept private: the public one-shot shape is the oracle's.
    fn compress_pumped(
        config: DeflateConfig,
        input: &[u8],
        in_chunk: usize,
        out_chunk: usize,
    ) -> Result<Vec<u8>> {
        let mut stream = Deflate::new(config)?;
        let mut out = Vec::new();
        let mut buf = vec![0u8; out_chunk];
        let mut at = 0usize;
        loop {
            let mut produced = 0usize;
            let mut consumed = 0usize;
            // One `deflate(Z_NO_FLUSH)` per input piece, into a fresh output buffer.
            if at < input.len() {
                let end = (at + in_chunk).min(input.len());
                let pump = stream.deflate(&input[at..end], &mut buf, Flush::None)?;
                if pump.consumed == 0 && pump.produced == 0 {
                    // The C's NIF turns "no progress with input outstanding" into
                    // `Z_DATA_ERROR` (`zlib_nif.c:439`); it cannot happen for a stream this
                    // module created, and it is a rejected configuration (a zero-sized
                    // output buffer), never a merely-conforming stream.
                    return Err(BeamError::BadArg);
                }
                at += pump.consumed;
                consumed = pump.consumed;
                produced = pump.produced;
            }
            if at == input.len() && produced < out_chunk {
                // The flush pump rides in the same output buffer, as the NIF's does.
                let pump = stream.deflate(&[], &mut buf[produced..], Flush::Finish)?;
                produced += pump.produced;
                out.extend_from_slice(&buf[..produced]);
                match pump.status {
                    Status::StreamEnd => return Ok(out),
                    // `Z_OK` means the 8 KiB buffer filled: the caller comes back with a
                    // fresh one, exactly as `dequeue_all_chunks/2` does.
                    Status::Ok => {}
                    _ => return Err(BeamError::BadArg),
                }
            } else {
                out.extend_from_slice(&buf[..produced]);
                if consumed == 0 && produced == 0 {
                    return Err(BeamError::BadArg);
                }
            }
        }
    }
    /// One `deflate()` pump step: consume at most `input.len()` bytes, write at most
    /// `output.len()` bytes, and report zlib's return code with both counts.
    ///
    /// `Pump::consumed`/`Pump::produced` are the C's `avail_in`/`avail_out` deltas, so a
    /// caller that mirrors `zlib:deflate/3` (feed a chunk, take what comes back) sees the
    /// same chunking the oracle does.
    pub fn deflate(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Result<Pump> {
        let mut io = Io::new(input, output);
        let status = self.state.deflate_step(&mut io, flush);
        Ok(Pump {
            status,
            consumed: io.in_pos,
            produced: io.out_pos,
        })
    }

    /// `deflateReset` — start a new stream with the same configuration.
    pub fn reset(&mut self) -> Result<()> {
        self.state.reset_keep();
        self.state.lm_init();
        Ok(())
    }

    /// `deflateSetDictionary` — the Adler-32 of the dictionary is returned.
    ///
    /// The dictionary becomes the stream's LZ77 history and (for zlib framing) is
    /// checksummed into `strm->adler`, which the header then carries as the preset
    /// dictionary id with the `PRESET_DICT` flag set. Called on a gzip stream, on an
    /// already-started zlib stream, or once the window holds data, it is
    /// [`beam_common::BeamError::BadArg`] — the C's `Z_STREAM_ERROR`.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> Result<u32> {
        let s = &mut self.state;
        let wrap = s.wrap;
        if wrap == 2 || (wrap == 1 && s.status != INIT_STATE) || s.lookahead != 0 {
            return Err(BeamError::BadArg);
        }
        // With the zlib wrapper, the dictionary's Adler-32 goes into the header.
        if wrap == 1 {
            s.adler = adler32(s.adler, dictionary);
        }
        s.wrap = 0; // avoid computing Adler-32 in read_buf

        let mut dict = dictionary;
        if dict.len() >= s.w_size as usize {
            if wrap == 0 {
                // already empty otherwise
                s.clear_hash();
                s.strstart = 0;
                s.block_start = 0;
                s.insert = 0;
            }
            dict = &dict[dict.len() - s.w_size as usize..]; // use the tail
        }

        // Insert the dictionary into the window and the hash chains. The C temporarily
        // points `next_in` at the dictionary and reuses `fill_window`.
        let mut scratch: [u8; 0] = [];
        let mut io = Io::new(dict, &mut scratch);
        s.fill_window(&mut io);
        while s.lookahead >= MIN_MATCH {
            let mut str = s.strstart;
            let mut n = s.lookahead - (MIN_MATCH - 1);
            loop {
                let c = s.window[str as usize + MIN_MATCH as usize - 1];
                s.update_hash(c);
                let h = (s.ins_h & s.hash_mask) as usize;
                let slot = (str & s.w_mask) as usize;
                s.prev[slot] = s.head[h];
                s.head[h] = str as u16;
                str += 1;
                n -= 1;
                if n == 0 {
                    break;
                }
            }
            s.strstart = str;
            s.lookahead = MIN_MATCH - 1;
            s.fill_window(&mut io);
        }
        s.strstart += s.lookahead;
        s.block_start = i64::from(s.strstart);
        s.insert = s.lookahead;
        s.lookahead = 0;
        s.match_length = MIN_MATCH - 1;
        s.prev_length = MIN_MATCH - 1;
        s.match_available = false;
        s.wrap = wrap;
        Ok(s.adler)
    }

    /// `deflateSetHeader` — attach a [`GzHeader`] to a gzip-framed stream.
    ///
    /// [`beam_common::BeamError::BadArg`] unless the stream is gzip-framed (the C's
    /// `wrap != 2` check).
    pub fn set_header(&mut self, header: GzHeader) -> Result<()> {
        if self.state.wrap != 2 {
            return Err(BeamError::BadArg);
        }
        self.state.gzhead = Some(header);
        Ok(())
    }

    /// `deflateParams` — change the level/strategy mid-stream, flushing the current block
    /// into `output` the way the C does.
    ///
    /// When the *strategy function* changes (or the strategy itself does) and the stream
    /// has already been started, the C first runs `deflate(strm, Z_BLOCK)` so the pending
    /// block is closed with the settings it was built under; if that leaves anything
    /// unwritten the call reports [`Status::BufError`], and the caller must retry with a
    /// bigger buffer (the level/strategy change has *not* been applied yet).
    /// Switching away from level 0 replays the hash-table slides `deflate_stored`
    /// deferred (`matches`), clearing instead when two or more are owed.
    pub fn set_params(
        &mut self,
        level: i8,
        strategy: Strategy,
        output: &mut [u8],
    ) -> Result<Pump> {
        let mut level_i = i32::from(level);
        if level_i == DEFAULT_COMPRESSION as i32 {
            level_i = 6;
        }
        if !(0..=9).contains(&level_i) {
            return Err(BeamError::BadArg);
        }
        let strategy_i = strategy.code();
        let s = &mut self.state;
        let func = state::CONFIGURATION_TABLE[s.level as usize].func;
        let mut consumed = 0;
        let mut produced = 0;
        let mut status = Status::Ok;
        if (strategy_i != s.strategy
            || func != state::CONFIGURATION_TABLE[level_i as usize].func)
            && s.last_flush != -2
        {
            // Flush the last buffer.
            let mut io = Io::new(&[], output);
            status = s.deflate_step(&mut io, Flush::Block);
            consumed = io.in_pos;
            produced = io.out_pos;
            if status == Status::StreamError {
                return Ok(Pump {
                    status,
                    consumed,
                    produced,
                });
            }
            if io.avail_in() != 0
                || (i64::from(s.strstart) - s.block_start) + i64::from(s.lookahead) != 0
            {
                return Ok(Pump {
                    status: Status::BufError,
                    consumed,
                    produced,
                });
            }
        }
        if s.level != level_i {
            if s.level == 0 && s.matches != 0 {
                if s.matches == 1 {
                    s.slide_hash();
                } else {
                    s.clear_hash();
                }
                s.matches = 0;
            }
            s.level = level_i;
            s.max_lazy_match = state::CONFIGURATION_TABLE[level_i as usize].max_lazy;
            s.good_match = state::CONFIGURATION_TABLE[level_i as usize].good_length;
            s.nice_match = state::CONFIGURATION_TABLE[level_i as usize].nice_length;
            s.max_chain_length = state::CONFIGURATION_TABLE[level_i as usize].max_chain;
        }
        s.strategy = strategy_i;
        Ok(Pump {
            status,
            consumed,
            produced,
        })
    }

    /// The configuration this stream was created with.
    pub fn config(&self) -> DeflateConfig {
        self.config
    }
}

/// `Z_DEFAULT_STRATEGY` is what [`DeflateConfig::default`] selects; pinned here so a
/// change to [`Strategy`]'s discriminants fails to build.
const _: () = assert!(Z_DEFAULT_STRATEGY == 0);
