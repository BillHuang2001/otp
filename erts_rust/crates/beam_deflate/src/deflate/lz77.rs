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
//! The LZ77 strategies — `longest_match` and the five `block_state` functions of
//! `../../../erts/emulator/zlib/deflate.c:1380-2186`.
//!
//! Each strategy is a loop that turns window bytes into `sym_buf` symbols and hands a
//! block to [`DeflateState::flush_block`] whenever `_tr_tally_*` says the symbol buffer
//! is full. The *only* thing that distinguishes them is how much search they pay for:
//!
//! | function | used by | matching |
//! |---|---|---|
//! | [`DeflateState::deflate_stored`] | level 0 | none — blocks are copied verbatim |
//! | [`DeflateState::deflate_fast`] | levels 1-3 | greedy, one `longest_match` per position |
//! | [`DeflateState::deflate_slow`] | levels 4-9 | lazy — a match is kept only if the next position cannot do better |
//! | [`DeflateState::deflate_rle`] | `Z_RLE` | distance-1 runs only, no hash table |
//! | [`DeflateState::deflate_huff`] | `Z_HUFFMAN_ONLY` | literals only, no hash table |
//!
//! The return value is the C's `block_state`: whether the caller must come back with
//! more output space ([`BlockState::NeedMore`]), the block is complete
//! ([`BlockState::BlockDone`]) or the stream is finished (`FinishStarted`/`FinishDone`).
//!
//! `longest_match`'s deliberate over-read (it compares up to `strstart + MAX_MATCH`
//! regardless of `lookahead`) is why `fill_window` zeroes `WIN_INIT` bytes past the data:
//! the over-read can only ever *lengthen* a match beyond `lookahead`, which the caller
//! then clamps.

use super::state::{
    DeflateState, StrategyFn, CONFIGURATION_TABLE, MAX_MATCH, MIN_LOOKAHEAD, MIN_MATCH, NIL,
    TOO_FAR, Z_FILTERED, Z_FINISH, Z_HUFFMAN_ONLY, Z_NO_FLUSH, Z_RLE,
};
use super::window::Io;

/// `MAX_STORED` — the largest deflate stored block, not counting its header.
const MAX_STORED: usize = 65535;

/// The C's `block_state` (`deflate.h:70-75`): what a strategy function tells the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockState {
    /// `need_more` — the output buffer is full; call again.
    NeedMore,
    /// `block_done` — a block boundary was reached and the flush is complete.
    BlockDone,
    /// `finish_started` — the final block was started but its output did not fit.
    FinishStarted,
    /// `finish_done` — the final block is complete.
    FinishDone,
}

/// `FLUSH_BLOCK(s, last)` (`deflate.c:1628`): emit the current block and, if the output
/// buffer is now full, return the C's early-exit state from the enclosing strategy
/// function.
macro_rules! flush_block {
    ($st:expr, $io:expr, $last:expr) => {{
        $st.flush_block_only($io, $last);
        if $io.avail_out() == 0 {
            return if $last {
                BlockState::FinishStarted
            } else {
                BlockState::NeedMore
            };
        }
    }};
}

impl DeflateState {
    /// `FLUSH_BLOCK_ONLY(s, last)` (`deflate.c:1617`): flush the block that ends at
    /// `strstart` into the pending buffer and move `block_start` up to it.
    ///
    /// `buf` is the window offset of the block's first byte, or `None` when
    /// `block_start` has gone negative (the C's NULL buffer) — in which case the stored
    /// form is unavailable and `_tr_flush_block` cannot pick it.
    pub(super) fn flush_block_only(&mut self, io: &mut Io, last: bool) {
        let buf = if self.block_start >= 0 {
            Some(self.block_start as usize)
        } else {
            None
        };
        let stored_len = (i64::from(self.strstart) - self.block_start) as u64;
        self.flush_block(buf, stored_len, last);
        self.block_start = i64::from(self.strstart);
        self.flush_pending(io);
    }

    /// `longest_match(s, cur_match)` (`deflate.c:1389`, the non-`UNALIGNED_OK` arm): walk
    /// the hash chain from `cur_match` and return the longest match length, setting
    /// `match_start`.
    ///
    /// The chain walk is bounded three ways: the chain length
    /// (`max_chain_length`, quartered once `prev_length >= good_match`), the
    /// `MAX_DIST` limit, and `nice_match` (stop as soon as a match is long enough).
    /// The result is clamped to `lookahead` at the end, which is what makes the
    /// deliberate over-read harmless — a match may be *found* beyond the available data
    /// (because those bytes are the guaranteed-zero guard region) but never *reported*
    /// that way.
    pub(super) fn longest_match(&mut self, cur_match: u32) -> u32 {
        let mut chain_length = self.max_chain_length;
        let mut nice_match = self.nice_match;
        let lookahead = self.lookahead;
        let best_floor = self.prev_length;
        let limit = if self.strstart > self.max_dist() {
            self.strstart - self.max_dist()
        } else {
            NIL
        };
        let wmask = self.w_mask as usize;
        let scan = self.strstart as usize;

        let mut best_len = best_floor as i32;
        let mut scan_end1 = self.window[scan + best_len as usize - 1];
        let mut scan_end = self.window[scan + best_len as usize];

        // Do not waste too much time if we already have a good match.
        if best_floor >= self.good_match {
            chain_length >>= 2;
        }
        // Do not look for matches beyond the end of the input. This is necessary to make
        // deflate deterministic.
        if nice_match as u32 > lookahead {
            nice_match = lookahead as i32;
        }

        let strend = scan + MAX_MATCH as usize;
        let mut match_start = self.match_start;
        let mut cur = cur_match;
        {
            let window = &self.window;
            let prev = &self.prev;
            loop {
                let m = cur as usize;
                // Skip to the next match if the match length cannot increase or if the
                // match length is less than 2.
                if window[m + best_len as usize] == scan_end
                    && window[m + best_len as usize - 1] == scan_end1
                    && window[m] == window[scan]
                    && window[m + 1] == window[scan + 1]
                {
                    // scan[2] and match[2] are always equal given equal hash keys, so the
                    // comparison starts at index 3. Lookahead is only checked every 8th
                    // comparison.
                    let mut s = scan + 2;
                    let mut mm = m + 2;
                    loop {
                        let mut mismatch = false;
                        for _ in 0..8 {
                            s += 1;
                            mm += 1;
                            if window[s] != window[mm] {
                                mismatch = true;
                                break;
                            }
                        }
                        if mismatch || s >= strend {
                            break;
                        }
                    }
                    let len = MAX_MATCH as i32 - (strend - s) as i32;
                    if len > best_len {
                        match_start = cur;
                        best_len = len;
                        if len >= nice_match {
                            break;
                        }
                        scan_end1 = window[scan + best_len as usize - 1];
                        scan_end = window[scan + best_len as usize];
                    }
                }
                cur = u32::from(prev[cur as usize & wmask]);
                if cur <= limit {
                    break;
                }
                // `--chain_length != 0`; the table's floors keep it above zero.
                chain_length = chain_length.saturating_sub(1);
                if chain_length == 0 {
                    break;
                }
            }
        }
        self.match_start = match_start;
        if best_len as u32 <= lookahead {
            best_len as u32
        } else {
            lookahead
        }
    }

    /// `deflate_stored` (`deflate.c:1668`): copy blocks through without compressing.
    ///
    /// This is the one strategy that writes to the caller's output buffer directly (the
    /// "single copy from `next_in` to `next_out`" the C's comment advertises), so it also
    /// owns the only place the *dummy* stored-block header is patched after the fact:
    /// `_tr_stored_block` is called with a zero length purely to emit the header bytes
    /// (including any pending bits), and `pending - 4..pending` is then overwritten with
    /// the real `LEN`/`~LEN`.
    ///
    /// `matches` doubles as the count of hash slides a later `deflateParams` switch to a
    /// compressing level owes (2 means "clear rather than slide").
    pub(super) fn deflate_stored(&mut self, io: &mut Io, flush: i32) -> BlockState {
        // Smallest worthy block size when not flushing or finishing. By default this is
        // 32K; as small as 507 bytes for memLevel == 1.
        let mut min_block = (self.pending_buf_size - 5).min(self.w_size as usize);
        let mut last = false;
        let mut used = io.avail_in();
        loop {
            // Maximum size block we can copy directly with the available input and output
            // space, and how much of that comes from what is left in the window.
            let mut len = MAX_STORED;
            let mut have = (self.bi_valid as usize + 42) >> 3;
            if io.avail_out() < have {
                break; // need room for the header
            }
            have = io.avail_out() - have;
            let mut left = (i64::from(self.strstart) - self.block_start) as usize;
            if len > left + io.avail_in() {
                len = left + io.avail_in();
            }
            if len > have {
                len = have;
            }
            // If the stored block would be less than min_block, or if we cannot copy all
            // of the available input while flushing, try the window and the pending buffer
            // instead. Also do not write an empty block when flushing — deflate() does.
            if len < min_block
                && ((len == 0 && flush != Z_FINISH)
                    || flush == Z_NO_FLUSH
                    || len != left + io.avail_in())
            {
                break;
            }
            // A dummy stored block in pending gets the header bytes (including any pending
            // bits); the lengths are overwritten below.
            last = flush == Z_FINISH && len == left + io.avail_in();
            self.stored_block(None, 0, last);
            let p = self.pending;
            self.pending_buf[p - 4] = len as u8;
            self.pending_buf[p - 3] = (len >> 8) as u8;
            self.pending_buf[p - 2] = !len as u8;
            self.pending_buf[p - 1] = (!len >> 8) as u8;
            self.flush_pending(io);
            // Copy uncompressed bytes from the window to next_out.
            if left != 0 {
                if left > len {
                    left = len;
                }
                let from = self.block_start as usize;
                let n = io.write_out(&self.window[from..from + left]);
                self.total_out += n as u64;
                self.block_start += n as i64;
                len -= n;
            }
            // Copy uncompressed bytes directly from next_in to next_out.
            if len != 0 {
                let n = self.read_buf_out(io, len);
                io.out_pos += n;
                self.total_out += n as u64;
            }
            if last {
                break;
            }
        }

        used -= io.avail_in(); // input bytes directly copied
        let w_size = self.w_size as usize;
        if used != 0 {
            // If any input was used then no unused input remains in the window, so
            // block_start == strstart.
            if used >= w_size {
                // Supplant the previous history.
                self.matches = 2; // clear hash
                let from = io.in_pos - w_size;
                self.window[..w_size].copy_from_slice(&io.input[from..from + w_size]);
                self.strstart = self.w_size;
                self.insert = self.strstart;
            } else {
                if self.window_size - i64::from(self.strstart) as u64 <= used as u64 {
                    // Slide the window down.
                    self.strstart -= self.w_size;
                    let n = self.strstart as usize;
                    self.window.copy_within(w_size..w_size + n, 0);
                    if self.matches < 2 {
                        self.matches += 1; // add a pending slide_hash()
                    }
                    if self.insert > self.strstart {
                        self.insert = self.strstart;
                    }
                }
                let from = io.in_pos - used;
                let at = self.strstart as usize;
                self.window[at..at + used].copy_from_slice(&io.input[from..from + used]);
                self.strstart += used as u32;
                let room = w_size.saturating_sub(self.insert as usize);
                self.insert += used.min(room) as u32;
            }
            self.block_start = i64::from(self.strstart);
        }
        if self.high_water < i64::from(self.strstart) as u64 {
            self.high_water = i64::from(self.strstart) as u64;
        }
        // If the last block was written to next_out, then done.
        if last {
            self.bi_used = 8;
            return BlockState::FinishDone;
        }
        // If flushing and all input has been consumed, then done.
        if flush != Z_NO_FLUSH
            && flush != Z_FINISH
            && io.avail_in() == 0
            && i64::from(self.strstart) == self.block_start
        {
            return BlockState::BlockDone;
        }

        // Fill the window with any remaining input.
        let mut have = self
            .window_size
            .saturating_sub(i64::from(self.strstart) as u64);
        if io.avail_in() as u64 > have && self.block_start >= self.w_size as i64 {
            // Slide the window down.
            self.block_start -= i64::from(self.w_size);
            self.strstart -= self.w_size;
            let n = self.strstart as usize;
            self.window.copy_within(w_size..w_size + n, 0);
            if self.matches < 2 {
                self.matches += 1; // add a pending slide_hash()
            }
            have += u64::from(self.w_size);
            if self.insert > self.strstart {
                self.insert = self.strstart;
            }
        }
        let have = have.min(io.avail_in() as u64) as usize;
        if have != 0 {
            let at = self.strstart as usize;
            self.read_buf_window(io, at, have);
            self.strstart += have as u32;
            let room = w_size.saturating_sub(self.insert as usize);
            self.insert += have.min(room) as u32;
        }
        if self.high_water < i64::from(self.strstart) as u64 {
            self.high_water = i64::from(self.strstart) as u64;
        }

        // If there was not enough avail_out for a worthy or flushed stored block, write
        // one to pending instead.
        let mut have = (self.bi_valid as usize + 42) >> 3;
        have = (self.pending_buf_size - have).min(MAX_STORED);
        min_block = have.min(w_size);
        let left = (i64::from(self.strstart) - self.block_start) as usize;
        if left >= min_block
            || ((left != 0 || flush == Z_FINISH) && flush != Z_NO_FLUSH && io.avail_in() == 0
                && left <= have)
        {
            let len = left.min(have);
            last = flush == Z_FINISH && io.avail_in() == 0 && len == left;
            let from = self.block_start as usize;
            self.stored_block(Some(from), len as u64, last);
            self.block_start += len as i64;
            self.flush_pending(io);
        }
        if last {
            self.bi_used = 8;
        }
        if last {
            BlockState::FinishStarted
        } else {
            BlockState::NeedMore
        }
    }

    /// `deflate_fast` (`deflate.c:1857`): greedy matching, one `longest_match` per
    /// position. Levels 1-3. Strings are re-inserted after a match only while the match
    /// is no longer than `max_lazy_match` (the C's `max_insert_length` alias).
    pub(super) fn deflate_fast(&mut self, io: &mut Io, flush: i32) -> BlockState {
        loop {
            // Make sure that we always have enough lookahead, except at the end of the
            // input file.
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window(io);
                if self.lookahead < MIN_LOOKAHEAD && flush == Z_NO_FLUSH {
                    return BlockState::NeedMore;
                }
                if self.lookahead == 0 {
                    break; // flush the current block
                }
            }

            // Insert window[strstart .. strstart+2] in the dictionary.
            let mut hash_head = NIL;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }

            // Find the longest match, discarding those <= prev_length. At this point we
            // always have match_length < MIN_MATCH.
            if hash_head != NIL && self.strstart - hash_head <= self.max_dist() {
                self.match_length = self.longest_match(hash_head);
            }

            if self.match_length >= MIN_MATCH {
                let bflush = self
                    .tr_tally_dist(self.strstart - self.match_start, self.match_length - MIN_MATCH);
                self.lookahead -= self.match_length;
                // Insert new strings in the hash table only if the match length is not too
                // large. This saves time but degrades compression.
                if self.match_length <= self.max_lazy_match && self.lookahead >= MIN_MATCH {
                    self.match_length -= 1; // string at strstart already in the table
                    loop {
                        self.strstart += 1;
                        let _ = self.insert_string(self.strstart);
                        self.match_length -= 1;
                        if self.match_length == 0 {
                            break;
                        }
                    }
                    self.strstart += 1;
                } else {
                    self.strstart += self.match_length;
                    self.match_length = 0;
                    self.ins_h = u32::from(self.window[self.strstart as usize]);
                    let c = self.window[self.strstart as usize + 1];
                    self.update_hash(c);
                }
                if bflush {
                    flush_block!(self, io, false);
                }
            } else {
                // No match, output a literal byte.
                let c = self.window[self.strstart as usize];
                let bflush = self.tr_tally_lit(c);
                self.lookahead -= 1;
                self.strstart += 1;
                if bflush {
                    flush_block!(self, io, false);
                }
            }
        }
        self.insert = if self.strstart < MIN_MATCH - 1 {
            self.strstart
        } else {
            MIN_MATCH - 1
        };
        if flush == Z_FINISH {
            flush_block!(self, io, true);
            return BlockState::FinishDone;
        }
        if self.sym_next != 0 {
            flush_block!(self, io, false);
        }
        BlockState::BlockDone
    }

    /// `deflate_slow` (`deflate.c:1955`): lazy matching. Levels 4-9 — the default at
    /// every level `zlib:compress/1` uses. A match found at `strstart` is only adopted
    /// once `strstart + 1` fails to beat it; otherwise the previous position emits a
    /// literal and the search moves on.
    ///
    /// The `avail_out == 0` check in the literal arm is load-bearing: the C emits the
    /// pending literal, *then* bumps `strstart`/`lookahead`, and only then gives up the
    /// machine — which is what makes a one-byte output buffer make progress instead of
    /// looping forever.
    pub(super) fn deflate_slow(&mut self, io: &mut Io, flush: i32) -> BlockState {
        loop {
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window(io);
                if self.lookahead < MIN_LOOKAHEAD && flush == Z_NO_FLUSH {
                    return BlockState::NeedMore;
                }
                if self.lookahead == 0 {
                    break; // flush the current block
                }
            }

            let mut hash_head = NIL;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }

            // Find the longest match, discarding those <= prev_length.
            self.prev_length = self.match_length;
            self.prev_match = self.match_start;
            self.match_length = MIN_MATCH - 1;
            if hash_head != NIL
                && self.prev_length < self.max_lazy_match
                && self.strstart - hash_head <= self.max_dist()
            {
                self.match_length = self.longest_match(hash_head);
                if self.match_length <= 5
                    && (self.strategy == Z_FILTERED
                        || (self.match_length == MIN_MATCH
                            && self.strstart - self.match_start > TOO_FAR))
                {
                    // If prev_match is also MIN_MATCH, match_start is garbage but we will
                    // ignore the current match anyway.
                    self.match_length = MIN_MATCH - 1;
                }
            }

            // If there was a match at the previous step and the current match is not
            // better, output the previous match.
            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                // Do not insert strings in the hash table beyond this.
                let max_insert = self.strstart + self.lookahead - MIN_MATCH;
                let bflush = self.tr_tally_dist(
                    self.strstart - 1 - self.prev_match,
                    self.prev_length - MIN_MATCH,
                );

                // Insert in the hash table all strings up to the end of the match.
                // strstart - 1 and strstart are already inserted. If there is not enough
                // lookahead, the last two strings are not inserted.
                self.lookahead -= self.prev_length - 1;
                self.prev_length -= 2;
                loop {
                    self.strstart += 1;
                    if self.strstart <= max_insert {
                        let _ = self.insert_string(self.strstart);
                    }
                    self.prev_length -= 1;
                    if self.prev_length == 0 {
                        break;
                    }
                }
                self.match_available = false;
                self.match_length = MIN_MATCH - 1;
                self.strstart += 1;
                if bflush {
                    flush_block!(self, io, false);
                }
            } else if self.match_available {
                // If there was no match at the previous position, output a single literal.
                // If there was a match but the current match is longer, truncate the
                // previous match to a single literal.
                let c = self.window[self.strstart as usize - 1];
                let bflush = self.tr_tally_lit(c);
                if bflush {
                    self.flush_block_only(io, false);
                }
                self.strstart += 1;
                self.lookahead -= 1;
                if io.avail_out() == 0 {
                    return BlockState::NeedMore;
                }
            } else {
                // There is no previous match to compare with; wait for the next step.
                self.match_available = true;
                self.strstart += 1;
                self.lookahead -= 1;
            }
        }
        debug_assert!(flush != Z_NO_FLUSH, "no flush?");
        if self.match_available {
            let c = self.window[self.strstart as usize - 1];
            let _ = self.tr_tally_lit(c);
            self.match_available = false;
        }
        self.insert = if self.strstart < MIN_MATCH - 1 {
            self.strstart
        } else {
            MIN_MATCH - 1
        };
        if flush == Z_FINISH {
            flush_block!(self, io, true);
            return BlockState::FinishDone;
        }
        if self.sym_next != 0 {
            flush_block!(self, io, false);
        }
        BlockState::BlockDone
    }

    /// `deflate_rle` (`deflate.c:2091`): distance-1 matches only, no hash table.
    ///
    /// The run scan starts at `strstart - 1` and compares forward with the 8-way unrolled
    /// loop `deflate.c` uses, so a run longer than `MAX_MATCH` is capped at `MAX_MATCH`
    /// (and then at `lookahead`).
    pub(super) fn deflate_rle(&mut self, io: &mut Io, flush: i32) -> BlockState {
        loop {
            // We need MAX_MATCH bytes for the longest run, plus one for the unrolled loop.
            if self.lookahead <= MAX_MATCH {
                self.fill_window(io);
                if self.lookahead <= MAX_MATCH && flush == Z_NO_FLUSH {
                    return BlockState::NeedMore;
                }
                if self.lookahead == 0 {
                    break; // flush the current block
                }
            }

            // See how many times the previous byte repeats.
            self.match_length = 0;
            if self.lookahead >= MIN_MATCH && self.strstart > 0 {
                let mut scan = self.strstart as usize - 1;
                let prev = self.window[scan];
                let mut ok = true;
                for _ in 0..3 {
                    scan += 1;
                    if prev != self.window[scan] {
                        ok = false;
                        break;
                    }
                }
                if ok {
                    let strend = self.strstart as usize + MAX_MATCH as usize;
                    loop {
                        let mut mismatch = false;
                        for _ in 0..8 {
                            scan += 1;
                            if prev != self.window[scan] {
                                mismatch = true;
                                break;
                            }
                        }
                        if mismatch || scan >= strend {
                            break;
                        }
                    }
                    self.match_length = MAX_MATCH - (strend - scan) as u32;
                    if self.match_length > self.lookahead {
                        self.match_length = self.lookahead;
                    }
                }
            }

            // Emit a match if we have a run of MIN_MATCH or longer, else a literal.
            if self.match_length >= MIN_MATCH {
                let bflush = self.tr_tally_dist(1, self.match_length - MIN_MATCH);
                self.lookahead -= self.match_length;
                self.strstart += self.match_length;
                self.match_length = 0;
                if bflush {
                    flush_block!(self, io, false);
                }
            } else {
                let c = self.window[self.strstart as usize];
                let bflush = self.tr_tally_lit(c);
                self.lookahead -= 1;
                self.strstart += 1;
                if bflush {
                    flush_block!(self, io, false);
                }
            }
        }
        self.insert = 0;
        if flush == Z_FINISH {
            flush_block!(self, io, true);
            return BlockState::FinishDone;
        }
        if self.sym_next != 0 {
            flush_block!(self, io, false);
        }
        BlockState::BlockDone
    }

    /// `deflate_huff` (`deflate.c:2145`): literals only, no matching, no hash table.
    pub(super) fn deflate_huff(&mut self, io: &mut Io, flush: i32) -> BlockState {
        loop {
            // Make sure that we have a literal to write.
            if self.lookahead == 0 {
                self.fill_window(io);
                if self.lookahead == 0 {
                    if flush == Z_NO_FLUSH {
                        return BlockState::NeedMore;
                    }
                    break; // flush the current block
                }
            }
            // Output a literal byte.
            self.match_length = 0;
            let c = self.window[self.strstart as usize];
            let bflush = self.tr_tally_lit(c);
            self.lookahead -= 1;
            self.strstart += 1;
            if bflush {
                flush_block!(self, io, false);
            }
        }
        self.insert = 0;
        if flush == Z_FINISH {
            flush_block!(self, io, true);
            return BlockState::FinishDone;
        }
        if self.sym_next != 0 {
            flush_block!(self, io, false);
        }
        BlockState::BlockDone
    }

    /// The C's strategy dispatch (`deflate.c:1224`): `level == 0` beats every strategy,
    /// then `Z_HUFFMAN_ONLY`/`Z_RLE` override the level, and only then does
    /// `configuration_table[level].func` decide.
    pub(super) fn run_strategy(&mut self, io: &mut Io, flush: i32) -> BlockState {
        if self.level == 0 {
            return self.deflate_stored(io, flush);
        }
        if self.strategy == Z_HUFFMAN_ONLY {
            return self.deflate_huff(io, flush);
        }
        if self.strategy == Z_RLE {
            return self.deflate_rle(io, flush);
        }
        match CONFIGURATION_TABLE[self.level as usize].func {
            StrategyFn::Stored => self.deflate_stored(io, flush),
            StrategyFn::Fast => self.deflate_fast(io, flush),
            StrategyFn::Slow => self.deflate_slow(io, flush),
        }
    }
}
