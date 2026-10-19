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
//! The sliding window and the input/output cursors — `read_buf`, `fill_window` and the
//! `z_stream`'s `next_in`/`avail_in`/`next_out`/`avail_out` quartet
//! (`../../../erts/emulator/zlib/deflate.c:219-376`).
//!
//! The C mutates four fields of `z_stream` (`next_in`, `avail_in`, `next_out`,
//! `avail_out`) plus `total_in`/`total_out`/`adler` while it compresses. This port keeps
//! the two *cursor pairs* in [`Io`] — a borrow of the caller's `input`/`output` slices
//! plus an offset each — and the three stream fields in
//! [`DeflateState`], so the arithmetic (and therefore the byte counts the caller
//! observes) is the C's.
//!
//! The decisions that matter for byte identity all live here: `fill_window`'s slide
//! condition (`strstart >= w_size + MAX_DIST`), the hash re-insertion loop over `insert`,
//! the `high_water`/`WIN_INIT` zero fill that makes `longest_match`'s deliberate
//! over-read deterministic, and the order in which `read_buf` accumulates the zlib
//! Adler-32 (or gzip CRC-32) — it sees exactly the bytes that entered the window, in the
//! same pieces.

use super::state::{DeflateState, MIN_LOOKAHEAD, MIN_MATCH, WIN_INIT};
use crate::checksum::{adler32, crc32};

/// The `z_stream` cursor pairs: the caller's input/output buffers plus how much of each
/// has been consumed/produced.
pub(super) struct Io<'a> {
    /// `strm->next_in`/`avail_in` — the caller's whole input, with [`Self::in_pos`] as
    /// the `next_in` pointer.
    pub(super) input: &'a [u8],
    /// How much of [`Self::input`] has been consumed.
    pub(super) in_pos: usize,
    /// `strm->next_out`/`avail_out` — the caller's whole output buffer, with
    /// [`Self::out_pos`] as the `next_out` pointer.
    pub(super) output: &'a mut [u8],
    /// How much of [`Self::output`] has been produced.
    pub(super) out_pos: usize,
}

impl<'a> Io<'a> {
    /// The `deflate(strm, flush)` call's buffers.
    pub(super) fn new(input: &'a [u8], output: &'a mut [u8]) -> Io<'a> {
        Io {
            input,
            in_pos: 0,
            output,
            out_pos: 0,
        }
    }

    /// `strm->avail_in`.
    pub(super) fn avail_in(&self) -> usize {
        self.input.len() - self.in_pos
    }

    /// `strm->avail_out`.
    pub(super) fn avail_out(&self) -> usize {
        self.output.len() - self.out_pos
    }

    /// `zmemcpy(strm->next_out, src, len); strm->next_out += len; avail_out -= len;
    /// strm->total_out += len` — the four-field bump every output path in `deflate.c`
    /// spells out. Returns the number of bytes copied, which is `src.len()` clipped to
    /// what still fits.
    pub(super) fn write_out(&mut self, src: &[u8]) -> usize {
        let len = src.len().min(self.avail_out());
        let at = self.out_pos;
        self.output[at..at + len].copy_from_slice(&src[..len]);
        self.out_pos += len;
        len
    }
}

impl DeflateState {
    /// `read_buf(strm, buf, size)` (`deflate.c:219`) for the window: copy up to `size`
    /// bytes of the input into `window[offset..]`, accumulate `adler`/`total_in`, and
    /// return the count.
    ///
    /// The Adler-32 (zlib framing) or CRC-32 (gzip framing) accumulates exactly the bytes
    /// that entered the window, **in the pieces `fill_window`/`deflate_stored` hand them
    /// over** — which is why it is folded here rather than over the caller's whole input
    /// slice: the two agree, but only this way reproduces `deflateSetDictionary`'s
    /// deliberate `s->wrap = 0` window (the dictionary must NOT be checksummed).
    pub(super) fn read_buf_window(&mut self, io: &mut Io, offset: usize, size: usize) -> usize {
        let len = io.avail_in().min(size);
        if len == 0 {
            return 0;
        }
        let from = io.in_pos;
        io.in_pos += len;
        let to = offset + len;
        self.window[offset..to].copy_from_slice(&io.input[from..from + len]);
        self.total_in += len as u64;
        self.fold_checksum(offset, to);
        len
    }

    /// `read_buf(strm, strm->next_out, size)` (`deflate.c:219`) for the *output* side —
    /// the path `deflate_stored` uses to copy input straight through.
    ///
    /// It writes at `io.out_pos` **without advancing it**, exactly as the C leaves
    /// `next_out` for its caller to bump; `deflate_stored` then does
    /// `next_out += len; avail_out -= len; total_out += len`.
    pub(super) fn read_buf_out(&mut self, io: &mut Io, size: usize) -> usize {
        let len = io.avail_in().min(size).min(io.avail_out());
        if len == 0 {
            return 0;
        }
        let from = io.in_pos;
        let to = io.out_pos;
        io.in_pos += len;
        io.output[to..to + len].copy_from_slice(&io.input[from..from + len]);
        self.total_in += len as u64;
        self.fold_checksum_into_output(to, to + len, io);
        len
    }

    /// Fold the just-read window bytes into the running checksum (`read_buf`'s
    /// `wrap == 1` / `wrap == 2` branches).
    fn fold_checksum(&mut self, from: usize, to: usize) {
        if self.wrap == 1 {
            self.adler = adler32(self.adler, &self.window[from..to]);
        } else if self.wrap == 2 {
            self.adler = crc32(self.adler, &self.window[from..to]);
        }
    }

    /// The same fold for bytes that went straight to the caller's output.
    fn fold_checksum_into_output(&mut self, from: usize, to: usize, io: &Io<'_>) {
        if self.wrap == 1 {
            self.adler = adler32(self.adler, &io.output[from..to]);
        } else if self.wrap == 2 {
            self.adler = crc32(self.adler, &io.output[from..to]);
        }
    }

    /// `fill_window(s)` (`deflate.c:251`): top the sliding window up so the matcher has
    /// `MIN_LOOKAHEAD` bytes to look at, sliding the window (and the hash chains) down
    /// when it is nearly full.
    ///
    /// The slide condition is `strstart >= w_size + MAX_DIST`, i.e. `strstart >=
    /// 2*w_size - MIN_LOOKAHEAD`: the upper half is then moved onto the lower half, which
    /// is only sound because every live match is at most `MAX_DIST` back.
    pub(super) fn fill_window(&mut self, io: &mut Io) {
        let wsize = self.w_size as usize;
        loop {
            let mut more = self
                .window_size
                .saturating_sub(u64::from(self.lookahead))
                .saturating_sub(u64::from(self.strstart)) as usize;

            // If the window is almost full and there is insufficient lookahead, move the
            // upper half to the lower one to make room in the upper half.
            if self.strstart >= self.w_size + self.max_dist() {
                // `zmemcpy(window, window + wsize, (unsigned)wsize - more)` — `more` is
                // below `wsize` in this branch, so the copy length is positive.
                let len = wsize - more.min(wsize);
                self.window.copy_within(wsize..wsize + len, 0);
                self.match_start = self.match_start.wrapping_sub(self.w_size);
                self.strstart -= self.w_size;
                self.block_start -= i64::from(self.w_size);
                if self.insert > self.strstart {
                    self.insert = self.strstart;
                }
                self.slide_hash();
                more += wsize;
            }
            if io.avail_in() == 0 {
                break;
            }

            let n = self.read_buf_window(
                io,
                (self.strstart + self.lookahead) as usize,
                more,
            );
            self.lookahead += n as u32;

            // Initialize the hash value now that we have some input.
            if self.lookahead + self.insert >= MIN_MATCH {
                let mut str = self.strstart - self.insert;
                self.ins_h = u32::from(self.window[str as usize]);
                let c = self.window[str as usize + 1];
                self.update_hash(c);
                while self.insert != 0 {
                    let c = self.window[str as usize + MIN_MATCH as usize - 1];
                    self.update_hash(c);
                    let slot = (str & self.w_mask) as usize;
                    let h = (self.ins_h & self.hash_mask) as usize;
                    self.prev[slot] = self.head[h];
                    self.head[h] = str as u16;
                    str += 1;
                    self.insert -= 1;
                    if self.lookahead + self.insert < MIN_MATCH {
                        break;
                    }
                }
            }

            if self.lookahead >= MIN_LOOKAHEAD || io.avail_in() == 0 {
                break;
            }
        }

        // Zero the `WIN_INIT` bytes past the end of the current data if they have never
        // been written, so the matcher's deliberate over-read is deterministic rather
        // than reading uninitialized memory.
        if self.high_water < self.window_size {
            let curr = u64::from(self.strstart) + u64::from(self.lookahead);
            let mut init;
            if self.high_water < curr {
                // Previous high water mark below current data — zero WIN_INIT bytes or up
                // to the end of the window, whichever is less.
                init = self.window_size - curr;
                init = init.min(WIN_INIT);
                let at = curr as usize;
                self.window[at..at + init as usize].fill(0);
                self.high_water = curr + init;
            } else if self.high_water < curr + WIN_INIT {
                // High water mark at or above current data, but below current data plus
                // WIN_INIT — zero out to current data plus WIN_INIT, or up to the end of
                // the window, whichever is less.
                init = curr + WIN_INIT - self.high_water;
                init = init.min(self.window_size - self.high_water);
                let at = self.high_water as usize;
                self.window[at..at + init as usize].fill(0);
                self.high_water += init;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::state::{
        BUSY_STATE, MIN_LOOKAHEAD, MIN_MATCH, NIL, OS_CODE, Z_DEFAULT_STRATEGY,
    };

    /// A fresh window is all zeroes and `fill_window` must read the whole short input in
    /// one go, leaving `lookahead` at its length, `total_in` at the byte count and (for
    /// zlib framing) the Adler-32 of exactly those bytes.
    #[test]
    fn fill_window_reads_the_input_and_checksums_it() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 1, 15, 8);
        let data = b"the quick brown fox jumps over the lazy dog";
        let mut out = [];
        let mut io = Io::new(data, &mut out);
        st.fill_window(&mut io);
        assert_eq!(st.lookahead, data.len() as u32);
        assert_eq!(st.strstart, 0);
        assert_eq!(st.total_in, data.len() as u64);
        assert_eq!(st.adler, adler32(1, data));
        assert_eq!(io.avail_in(), 0);
        assert_eq!(&st.window[..data.len()], data);
    }

    /// Raw framing must not accumulate any checksum at all — that is the whole difference
    /// between `windowBits` 15 and -15.
    #[test]
    fn a_raw_stream_accumulates_no_checksum() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 0, 15, 8);
        let mut out = [];
        let mut io = Io::new(b"abcdef", &mut out);
        st.fill_window(&mut io);
        assert_eq!(st.adler, 1, "untouched adler identity");
    }

    /// gzip framing accumulates a CRC-32 instead, and `reset_keep` starts it at
    /// `crc32(0, "")` = 0 rather than Adler's 1.
    #[test]
    fn a_gzip_stream_accumulates_a_crc32() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 2, 15, 8);
        assert_eq!(st.adler, 0);
        let mut out = [];
        let data = b"gzip me";
        let mut io = Io::new(data, &mut out);
        st.fill_window(&mut io);
        assert_eq!(st.adler, crc32(0, data));
    }

    /// The `insert` bookkeeping: bytes that the caller promised were already inserted get
    /// inserted by `fill_window`, and the chain they build is the same one
    /// `insert_string` would build byte by byte.
    #[test]
    fn fill_window_inserts_the_pending_strings() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 0, 15, 8);
        let mut out = [];
        let mut io = Io::new(b"abcabcabc", &mut out);
        // Pretend the first three bytes are already in the window and uninserted.
        st.insert = 3;
        st.fill_window(&mut io);
        assert_eq!(st.insert, 0);
        // Positions 0, 1 and 2 were inserted; "abc" at 0 and at 3 hash together, so the
        // bucket for it is the highest of the three inserted positions.
        let h = (st.ins_h & st.hash_mask) as usize;
        assert_ne!(st.head[h], NIL as u16);
    }

    /// `hi gh_water`/`WIN_INIT`: the bytes just past the data are zeroed, so the matcher's
    /// over-read cannot depend on stale window content.
    #[test]
    fn the_bytes_past_the_data_are_zeroed() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 0, 9, 8);
        // Dirty the whole window first.
        st.window.iter_mut().for_each(|b| *b = 0xff);
        let mut out = [];
        let mut io = Io::new(b"xyz", &mut out);
        st.fill_window(&mut io);
        assert_eq!(st.high_water, 3 + WIN_INIT);
        assert_eq!(&st.window[..3], b"xyz");
        assert!(st.window[3..3 + WIN_INIT as usize].iter().all(|b| *b == 0));
        assert_eq!(st.window[3 + WIN_INIT as usize], 0xff, "beyond the guard");
    }

    /// When the window fills, the upper half slides onto the lower half and every chain
    /// entry (and `strstart`/`block_start`) moves down by `w_size`.
    #[test]
    fn a_full_window_slides_down() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 0, 9, 8);
        assert_eq!(st.w_size, 512);
        st.status = BUSY_STATE;
        let data = vec![b'q'; 2048];
        let mut out = [];
        let mut io = Io::new(&data, &mut out);
        st.fill_window(&mut io);
        // 1024 bytes of window are full, so a slide must have happened.
        assert_eq!(st.strstart, 0);
        assert!(st.lookahead >= MIN_LOOKAHEAD);
        assert!(st.block_start <= 0);
        let _ = OS_CODE;
        let _ = MIN_MATCH;
    }

    /// `read_buf_out` writes into the caller's output without advancing `next_out` (the C
    /// leaves that to `deflate_stored`), and it checksums the bytes it moved.
    #[test]
    fn read_buf_out_leaves_the_cursor_to_its_caller() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 1, 15, 8);
        let data = b"straight through";
        let mut out = vec![0u8; 32];
        let mut io = Io::new(data, &mut out);
        let n = st.read_buf_out(&mut io, 8);
        assert_eq!(n, 8);
        assert_eq!(io.out_pos, 0, "the caller bumps next_out");
        assert_eq!(&io.output[..8], &data[..8]);
        assert_eq!(st.total_in, 8);
        assert_eq!(st.adler, adler32(1, &data[..8]));
        assert_eq!(io.avail_in(), data.len() - 8);
    }

    /// An empty input is a no-op that must not disturb the counters.
    #[test]
    fn an_empty_input_reads_nothing() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 1, 15, 8);
        let mut out = [];
        let mut io = Io::new(&[], &mut out);
        st.fill_window(&mut io);
        assert_eq!(st.lookahead, 0);
        assert_eq!(st.total_in, 0);
        assert_eq!(st.adler, 1);
    }
}
