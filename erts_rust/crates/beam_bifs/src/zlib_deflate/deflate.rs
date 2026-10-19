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

//! The LZ77 half of the encoder — the port of `../../erts/emulator/zlib/deflate.c`.
//!
//! Three pieces, in the C's own order:
//!
//! * [`DeflateState::longest_match`] — the hash-chain walk that finds the best match at
//!   the current position. The non-`UNALIGNED_OK`, non-`FASTEST` variant is the one a
//!   plain zlib build compiles (the other two are `-D` experiments with identical
//!   output, only different compare widths), and its eight-comparisons-per-lookahead-
//!   check loop is reproduced literally: the *number* of bytes examined is part of the
//!   heuristic, so a shorter-circuiting rewrite would change which match wins ties.
//! * [`DeflateState::deflate_slow`] — `deflate_slow`, the level-6 strategy: keep one
//!   match pending and emit it lazily, only once the *next* position fails to produce a
//!   longer one (`prev_length >= MIN_MATCH && match_length <= prev_length`).
//! * [`super::compress`] — the `deflate(Z_FINISH)` call: the zlib header, the body, the
//!   Adler-32 trailer.
//!
//! Two C constructs have no Rust counterpart and are handled explicitly:
//!
//! * The `flush == Z_NO_FLUSH` early returns in `deflate_slow` (and the `avail_out == 0`
//!   returns inside `FLUSH_BLOCK`) exist only for a streaming caller. This entry point is
//!   a one-shot `Z_FINISH` over the whole body, so they cannot be reached; they are
//!   omitted rather than faked, and the one-shot equivalence is the property the corpus
//!   in `tests/zlib_oracle.rs` checks.
//! * `s->strstart - hash_head` and `s->strstart - 1 - s->prev_match` are `uInt`
//!   subtractions that the C lets wrap. They are `wrapping_sub` here for exactly that
//!   reason: a wrapping value then fails the distance test the way the C's does, instead
//!   of panicking in a debug build.

use super::state::{
    DeflateState, MAX_DIST, MAX_MATCH, MIN_LOOKAHEAD, MIN_MATCH, NIL, TOO_FAR, W_BITS, W_MASK,
};
use crate::checksum::adler32_step;

/// `Z_DEFLATED` (`zlib.h:200`) — the only compression method zlib emits.
const Z_DEFLATED: u32 = 8;

/// `level_flags` for level 6 (`deflate.c:894-901`): `2` selects the "default" bucket, and
/// is part of the two-byte zlib header (`0x78 0x9C`), not of the compressed body.
const LEVEL_FLAGS: u32 = 2;

impl<'a> DeflateState<'a> {
    /// `longest_match` (`deflate.c:1290-1400`) — the best match length at `strstart`
    /// among the strings on `cur_match`'s hash chain, clamped to the lookahead.
    ///
    /// Also sets [`DeflateState::match_start`] whenever it improves on
    /// [`DeflateState::prev_length`], which is what the caller records as the pending
    /// match's position.
    fn longest_match(&mut self, cur_match: u32) -> u32 {
        let mut chain_length = self.max_chain_length;
        let strstart = self.strstart as usize;
        // `scan` is `window + strstart`. After a successful compare it is reset to
        // `strend - MAX_MATCH`, i.e. back to `strstart` — kept as an index so the resets
        // and the `scan_end*` refreshes below read as the C does.
        let mut scan = strstart;
        let mut best_len = self.prev_length as i32;
        let mut nice_match = self.nice_match;
        let limit = if self.strstart > MAX_DIST {
            self.strstart - MAX_DIST
        } else {
            NIL
        };
        // `strend = window + strstart + MAX_MATCH` (the non-`UNALIGNED_OK` arm).
        let strend = strstart + MAX_MATCH as usize;
        let mut scan_end1 = self.window[scan + best_len as usize - 1];
        let mut scan_end = self.window[scan + best_len as usize];

        // "Do not waste too much time if we already have a good match."
        if self.prev_length >= self.good_match {
            chain_length >>= 2;
        }
        // "Do not look for matches beyond the end of the input. This is necessary to make
        // deflate deterministic."
        if nice_match as u32 > self.lookahead {
            nice_match = self.lookahead as i32;
        }

        let mut cur = cur_match as usize;
        loop {
            // "Skip to next match if the match length cannot increase or if the match
            // length is less than 2."
            if self.window[cur + best_len as usize] == scan_end
                && self.window[cur + best_len as usize - 1] == scan_end1
                && self.window[cur] == self.window[scan]
                && self.window[cur + 1] == self.window[scan + 1]
            {
                // `scan += 2, match++`: byte 2 is known equal by hash equality, so the
                // compares start at offset 3. Insufficient-lookahead is checked only
                // every eighth comparison; `strend` is exactly 256 bytes past the start,
                // so the loop always lands on it rather than past it.
                let mut m = cur + 2;
                let mut s = scan + 2;
                loop {
                    let mut equal = true;
                    for _ in 0..8 {
                        s += 1;
                        m += 1;
                        if self.window[s] != self.window[m] {
                            equal = false;
                            break;
                        }
                    }
                    if !equal || s >= strend {
                        break;
                    }
                }
                let len = (s - strstart) as i32;
                // `scan = strend - MAX_MATCH`.
                scan = strend - MAX_MATCH as usize;
                if len > best_len {
                    self.match_start = cur as u32;
                    best_len = len;
                    if len >= nice_match {
                        break;
                    }
                    scan_end1 = self.window[scan + best_len as usize - 1];
                    scan_end = self.window[scan + best_len as usize];
                }
            }
            // `while ((cur_match = prev[cur_match & wmask]) > limit && --chain_length != 0)`
            cur = self.prev[(cur as u32 & W_MASK) as usize] as usize;
            if (cur as u32) <= limit {
                break;
            }
            chain_length = chain_length.wrapping_sub(1);
            if chain_length == 0 {
                break;
            }
        }

        if (best_len as u32) <= self.lookahead {
            best_len as u32
        } else {
            self.lookahead
        }
    }

    /// `_tr_flush_block` for the current block, then start a new one — the body of the C's
    /// `FLUSH_BLOCK_ONLY` macro (`deflate.c:437-444`).
    ///
    /// `block_start` is the signed window offset of the block's first byte; a negative
    /// value is the C's `Z_NULL` buffer (the window has slid past that byte), which
    /// disables the stored form for the block.
    fn flush_block_only(&mut self, last: bool) {
        let buf = if self.block_start >= 0 {
            Some(self.block_start as usize)
        } else {
            None
        };
        let stored_len = (self.strstart as i64 - self.block_start) as u64;
        self.flush_block(buf, stored_len, last);
        self.block_start = i64::from(self.strstart);
    }

    /// `deflate_slow` (`deflate.c:1830-1930`) for a `Z_FINISH` call over the whole input.
    ///
    /// The strategy is lazy matching: the match found at the previous position is held in
    /// `prev_length`/`prev_match`, and is emitted only when the current position cannot do
    /// better. `match_available` covers the third case — a literal is pending and the
    /// encoder has not decided yet whether it starts a match.
    fn deflate_slow(&mut self) {
        loop {
            // "Make sure that we always have enough lookahead, except at the end of the
            // input file."
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window();
                // The C's `lookahead < MIN_LOOKAHEAD && flush == Z_NO_FLUSH` return is
                // unreachable here (this is the `Z_FINISH` call).
                if self.lookahead == 0 {
                    break;
                }
            }

            // "Insert the string window[strstart .. strstart + 2] in the dictionary, and
            // set hash_head to the head of the hash chain."
            let mut hash_head = NIL;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }

            // "Find the longest match, discarding those <= prev_length."
            self.prev_length = self.match_length;
            self.prev_match = self.match_start;
            self.match_length = MIN_MATCH - 1;

            if hash_head != NIL
                && self.prev_length < self.max_lazy_match
                && self.strstart.wrapping_sub(hash_head) <= MAX_DIST
            {
                self.match_length = self.longest_match(hash_head);
                // A length-3 match too far away is not worth its distance code; the
                // default `Z_DEFAULT_STRATEGY` is assumed, so only the `TOO_FAR` arm of
                // the C's `Z_FILTERED || …` condition can fire.
                if self.match_length <= 5
                    && self.match_length == MIN_MATCH
                    && self.strstart.wrapping_sub(self.match_start) > TOO_FAR
                {
                    self.match_length = MIN_MATCH - 1;
                }
            }

            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                // "If there was a match at the previous step and the current match is not
                // better, output the previous match."
                let max_insert = self.strstart + self.lookahead - MIN_MATCH;
                let bflush = self.tr_tally_dist(
                    self.strstart.wrapping_sub(1).wrapping_sub(self.prev_match),
                    self.prev_length - MIN_MATCH,
                );

                // "Insert in hash table all strings up to the end of the match": the two
                // ends are already inserted, so only the interior is (and only while there
                // is enough lookahead left).
                self.lookahead -= self.prev_length - 1;
                self.prev_length -= 2;
                loop {
                    self.strstart += 1;
                    if self.strstart <= max_insert {
                        self.insert_string(self.strstart);
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
                    self.flush_block_only(false);
                }
            } else if self.match_available {
                // "If there was no match at the previous position, output a single
                // literal. If there was a match but the current match is longer, truncate
                // the previous match to a single literal."
                let bflush = self.tr_tally_lit(self.window[(self.strstart - 1) as usize]);
                if bflush {
                    self.flush_block_only(false);
                }
                self.strstart += 1;
                self.lookahead -= 1;
            } else {
                // "There is no previous match to compare with, wait for the next step to
                // decide."
                self.match_available = true;
                self.strstart += 1;
                self.lookahead -= 1;
            }
        }

        // "Assert (flush != Z_NO_FLUSH, 'no flush?')" — the trailing pending literal.
        if self.match_available {
            self.tr_tally_lit(self.window[(self.strstart - 1) as usize]);
            self.match_available = false;
        }
        // "Do not insert strings in hash table beyond this."
        self.insert = if self.strstart < MIN_MATCH - 1 {
            self.strstart
        } else {
            MIN_MATCH - 1
        };
        // The `Z_FINISH` arm: the last block is flushed with the BFINAL bit set, which is
        // what closes the stream.
        self.flush_block_only(true);
    }
}

/// `deflate(Z_FINISH)` at level 6 — the zlib-wrapped stream `zlib:compress/1` returns.
///
/// The framing is `deflate.c:886-905` (the header) plus `:1090-1113` (the trailer): the
/// header's `CMF` carries the method and the 32 KiB window (`0x78`), its `FLG` carries
/// [`LEVEL_FLAGS`] and the check value that makes `CMF << 8 | FLG` a multiple of 31
/// (`0x9C`), and the trailer is the Adler-32 of the whole body, big-endian.
///
/// The level-6 configuration row is the only one this port implements, because it is the
/// only one the six `zlib` convenience functions request
/// (`deflateInit(Z, Z_DEFAULT_COMPRESSION)`).
pub(super) fn compress(input: &[u8]) -> Vec<u8> {
    let mut st = DeflateState::new(input);

    // `uInt header = (Z_DEFLATED + ((s->w_bits - 8) << 4)) << 8;` then the level flags,
    // then the check value. `strstart == 0`, so the preset-dictionary bit stays clear.
    let mut header = (Z_DEFLATED + ((W_BITS - 8) << 4)) << 8;
    header |= LEVEL_FLAGS << 6;
    header += 31 - (header % 31);
    // `putShortMSB` writes the high byte first.
    st.out.push((header >> 8) as u8);
    st.out.push(header as u8);

    st.deflate_slow();

    // `putShortMSB(adler >> 16); putShortMSB(adler & 0xffff)` — the whole 32-bit Adler-32,
    // big-endian. Initial value 1 is zlib's `adler32(0, Z_NULL, 0)`.
    let adler = adler32_step(1, input);
    st.out.push((adler >> 24) as u8);
    st.out.push((adler >> 16) as u8);
    st.out.push((adler >> 8) as u8);
    st.out.push(adler as u8);

    st.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zlib_deflate::tables::STATIC_LTREE;

    /// The empty body is the smallest complete stream: header, one final static block with
    /// only `END_BLOCK`, trailer. This is also the golden the crate's `zlib` suite pins.
    #[test]
    fn the_empty_body_is_the_zlib_stream_of_a_single_empty_final_block() {
        assert_eq!(compress(b""), vec![0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]);
    }

    /// `<<"abc">>` fits in one static block whose three literals have their own 8-bit
    /// codes; the bytes are the oracle's (`zlib:compress(<<"abc">>)`), so this covers the
    /// literal-only path, the `END_BLOCK` code and the `bi_windup` padding in one golden.
    #[test]
    fn a_three_byte_literal_run_matches_the_oracle_bytes() {
        assert_eq!(
            compress(b"abc"),
            vec![0x78, 0x9c, 0x4b, 0x4c, 0x4a, 0x06, 0x00, 0x02, 0x4d, 0x01, 0x27]
        );
    }

    /// Sixteen zero bytes: a match at distance 1, i.e. the length/distance trees instead
    /// of literals. Also the oracle's bytes, and the smallest input with a real match.
    #[test]
    fn a_sixteen_byte_run_emits_a_distance_code_like_the_oracle() {
        assert_eq!(
            compress(&[0u8; 16]),
            vec![0x78, 0x9c, 0x63, 0x60, 0x40, 0x05, 0x00, 0x00, 0x10, 0x00, 0x01]
        );
    }
    /// `longest_match` never reports a match longer than `MAX_MATCH` and records the
    /// position it found — the invariant the caller's lazy logic relies on. A body of 300
    /// `'a'`s is a run at distance 1, so the walk over-reads its `MAX_MATCH` window and
    /// stops there, well before the end of the run.
    #[test]
    fn a_long_run_stops_the_match_at_max_match() {
        let body = vec![b'a'; 300];
        let mut st = DeflateState::new(&body);
        st.fill_window();
        assert_eq!(st.lookahead, 300);
        let head = st.insert_string(0);
        assert_eq!(head, NIL);
        let head = st.insert_string(1);
        assert_eq!(head, 0);
        st.prev_length = MIN_MATCH - 1;
        let len = st.longest_match(head);
        assert_eq!(len, MAX_MATCH);
        assert_eq!(st.match_start, 0);
    }

    /// The return is clamped to the lookahead even when the window past the current data
    /// holds bytes that lengthen the match — which is exactly why `fill_window` zeroes
    /// those bytes (`WIN_INIT`): the C reads them and then clamps rather than refusing to
    /// look.
    #[test]
    fn a_match_reaching_past_the_data_is_clamped_to_the_lookahead() {
        let body = vec![b'a'; 20];
        let mut st = DeflateState::new(&body);
        st.fill_window();
        assert_eq!(st.lookahead, 20);
        st.window[20..20 + MAX_MATCH as usize].fill(b'a');

        let head = st.insert_string(0);
        assert_eq!(head, NIL);
        let head = st.insert_string(1);
        assert_eq!(head, 0);
        st.prev_length = MIN_MATCH - 1;
        let len = st.longest_match(head);
        assert_eq!(len, 20, "clamped to the lookahead, not the raw match length");
    }

    /// The Huffman tables are shared with `trees.rs`; a literal code must stay inside its
    /// 8-bit static width so `compress_block`'s fixed-tree arm can emit it.
    #[test]
    fn the_static_literal_codes_are_eight_bits_wide() {
        for (code, len) in STATIC_LTREE.iter().take(144) {
            assert_eq!(*len, 8);
            assert!(*code < 1 << 8);
        }
    }
}
