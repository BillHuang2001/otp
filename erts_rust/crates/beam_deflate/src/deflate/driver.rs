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
//! The `deflate()` driver — `../../../erts/emulator/zlib/deflate.c:981-1290`.
//!
//! This is the pump the streaming API exposes, and its *call shape* is observable: the
//! `zlib:deflate/3` / `zlib:deflate/4` wrappers feed input in 8 KiB pieces and hand the
//! NIF an 8 KiB output buffer, so how many bytes come back per call (and whether the call
//! returns `Z_OK`, `Z_BUF_ERROR` or `Z_STREAM_END`) is part of the contract. Everything
//! here therefore exists to keep the C's *ordering*: flush the pending buffer first and
//! bail out with `Z_OK` if that filled the caller's buffer, refuse a redundant flush with
//! `Z_BUF_ERROR`, write the wrapper header, run the strategy function, then — only for
//! `Z_FINISH` — write the trailer.
//!
//! Two conventions the C spells out and this port keeps:
//!
//! * `last_flush` is set to `-1` (not to the flush mode) whenever the call ends because
//!   `avail_out` hit zero, so the next call's `RANK(flush) <= RANK(old_flush)` check
//!   cannot fire and turn "the caller gave me a full buffer" into a spurious
//!   `Z_BUF_ERROR`.
//! * `wrap` is negated once the trailer is written, so a repeated `Z_FINISH` returns
//!   `Z_STREAM_END` instead of emitting a second trailer.

use crate::{Flush, Status};
use super::lz77::BlockState;
use super::state::{
    DeflateState, BUSY_STATE, COMMENT_STATE, EXTRA_STATE, FINISH_STATE, GZIP_STATE, HCRC_STATE,
    INIT_STATE, NAME_STATE, OS_CODE, PRESET_DICT, Z_BLOCK, Z_DEFLATED, Z_FINISH, Z_FULL_FLUSH,
    Z_HUFFMAN_ONLY, Z_NO_FLUSH, Z_PARTIAL_FLUSH,
};
use super::window::Io;
use crate::checksum::crc32;

/// `RANK(f)` (`deflate.c:150`): the flush modes' ordering, with `Z_BLOCK` ranked *below*
/// `Z_PARTIAL_FLUSH` because it is the weakest thing that can be asked of the encoder.
fn rank(f: i32) -> i32 {
    f * 2 - if f > 4 { 9 } else { 0 }
}

impl DeflateState {
    /// `flush_pending(strm)` (`deflate.c:950`): drain `_tr_flush_bits` and as much of the
    /// pending buffer as the caller's output can take.
    pub(super) fn flush_pending(&mut self, io: &mut Io) {
        self.flush_bits();
        let len = self.pending.min(io.avail_out());
        if len == 0 {
            return;
        }
        let at = self.pending_out;
        io.write_out(&self.pending_buf[at..at + len]);
        self.pending_out += len;
        self.total_out += len as u64;
        self.pending -= len;
        if self.pending == 0 {
            self.pending_out = 0;
        }
    }

    /// Append `src` at the current `pending` cursor without advancing it — the C's raw
    /// `zmemcpy(s->pending_buf + s->pending, ...)` in the gzip header paths, which own
    /// their own `pending` accounting.
    fn copy_into_pending(&mut self, src: &[u8]) {
        let at = self.pending;
        let end = at + src.len();
        if end > self.pending_buf.len() {
            self.pending_buf.resize(end, 0);
        }
        self.pending_buf[at..end].copy_from_slice(src);
    }

    /// `HCRC_UPDATE(beg)` (`deflate.c:972`): fold the pending bytes from `beg` into the
    /// header CRC.
    fn hcrc_update(&mut self, beg: usize) {
        let hcrc = self.gzhead.as_ref().map(|h| h.hcrc).unwrap_or(false);
        if hcrc && self.pending > beg {
            let pending = self.pending;
            self.adler = crc32(self.adler, &self.pending_buf[beg..pending]);
        }
    }

    /// `XFL` — the gzip extra-flags byte both header paths compute from the level and
    /// strategy.
    fn gzip_xfl(&self) -> u8 {
        if self.level == 9 {
            2
        } else if self.strategy >= Z_HUFFMAN_ONLY || self.level < 2 {
            4
        } else {
            0
        }
    }

    /// The `GZIP_STATE` … `HCRC_STATE` half of `deflate()`: write the gzip member header
    /// (fixed fields, then `extra`, `name`, `comment` and the header CRC as each state is
    /// reached).
    ///
    /// Returns `Some(status)` when the C's `return Z_OK` early exit fires (the pending
    /// buffer did not fit in the caller's output), `None` to continue into the compressor.
    fn write_gzip_header(&mut self, io: &mut Io) -> Option<Status> {
        self.adler = crc32(0, &[]);
        self.put_byte(31);
        self.put_byte(139);
        self.put_byte(8);
        match self.gzhead.clone() {
            None => {
                self.put_byte(0);
                self.put_byte(0);
                self.put_byte(0);
                self.put_byte(0);
                self.put_byte(0);
                let xfl = self.gzip_xfl();
                self.put_byte(xfl);
                self.put_byte(OS_CODE);
                self.status = BUSY_STATE;
                self.flush_pending(io);
                if self.pending != 0 {
                    self.last_flush = -1;
                    return Some(Status::Ok);
                }
            }
            Some(h) => {
                let mut flg = 0u8;
                if h.text {
                    flg += 1;
                }
                if h.hcrc {
                    flg += 2;
                }
                if h.extra.is_some() {
                    flg += 4;
                }
                if h.name.is_some() {
                    flg += 8;
                }
                if h.comment.is_some() {
                    flg += 16;
                }
                self.put_byte(flg);
                self.put_byte((h.time & 0xff) as u8);
                self.put_byte(((h.time >> 8) & 0xff) as u8);
                self.put_byte(((h.time >> 16) & 0xff) as u8);
                self.put_byte(((h.time >> 24) & 0xff) as u8);
                let xfl = self.gzip_xfl();
                self.put_byte(xfl);
                self.put_byte(h.os & 0xff);
                if let Some(extra) = &h.extra {
                    let len = extra.len() & 0xffff;
                    self.put_byte((len & 0xff) as u8);
                    self.put_byte(((len >> 8) & 0xff) as u8);
                }
                if h.hcrc {
                    let pending = self.pending;
                    self.adler = crc32(self.adler, &self.pending_buf[..pending]);
                }
                self.gzindex = 0;
                self.status = EXTRA_STATE;
            }
        }

        if self.status == EXTRA_STATE {
            let extra = self.gzhead.as_ref().and_then(|h| h.extra.clone());
            if let Some(extra) = extra {
                let extra_len = extra.len() & 0xffff;
                let mut beg = self.pending;
                let mut left = extra_len - self.gzindex;
                while self.pending + left > self.pending_buf_size {
                    let copy = self.pending_buf_size - self.pending;
                    let at = self.gzindex;
                    self.copy_into_pending(&extra[at..at + copy]);
                    self.pending = self.pending_buf_size;
                    self.hcrc_update(beg);
                    self.gzindex += copy;
                    self.flush_pending(io);
                    if self.pending != 0 {
                        self.last_flush = -1;
                        return Some(Status::Ok);
                    }
                    beg = 0;
                    left -= copy;
                }
                let at = self.gzindex;
                self.copy_into_pending(&extra[at..at + left]);
                self.pending += left;
                self.hcrc_update(beg);
                self.gzindex = 0;
            }
            self.status = NAME_STATE;
        }

        if self.status == NAME_STATE {
            let name = self.gzhead.as_ref().and_then(|h| h.name.clone());
            if let Some(name) = name {
                let mut beg = self.pending;
                loop {
                    if self.pending == self.pending_buf_size {
                        self.hcrc_update(beg);
                        self.flush_pending(io);
                        if self.pending != 0 {
                            self.last_flush = -1;
                            return Some(Status::Ok);
                        }
                        beg = 0;
                    }
                    // The C walks a NUL-terminated string; this port stores the bytes
                    // without the terminator and synthesizes it.
                    let val = if self.gzindex < name.len() {
                        name[self.gzindex]
                    } else {
                        0
                    };
                    self.gzindex += 1;
                    self.put_byte(val);
                    if val == 0 {
                        break;
                    }
                }
                self.hcrc_update(beg);
                self.gzindex = 0;
            }
            self.status = COMMENT_STATE;
        }

        if self.status == COMMENT_STATE {
            let comment = self.gzhead.as_ref().and_then(|h| h.comment.clone());
            if let Some(comment) = comment {
                let mut beg = self.pending;
                loop {
                    if self.pending == self.pending_buf_size {
                        self.hcrc_update(beg);
                        self.flush_pending(io);
                        if self.pending != 0 {
                            self.last_flush = -1;
                            return Some(Status::Ok);
                        }
                        beg = 0;
                    }
                    let val = if self.gzindex < comment.len() {
                        self.gzindex += 1;
                        comment[self.gzindex - 1]
                    } else {
                        0
                    };
                    self.put_byte(val);
                    if val == 0 {
                        break;
                    }
                }
                self.hcrc_update(beg);
            }
            self.status = HCRC_STATE;
        }

        if self.status == HCRC_STATE {
            if self.gzhead.as_ref().map(|h| h.hcrc).unwrap_or(false) {
                if self.pending + 2 > self.pending_buf_size {
                    self.flush_pending(io);
                    if self.pending != 0 {
                        self.last_flush = -1;
                        return Some(Status::Ok);
                    }
                }
                let adler = self.adler;
                self.put_byte((adler & 0xff) as u8);
                self.put_byte(((adler >> 8) & 0xff) as u8);
                self.adler = crc32(0, &[]);
            }
            self.status = BUSY_STATE;
            self.flush_pending(io);
            if self.pending != 0 {
                self.last_flush = -1;
                return Some(Status::Ok);
            }
        }
        None
    }

    /// `deflate(strm, flush)` (`deflate.c:981`) — one pump step.
    pub(super) fn deflate_step(&mut self, io: &mut Io, flush: Flush) -> Status {
        let flush = flush.code();
        if flush > Z_BLOCK || flush < 0 {
            return Status::StreamError;
        }
        if self.status == FINISH_STATE && flush != Z_FINISH {
            return Status::StreamError;
        }
        if io.avail_out() == 0 {
            return Status::BufError;
        }
        let old_flush = self.last_flush;
        self.last_flush = flush;

        // Flush as much pending output as possible.
        if self.pending != 0 {
            self.flush_pending(io);
            if io.avail_out() == 0 {
                // Since avail_out is 0, deflate will be called again with more output
                // space, but possibly with both pending and avail_in equal to zero. There
                // won't be anything to do, but this is not an error situation, so make
                // sure the next call returns Z_OK instead of Z_BUF_ERROR.
                self.last_flush = -1;
                return Status::Ok;
            }
        } else if io.avail_in() == 0 && rank(flush) <= rank(old_flush) && flush != Z_FINISH {
            // Make sure there is something to do and avoid duplicate consecutive flushes.
            // For repeated and useless calls with Z_FINISH we keep returning
            // Z_STREAM_END instead of Z_BUF_ERROR.
            return Status::BufError;
        }

        // The caller must not provide more input after the first FINISH.
        if self.status == FINISH_STATE && io.avail_in() != 0 {
            return Status::BufError;
        }

        // Write the header.
        if self.status == INIT_STATE && self.wrap == 0 {
            self.status = BUSY_STATE;
        }
        if self.status == INIT_STATE {
            let mut header = (Z_DEFLATED + ((self.w_bits - 8) << 4)) << 8;
            let level_flags = if self.strategy >= Z_HUFFMAN_ONLY || self.level < 2 {
                0
            } else if self.level < 6 {
                1
            } else if self.level == 6 {
                2
            } else {
                3
            };
            header |= level_flags << 6;
            if self.strstart != 0 {
                header |= PRESET_DICT;
            }
            header += 31 - (header % 31);
            self.put_short_msb(header);
            // Save the Adler-32 of the preset dictionary.
            if self.strstart != 0 {
                let adler = self.adler;
                self.put_short_msb(adler >> 16);
                self.put_short_msb(adler & 0xffff);
            }
            self.adler = crate::checksum::adler32(0, &[]);
            self.status = BUSY_STATE;
            // Compression must start with an empty pending buffer.
            self.flush_pending(io);
            if self.pending != 0 {
                self.last_flush = -1;
                return Status::Ok;
            }
        }
        if self.status == GZIP_STATE {
            if let Some(status) = self.write_gzip_header(io) {
                return status;
            }
        }

        // Start a new block or continue the current one.
        if io.avail_in() != 0
            || self.lookahead != 0
            || (flush != Z_NO_FLUSH && self.status != FINISH_STATE)
        {
            let bstate = self.run_strategy(io, flush);
            if bstate == BlockState::FinishStarted || bstate == BlockState::FinishDone {
                self.status = FINISH_STATE;
            }
            if bstate == BlockState::NeedMore || bstate == BlockState::FinishStarted {
                if io.avail_out() == 0 {
                    self.last_flush = -1; // avoid BUF_ERROR next call, see above
                }
                return Status::Ok;
            }
            if bstate == BlockState::BlockDone {
                if flush == Z_PARTIAL_FLUSH {
                    self.align();
                } else if flush != Z_BLOCK {
                    // FULL_FLUSH or SYNC_FLUSH.
                    self.stored_block(None, 0, false);
                    // For a full flush, this empty block is recognized as a special marker
                    // by inflate_sync().
                    if flush == Z_FULL_FLUSH {
                        self.clear_hash();
                        if self.lookahead == 0 {
                            self.strstart = 0;
                            self.block_start = 0;
                            self.insert = 0;
                        }
                    }
                }
                self.flush_pending(io);
                if io.avail_out() == 0 {
                    self.last_flush = -1; // avoid BUF_ERROR at next call, see above
                    return Status::Ok;
                }
            }
        }

        if flush != Z_FINISH {
            return Status::Ok;
        }
        if self.wrap <= 0 {
            return Status::StreamEnd;
        }

        // Write the trailer.
        if self.wrap == 2 {
            let adler = self.adler;
            let total_in = self.total_in;
            self.put_byte((adler & 0xff) as u8);
            self.put_byte(((adler >> 8) & 0xff) as u8);
            self.put_byte(((adler >> 16) & 0xff) as u8);
            self.put_byte(((adler >> 24) & 0xff) as u8);
            self.put_byte((total_in & 0xff) as u8);
            self.put_byte(((total_in >> 8) & 0xff) as u8);
            self.put_byte(((total_in >> 16) & 0xff) as u8);
            self.put_byte(((total_in >> 24) & 0xff) as u8);
        } else {
            let adler = self.adler;
            self.put_short_msb(adler >> 16);
            self.put_short_msb(adler & 0xffff);
        }
        self.flush_pending(io);
        // If avail_out is zero, the application will call deflate again to flush the rest.
        if self.wrap > 0 {
            self.wrap = -self.wrap; // write the trailer only once!
        }
        if self.pending != 0 {
            Status::Ok
        } else {
            Status::StreamEnd
        }
    }
}
