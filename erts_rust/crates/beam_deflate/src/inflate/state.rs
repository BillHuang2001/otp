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
//! The inflate stream state (`inflate.h`'s `struct inflate_state`) and its resets.
//!
//! Every field the C keeps between `inflate()` calls is here, with one interior
//! difference: the code tables are three separate arrays (`codes[ENOUGH]` in the C, with
//! `lencode`/`distcode`/`next` as pointers into it) and the sliding window is stored
//! **mirrored** rather than as a bare ring ([`super::window`] explains why and proves
//! the two are observationally identical).
//!
//! Fields the C keeps but this port does not need are called out where they would be:
//! `head` (`inflateGetHeader` is not part of the API — the gzip header is still parsed
//! and verified, just not reported), `sane` (it is only ever cleared by
//! `inflateUndermine` under a build-time option that this port does not have, so an
//! over-long distance is always a data error), and `back`/`was` (they only feed
//! `inflateMark`, which is introspection the crate does not expose).

use beam_common::{BeamError, Result};

use crate::inflate::inffixed::{DISTFIX, LENFIX};
use crate::inflate::inftrees::{
    Code, ENOUGH_CODES, ENOUGH_DISTS, ENOUGH_LENS, INVALID,
};

/// The C's `inflate_mode`, in the C's own order.
///
/// The order is **load-bearing**: `inflate()` compares modes (`state->mode < BAD`,
/// `state->mode < CHECK`) when deciding whether to refresh the sliding window, so the
/// variants must stay in this sequence. `COPY_` and `LEN_` are omitted — they exist
/// only to give `Z_TREES` a stopping point before `COPY`/`LEN`, and [`crate::Flush`]
/// has no `Z_TREES` counterpart (dropping them leaves every other relative order
/// intact).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Mode {
    /// Waiting for the magic header.
    Head,
    /// Waiting for the gzip method and flags.
    Flags,
    /// Waiting for the gzip modification time.
    Time,
    /// Waiting for the gzip extra flags and OS.
    Os,
    /// Waiting for the gzip extra length.
    Exlen,
    /// Waiting for the gzip extra bytes.
    Extra,
    /// Waiting for the end of the gzip file name.
    Name,
    /// Waiting for the end of the gzip comment.
    Comment,
    /// Waiting for the gzip header CRC.
    Hcrc,
    /// Waiting for the dictionary check value.
    DictId,
    /// Waiting for an `inflateSetDictionary` call.
    Dict,
    /// Waiting for the block-type bits.
    Type,
    /// The same, but past the `Z_BLOCK` check.
    TypedO,
    /// Waiting for a stored block's length and complement.
    Stored,
    /// Waiting for input or output space to copy a stored block.
    Copy,
    /// Waiting for a dynamic block's table lengths.
    Table,
    /// Waiting for the code-length code lengths.
    LenLens,
    /// Waiting for the literal/length and distance code lengths.
    CodeLens,
    /// Waiting for a literal/length end-of-block code.
    Len,
    /// Waiting for a length's extra bits.
    LenExt,
    /// Waiting for a distance code.
    Dist,
    /// Waiting for a distance's extra bits.
    DistExt,
    /// Waiting for output space to copy a match.
    Match,
    /// Waiting for output space to write a literal.
    Lit,
    /// Waiting for the 32-bit check value.
    Check,
    /// Waiting for the gzip 32-bit length.
    Length,
    /// Finished: the stream is complete.
    Done,
    /// A data error — stay here until reset.
    Bad,
    /// A memory error — stay here until reset (unreachable: a Rust allocation cannot
    /// fail recoverably, and the window grows on demand).
    Mem,
    /// Looking for `Z_SYNC_FLUSH` marker bytes — the C's `SYNC`, which only
    /// `inflateSync` (not part of this crate's API) ever enters. Kept so the state set
    /// matches the C's and so the pump's answer to it (`Z_STREAM_ERROR`) is the C's.
    #[allow(dead_code)]
    Sync,
}

/// `state->lens[]` size — 286 literal/length + 30 distance lengths, plus the 19
/// code-length slots that share the array (the C: `unsigned short lens[320]`).
pub(crate) const LENS_CAP: usize = 320;

/// `state->work[]` size (`unsigned short work[288]`).
pub(crate) const WORK_CAP: usize = 288;

/// Everything the C keeps in `struct inflate_state`.
pub(crate) struct InflateState {
    /// The current point of the state machine.
    pub mode: Mode,
    /// True once the last block's header has been read.
    pub last: bool,
    /// Bit 0: zlib framing allowed, bit 1: gzip framing allowed, bit 2: verify the
    /// trailer checksum.
    pub wrap: u8,
    /// True once a preset dictionary has been installed.
    pub havedict: bool,
    /// The gzip header's method/flags word, `0` for a zlib header, `-1` before one.
    pub flags: i32,
    /// The maximum distance a zlib header permits (`INFLATE_STRICT` bookkeeping).
    pub dmax: u32,
    /// The running Adler-32 (zlib) or CRC-32 (gzip) of the output — `strm->adler`.
    pub check: u32,
    /// The output count, protected across the `CHECK` mode's counter adjustment.
    pub total: u64,
    /// Log2 of the window size requested (or read from a zlib/gzip header).
    pub wbits: u32,
    /// The window size, or 0 while no window is in use.
    pub wsize: u32,
    /// Valid bytes in the window.
    pub whave: u32,
    /// The window write index.
    pub wnext: u32,
    /// The sliding window, stored mirrored: `window[i] == window[i + wsize]`, so the
    /// last `whave` bytes are always the contiguous slice `[wnext .. wnext + whave]`.
    pub window: Vec<u8>,
    /// The input bit accumulator.
    pub hold: u64,
    /// How many bits `hold` carries.
    pub bits: u32,
    /// The literal, or the length of the match being copied.
    pub length: u32,
    /// The distance of the match being copied.
    pub offset: u32,
    /// Extra bits still to read for the current code.
    pub extra: u32,
    /// True while `lencode` is the fixed table (`inffixed::LENFIX`).
    pub lencode_fixed: bool,
    /// True while `distcode` is the fixed table (`inffixed::DISTFIX`).
    pub distcode_fixed: bool,
    /// Index bits for the literal/length table.
    pub lenbits: u32,
    /// Index bits for the distance table.
    pub distbits: u32,
    /// Index bits for the code-length table (`CODELENS`).
    pub code_bits: u32,
    /// The literal/length table.
    pub lens_table: [Code; ENOUGH_LENS],
    /// The distance table.
    pub dist_table: [Code; ENOUGH_DISTS],
    /// The code-length table.
    pub codetable: [Code; ENOUGH_CODES],
    /// Temporary storage for code lengths.
    pub lens: [u16; LENS_CAP],
    /// The table builder's work area.
    pub work: [u16; WORK_CAP],
    /// How many code lengths have been read so far.
    pub have: u32,
    /// Length of the code-length alphabet (`HLIT + 4`).
    pub ncode: u32,
    /// Length of the literal/length alphabet (`HLIT + 257`).
    pub nlen: u32,
    /// Length of the distance alphabet (`HDIST + 1`).
    pub ndist: u32,
}

impl InflateState {
    /// A fresh state, before any `inflateReset2`.
    pub(crate) fn new() -> InflateState {
        InflateState {
            mode: Mode::Head,
            last: false,
            wrap: 0,
            havedict: false,
            flags: -1,
            dmax: 32768,
            check: 0,
            total: 0,
            wbits: 0,
            wsize: 0,
            whave: 0,
            wnext: 0,
            window: Vec::new(),
            hold: 0,
            bits: 0,
            length: 0,
            offset: 0,
            extra: 0,
            lencode_fixed: false,
            distcode_fixed: false,
            lenbits: 0,
            distbits: 0,
            code_bits: 0,
            lens_table: [Code::default(); ENOUGH_LENS],
            dist_table: [Code::default(); ENOUGH_DISTS],
            codetable: [Code::default(); ENOUGH_CODES],
            lens: [0; LENS_CAP],
            work: [0; WORK_CAP],
            have: 0,
            ncode: 0,
            nlen: 0,
            ndist: 0,
        }
    }

    /// `inflateResetKeep` — reset everything but the window bits.
    ///
    /// The window buffer itself is kept (an `inflateReset` reuses it); only its
    /// bookkeeping is cleared, by [`InflateState::reset`].
    pub(crate) fn reset_keep(&mut self) {
        self.total = 0;
        if self.wrap != 0 {
            // The C mirrors this into `strm->adler`; there is no separate `adler` field
            // here, and `check` is not read before HEAD writes it.
            self.check = u32::from(self.wrap & 1);
        }
        self.mode = Mode::Head;
        self.last = false;
        self.havedict = false;
        self.flags = -1;
        self.dmax = 32768;
        self.hold = 0;
        self.bits = 0;
        self.lencode_fixed = false;
        self.distcode_fixed = false;
    }

    /// `inflateReset` — reset and forget the window contents.
    pub(crate) fn reset(&mut self) {
        self.wsize = 0;
        self.whave = 0;
        self.wnext = 0;
        self.reset_keep();
    }

    /// `inflateReset2` — reset onto a new `windowBits`.
    ///
    /// The accepted set is the C's, including its quirks: `-15..=-8` is raw RFC 1951;
    /// `0..=15` is zlib (0 means "take the window size from the header"); `16` and
    /// `24..=31` are gzip (`16` meaning "the gzip default of 15"); `32` and `40..=47`
    /// auto-detect; `-7..=-1`, `17..=23`, `33..=39` and everything from `48` up are
    /// `Z_STREAM_ERROR`, which is [`BeamError::BadArg`] here.
    pub(crate) fn reset_with_window_bits(&mut self, window_bits: i8) -> Result<()> {
        let mut bits = i32::from(window_bits);
        let wrap;
        if bits < 0 {
            if bits < -15 {
                return Err(BeamError::BadArg);
            }
            wrap = 0;
            bits = -bits;
        } else {
            wrap = ((bits >> 4) + 5) as u8;
            if bits < 48 {
                bits &= 15;
            }
        }
        if bits != 0 && !(8..=15).contains(&bits) {
            return Err(BeamError::BadArg);
        }
        if !self.window.is_empty() && self.wbits != bits as u32 {
            self.window = Vec::new();
        }
        self.wrap = wrap;
        self.wbits = bits as u32;
        self.reset();
        Ok(())
    }

    /// Switch the literal/length and distance tables to the fixed codes
    /// (`inflate_fixed`).
    pub(crate) fn use_fixed_tables(&mut self) {
        self.lencode_fixed = true;
        self.lenbits = 9;
        self.distcode_fixed = true;
        self.distbits = 5;
    }

    /// The literal/length table entry at `index`.
    ///
    /// An out-of-range index cannot be produced by the table builder, and the decode
    /// sites only index with `lenbits`/`distbits`-wide bit fields; should one ever
    /// occur, the invalid-code marker turns it into a `Z_DATA_ERROR` instead of a
    /// panic or a wrong byte.
    pub(crate) fn lcode(&self, index: usize) -> Code {
        let entry = if self.lencode_fixed {
            LENFIX.get(index)
        } else {
            self.lens_table.get(index)
        };
        debug_assert!(entry.is_some(), "literal/length table index {index} out of range");
        entry.copied().unwrap_or(INVALID)
    }

    /// The distance table entry at `index`.
    pub(crate) fn dcode(&self, index: usize) -> Code {
        let entry = if self.distcode_fixed {
            DISTFIX.get(index)
        } else {
            self.dist_table.get(index)
        };
        debug_assert!(entry.is_some(), "distance table index {index} out of range");
        entry.copied().unwrap_or(INVALID)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The accept/reject set of `inflateReset2`, exhaustively over `i8`.
    ///
    /// This is the C's arithmetic verbatim: negative values negate into the window
    /// bits, and non-negative ones take `wrap = (bits >> 4) + 5` with the low nibble as
    /// the window size when below 48.
    #[test]
    fn the_window_bits_acceptance_set_is_zlibs() {
        let accepted = |bits: i8| InflateState::new().reset_with_window_bits(bits).is_ok();
        for bits in -15..=-8 {
            assert!(accepted(bits), "raw {bits}");
        }
        for bits in -7..=-1 {
            assert!(!accepted(bits), "raw {bits} is below the 8-bit window");
        }
        assert!(!accepted(-16), "raw -16");
        assert!(!accepted(i8::MIN), "raw i8::MIN");
        assert!(accepted(0), "zlib, window from the header");
        for bits in 1..=7 {
            assert!(!accepted(bits), "zlib {bits} is below the 8-bit window");
        }
        for bits in 8..=15 {
            assert!(accepted(bits), "zlib {bits}");
        }
        assert!(accepted(16), "gzip with the default window");
        for bits in 17..=23 {
            assert!(!accepted(bits), "gzip {bits} has a sub-8-bit window");
        }
        for bits in 24..=31 {
            assert!(accepted(bits), "gzip {bits}");
        }
        assert!(accepted(32), "auto-detect with the header's window");
        for bits in 33..=39 {
            assert!(!accepted(bits), "auto {bits} has a sub-8-bit window");
        }
        for bits in 40..=47 {
            assert!(accepted(bits), "auto {bits}");
        }
        for bits in 48..=127 {
            assert!(!accepted(bits), "{bits} is past the auto-detect range");
        }
    }

    /// `wrap`, `wbits` and the window-size quirk for the accepted forms.
    #[test]
    fn the_wrap_bits_follow_the_window_bits() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        assert_eq!((st.wrap, st.wbits), (5, 15));
        st.reset_with_window_bits(-15).expect("raw 15");
        assert_eq!((st.wrap, st.wbits), (0, 15));
        st.reset_with_window_bits(31).expect("gzip 15");
        assert_eq!((st.wrap, st.wbits), (6, 15));
        st.reset_with_window_bits(16).expect("gzip default");
        assert_eq!((st.wrap, st.wbits), (6, 0), "0 means 'from the header'");
        st.reset_with_window_bits(47).expect("auto 15");
        assert_eq!((st.wrap, st.wbits), (7, 15));
        st.reset_with_window_bits(0).expect("zlib, header window");
        assert_eq!((st.wrap, st.wbits), (5, 0));
    }

    /// A reset clears the stream state but keeps the window bits, and a
    /// `reset_with_window_bits` that changes them drops the window buffer.
    #[test]
    fn resets_clear_state_and_keep_or_drop_the_window() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        st.window = vec![7u8; 2 * 32768];
        st.wsize = 32768;
        st.whave = 100;
        st.wnext = 100;
        st.last = true;
        st.hold = 0x1234;
        st.bits = 13;
        st.flags = 0x0800;
        st.total = 99;

        st.reset();
        assert_eq!(st.mode, Mode::Head);
        assert!(!st.last);
        assert!(!st.havedict);
        assert_eq!(st.flags, -1);
        assert_eq!(st.dmax, 32768);
        assert_eq!((st.hold, st.bits), (0, 0));
        assert_eq!((st.wsize, st.whave, st.wnext), (0, 0, 0));
        assert_eq!(st.total, 0);
        assert_eq!(st.window.len(), 2 * 32768, "reset keeps the buffer");
        assert_eq!((st.wrap, st.wbits), (5, 15));

        st.reset_with_window_bits(9).expect("zlib 9");
        assert_eq!(st.wbits, 9);
        assert!(st.window.is_empty(), "a different window size drops the buffer");
    }
}
