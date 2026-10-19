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
//! The 32 KiB sliding window (`updatewindow` / `inflateGetDictionary`).
//!
//! # Why the window is mirrored
//!
//! The C keeps a ring of `wsize` bytes with a write index `wnext`; `inflateGetDictionary`
//! then has to copy *two* runs to present the bytes in stream order
//! (`window[wnext..whave]` followed by `window[0..wnext]`), and every match copy has to
//! handle three wrap cases (`wnext == 0`, `wnext < distance`, else).
//!
//! This port stores the same bytes twice: `window[i] == window[i + wsize]` for every
//! `i < wsize`. Consequences, all of them internal:
//!
//! * the bytes in stream order are the contiguous slice `window[wnext .. wnext + whave]`,
//!   which is exactly what `inflateGetDictionary` returns — no copy, no second run;
//! * the byte `distance` back from the write position is `window[wsize + wnext - distance]`,
//!   valid for every distance up to `wsize` without a wrap case, and reading forward from
//!   there stays inside the two-copy buffer (`wsize + wnext - distance + distance ==
//!   wsize + wnext <= 2 * wsize`);
//! * the write path writes each byte to both halves, so the mirror invariant holds by
//!   construction.
//!
//! Nothing outside this module can tell the difference: `wsize`, `whave` and `wnext` keep
//! the C's meaning and are updated by the C's arithmetic, and both read paths return the
//! same bytes in the same order as the ring does.

use crate::inflate::state::InflateState;

/// Allocate the window buffer if it is not there yet (`ZALLOC(1 << wbits)`).
///
/// Returns `false` only when `wbits` is out of range, which `inflateReset2` makes
/// impossible; the caller turns that into a data error rather than trusting a
/// zero-length window.
pub(crate) fn ensure_allocated(st: &mut InflateState) -> bool {
    if !st.window.is_empty() {
        return true;
    }
    let wbits = st.wbits;
    if wbits > 15 {
        return false;
    }
    let size = 1usize << wbits;
    st.window = vec![0u8; 2 * size];
    true
}

/// Write `data` at ring position `pos`, updating both halves of the mirror.
///
/// `pos + data.len()` must stay inside one copy (`<= wsize`), which every caller
/// guarantees because the C never copies across the ring's end in one `zmemcpy`.
fn write_at(st: &mut InflateState, pos: usize, data: &[u8]) -> bool {
    let wsize = st.wsize as usize;
    let end = pos + data.len();
    if end > wsize || st.window.len() != 2 * wsize {
        return false;
    }
    st.window[pos..end].copy_from_slice(data);
    st.window[wsize + pos..wsize + end].copy_from_slice(data);
    true
}

/// `updatewindow`: append the last `copy` bytes of `end` to the window.
///
/// `end` is the output produced **so far in this `inflate()` call**, so the bytes to
/// keep are its last `copy` (at most `wsize`) — the C's `zmemcpy(window, end - copy,
/// …)` with `end` pointing past the last byte produced.
pub(crate) fn update(st: &mut InflateState, end: &[u8], copy: usize) -> bool {
    if !ensure_allocated(st) {
        return false;
    }
    if st.wsize == 0 {
        st.wsize = 1u32 << st.wbits;
        st.wnext = 0;
        st.whave = 0;
    }
    let wsize = st.wsize as usize;
    if copy > end.len() {
        // The caller always passes a produced count no larger than the output it hands
        // over; refusing here keeps a mismatch a typed error.
        return false;
    }
    if copy >= wsize {
        let from = end.len() - wsize;
        if !write_at(st, 0, &end[from..]) {
            return false;
        }
        st.wnext = 0;
        st.whave = wsize as u32;
        return true;
    }

    let wnext = st.wnext as usize;
    let mut dist = wsize - wnext;
    if dist > copy {
        dist = copy;
    }
    let from = end.len() - copy;
    if !write_at(st, wnext, &end[from..from + dist]) {
        return false;
    }
    let rest = copy - dist;
    if rest != 0 {
        // The copy wrapped: the remainder goes to the start of the ring.
        if !write_at(st, 0, &end[end.len() - rest..]) {
            return false;
        }
        st.wnext = rest as u32;
        st.whave = wsize as u32;
    } else {
        st.wnext += dist as u32;
        if st.wnext as usize == wsize {
            st.wnext = 0;
        }
        if (st.whave as usize) < wsize {
            st.whave += dist as u32;
        }
    }
    true
}

/// Where a match of `back` bytes (a distance backwards from the write position) starts.
///
/// `None` when `back` reaches further back than the window holds — the caller's
/// "invalid distance too far back" test, done with the same bound as the C
/// (`back > whave`).
pub(crate) fn match_start(st: &InflateState, back: usize) -> Option<usize> {
    if st.wsize == 0 || back > st.whave as usize || back == 0 {
        return None;
    }
    let start = st.wsize as usize + st.wnext as usize - back;
    if start + back > st.window.len() {
        return None;
    }
    Some(start)
}

/// Copy `dst.len()` bytes out of the window starting at `start`.
///
/// `false` if the range leaves the buffer (impossible for a start returned by
/// [`match_start`] with a length no larger than `back`).
pub(crate) fn copy_out(st: &InflateState, start: usize, dst: &mut [u8]) -> bool {
    match st.window.get(start..start + dst.len()) {
        Some(src) => {
            dst.copy_from_slice(src);
            true
        }
        None => false,
    }
}

/// `inflateGetDictionary`: the bytes a stream continuing from here would need.
///
/// The C copies two runs, `window[wnext .. whave]` and then `window[0 .. wnext]`, in
/// ring order. With the mirror the second run equals `window[wsize .. wsize + wnext]`,
/// so the two are the single contiguous slice ending at the write position,
/// `window[wsize + wnext - whave .. wsize + wnext]` — up to `w_size` bytes, oldest
/// first. (The two-run form is contiguous only because `whave <= wsize` and either
/// `wnext == whave` — a window that has not wrapped — or `whave == wsize`.)
pub(crate) fn dictionary(st: &InflateState) -> &[u8] {
    if st.whave == 0 {
        return &[];
    }
    let end = st.wsize as usize + st.wnext as usize;
    let start = end - st.whave as usize;
    st.window.get(start..end).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::state::InflateState;

    /// A window configures itself on first use and keeps the mirror invariant.
    #[test]
    fn the_window_mirrors_itself() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(8).expect("zlib 8");
        assert_eq!(st.wsize, 0);
        assert!(update(&mut st, b"hello", 5));
        assert_eq!((st.wsize, st.whave, st.wnext), (256, 5, 5));
        assert_eq!(&st.window[..256], &st.window[256..]);
        assert_eq!(dictionary(&st), b"hello");
        // A distance-5 match from `wnext = 5` starts at the mirror of position 0.
        let start = match_start(&st, 5).expect("five bytes of history");
        assert_eq!(&st.window[start..start + 5], b"hello");
    }

    /// A write that crosses the ring end wraps, and the dictionary stays in stream
    /// order.
    #[test]
    fn a_wrapping_write_keeps_stream_order() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(8).expect("zlib 8");
        let a: Vec<u8> = (0..200u8).collect();
        assert!(update(&mut st, &a, 200));
        assert_eq!((st.whave, st.wnext), (200, 200));
        // 100 more bytes: 56 fill the ring to its end, the other 44 wrap to its start.
        let b: Vec<u8> = (200..244u8).chain(0..56).collect();
        assert!(update(&mut st, &b, 100));
        assert_eq!((st.whave, st.wnext), (256, 44), "the write wrapped");
        // The whole (now full) history in stream order: the last 256 bytes written.
        let written: Vec<u8> = (0..200u8).chain(200..244).chain(0..56).collect();
        let expect = &written[written.len() - 256..];
        let dict = dictionary(&st);
        assert_eq!(dict.len(), 256);
        assert_eq!(dict, expect);
        // A one-byte-back match reads the newest byte; a full-window one reads it all.
        let start = match_start(&st, 1).expect("one byte of history");
        assert_eq!(st.window[start], expect[255]);
        let start = match_start(&st, 256).expect("the whole window");
        assert_eq!(&st.window[start..start + 256], expect);
        assert_eq!(match_start(&st, 257), None, "past whave");
    }

    /// More than `wsize` bytes of history replaces the window wholesale with the tail.
    #[test]
    fn a_full_window_keeps_only_the_tail() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(8).expect("zlib 8");
        let big: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
        assert!(update(&mut st, &big, big.len()));
        assert_eq!((st.wsize, st.whave, st.wnext), (256, 256, 0));
        assert_eq!(dictionary(&st), &big[big.len() - 256..]);
    }

    /// `match_start` never hands back a range the window cannot serve.
    #[test]
    fn match_starts_are_bounded() {
        let mut st = InflateState::new();
        assert_eq!(match_start(&st, 1), None, "no window at all");
        st.reset_with_window_bits(8).expect("zlib 8");
        assert!(update(&mut st, b"abcdef", 6));
        assert_eq!(match_start(&st, 0), None);
        assert_eq!(match_start(&st, 6), Some(st.wsize as usize));
        assert_eq!(match_start(&st, 7), None);
        // Reading the whole distance reaches the write position but not past it.
        let start = match_start(&st, 6).expect("six bytes of history");
        let mut out = [0u8; 6];
        assert!(copy_out(&st, start, &mut out));
        assert_eq!(&out, b"abcdef");
    }
}
