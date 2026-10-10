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

//! Line-oriented input buffering — the `{line, N}` framing of `open_port/2`.
//!
//! A faithful Rust port of the C `LineBuf` used by the emulator's line-oriented port I/O
//! (`erts/emulator/beam/io.c`: `allocate_linebuf`/`read_linebuf`/`flush_linebuf` and the
//! `LINEBUF_*` states). [`LineBuffer::push`] accumulates raw bytes and
//! [`LineBuffer::take_lines`] hands out one frame per **complete** line:
//!
//! * a line is terminated by `\n`;
//! * a `\r` immediately followed by `\n` belongs to the terminator and is dropped, while
//!   a `\r` that is *not* followed by `\n` is kept as ordinary data;
//! * a line longer than the configured maximum ([`LineBuffer::new`]'s `max_len`) is
//!   delivered in `max_len`-byte chunks (the C `LINEBUF_NOEOL` case), so a driver never
//!   buffers an unbounded amount of unterminated input.
//!
//! The terminator bytes are **not** part of the delivered frames — like the emulator,
//! which delivers a line's content without its `\n` (`read_linebuf` reports `retlen`
//! excluding the newline). Because the port input queue is purely bytes, this byte-only
//! API does not distinguish a newline-terminated line from a length-split fragment: both
//! are just frames. A trailing partial line (no terminator yet) is held back until either
//! more input completes it or [`LineBuffer::flush_all`] delivers it as-is (the C's
//! `flush_linebuf`, called on EOF/`stop`).

use std::collections::VecDeque;

/// The maximum line length used when line mode is enabled without an explicit limit.
pub const DEFAULT_MAX_LINE_LEN: usize = 1024;

/// The `LineBuf` state machine (mirrors the C `LINEBUF_{MAIN,FULL,CR_INSIDE,CR_AFTER}`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Accumulating an ordinary line (`LINEBUF_MAIN`).
    Main,
    /// The current line reached `max_len`; the next byte decides whether it is split
    /// (`LINEBUF_FULL`).
    Full,
    /// A `\r` was seen; whether it is part of a terminator depends on the next byte
    /// (`LINEBUF_CR_INSIDE`).
    CrInside,
    /// The line is full and a `\r` follows it (`LINEBUF_CR_AFTER`).
    CrAfter,
}

/// Accumulates raw input and hands out complete lines (the `{line, N}` mode).
///
/// The buffer is fed with [`LineBuffer::push`] and drains either through
/// [`LineBuffer::take_lines`] / [`LineBuffer::drain`] (every complete frame so far) or
/// through [`LineBuffer::flush_all`] (the trailing partial line, delivered as-is).
pub struct LineBuffer {
    /// The maximum line length in bytes; `0` means "no length limit".
    max_len: usize,
    /// The current `LineBuf` state.
    state: State,
    /// The bytes of the line currently being assembled (its terminator is not included).
    line: Vec<u8>,
    /// Complete frames awaiting delivery, in arrival order.
    ready: VecDeque<Vec<u8>>,
}

impl LineBuffer {
    /// Creates a line buffer that splits a line longer than `max_len` bytes.
    ///
    /// A `max_len` of `0` disables length-based splitting: frames are then produced only
    /// at newline boundaries.
    pub fn new(max_len: usize) -> LineBuffer {
        LineBuffer {
            max_len,
            state: State::Main,
            line: Vec::new(),
            ready: VecDeque::new(),
        }
    }

    /// The configured maximum line length in bytes (`0` = unlimited).
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Sets the maximum line length (`0` = unlimited).
    ///
    /// The new limit applies to bytes pushed afterwards; a line already over the new
    /// limit is delivered as one chunk rather than being retroactively split.
    pub fn set_max_len(&mut self, max_len: usize) {
        self.max_len = max_len;
    }

    /// The number of bytes held back from delivery: the partial line plus a held `\r`
    /// whose fate is still undecided (it may yet turn out to be a `\r\n` terminator).
    pub fn pending(&self) -> usize {
        self.line.len() + usize::from(matches!(self.state, State::CrInside | State::CrAfter))
    }

    /// Appends `data`, extracting every complete line into the ready queue.
    pub fn push(&mut self, data: &[u8]) {
        let mut i = 0;
        while i < data.len() {
            let c = data[i];
            // A `\n` terminates the current line whatever the state — including a held
            // `\r`, which is then dropped as part of a `\r\n` terminator (`LINEBUF_EOL`).
            if c == b'\n' {
                self.state = State::Main;
                self.ready.push_back(std::mem::take(&mut self.line));
                i += 1;
                continue;
            }
            match self.state {
                State::Main => {
                    if self.at_limit() {
                        // The line is full: re-dispatch on this byte next iteration.
                        self.state = State::Full;
                    } else if c == b'\r' {
                        // Hold the `\r`: it is dropped if a `\n` follows.
                        self.state = State::CrInside;
                        i += 1;
                    } else {
                        self.line.push(c);
                        i += 1;
                    }
                }
                State::Full => {
                    if c == b'\r' {
                        self.state = State::CrAfter;
                        i += 1;
                    } else {
                        // Over-long line: deliver the full chunk and re-dispatch on this
                        // byte as the start of the next chunk (`LINEBUF_NOEOL`).
                        self.ready.push_back(std::mem::take(&mut self.line));
                        self.state = State::Main;
                    }
                }
                State::CrInside => {
                    // The held `\r` was not part of a `\r\n`: keep it as data.
                    self.line.push(b'\r');
                    self.state = State::Main;
                }
                State::CrAfter => {
                    // The full line plus a held `\r`: emit the full chunk, keep the `\r`.
                    self.ready.push_back(std::mem::take(&mut self.line));
                    self.state = State::CrInside;
                }
            }
        }
    }

    /// Takes every complete line buffered so far (in arrival order).
    ///
    /// A trailing partial line is *not* included — use [`LineBuffer::flush_all`] for it.
    pub fn take_lines(&mut self) -> Vec<Vec<u8>> {
        self.ready.drain(..).collect()
    }

    /// Drains every complete line buffered so far to `f`, in arrival order.
    pub fn drain(&mut self, f: &mut impl FnMut(&[u8])) {
        while let Some(frame) = self.ready.pop_front() {
            f(&frame);
        }
    }

    /// Delivers the trailing partial line as-is (called on EOF or `stop`).
    ///
    /// Returns the buffered bytes as frames (with a held `\r` restored), or an empty vector
    /// when nothing is held. Complete lines handed out by [`LineBuffer::take_lines`] are
    /// unaffected; the buffer is left empty and back in its initial state.
    ///
    /// A full line at the length limit followed by a held `\r` is delivered as **two**
    /// frames — `[full-line]` then `["\r"]` — matching the two `flush_linebuf` calls the C
    /// makes in that state (`io.c:3221-3226`: the `LINEBUF_CR_AFTER` arm returns the full
    /// line and resets to `LINEBUF_CR_INSIDE`, whose next call restores the `\r`). The bytes
    /// are identical to joining them; only the split differs.
    pub fn flush_all(&mut self) -> Vec<Vec<u8>> {
        match std::mem::replace(&mut self.state, State::Main) {
            State::CrInside => {
                // Restore the `\r` that was withheld pending a possible `\n`.
                self.line.push(b'\r');
                vec![std::mem::take(&mut self.line)]
            }
            State::CrAfter => {
                // A full line at the limit plus a held `\r`: the C delivers the line and
                // then the `\r` by two separate calls. The line is non-empty here by
                // construction (`CrAfter` is entered only from `Full`, which implies the
                // limit was reached and `max_len != 0`).
                let line = std::mem::take(&mut self.line);
                vec![line, vec![b'\r']]
            }
            State::Main | State::Full => {
                if self.line.is_empty() {
                    Vec::new()
                } else {
                    vec![std::mem::take(&mut self.line)]
                }
            }
        }
    }

    /// Whether the line currently being assembled reached the configured limit.
    ///
    /// `max_len == 0` never limits; `>=` (rather than `==`) keeps the invariant even when
    /// the limit is lowered while a longer line is already buffered.
    fn at_limit(&self) -> bool {
        self.max_len != 0 && self.line.len() >= self.max_len
    }
}

#[cfg(test)]
mod tests {
    use super::{LineBuffer, DEFAULT_MAX_LINE_LEN};

    fn lines(buf: &mut LineBuffer, data: &[u8]) -> Vec<Vec<u8>> {
        buf.push(data);
        buf.take_lines()
    }

    #[test]
    fn multiple_lines_in_one_chunk() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert_eq!(
            lines(&mut buf, b"one\ntwo\nthree\n"),
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        assert_eq!(buf.pending(), 0);
    }

    #[test]
    fn a_line_split_across_chunks_is_delivered_once_complete() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert!(lines(&mut buf, b"par").is_empty());
        assert_eq!(buf.pending(), 3);
        assert!(lines(&mut buf, b"tial").is_empty());
        assert_eq!(buf.pending(), 7);
        assert_eq!(lines(&mut buf, b" line\n"), vec![b"partial line".to_vec()]);
        assert_eq!(buf.pending(), 0);
    }

    #[test]
    fn a_blank_line_is_an_empty_frame() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert_eq!(
            lines(&mut buf, b"\n\n"),
            vec![Vec::<u8>::new(), Vec::<u8>::new()]
        );
    }

    #[test]
    fn carriage_return_before_newline_is_part_of_the_terminator() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        // `\r\n` terminates the line; the `\r` is not delivered as data.
        assert_eq!(lines(&mut buf, b"crlf\r\n"), vec![b"crlf".to_vec()]);
        // A lone `\r` is ordinary data.
        assert_eq!(lines(&mut buf, b"a\rb\n"), vec![b"a\rb".to_vec()]);
        // A `\r` split across chunks, followed by `\n`, is still a terminator.
        assert!(lines(&mut buf, b"split\r").is_empty());
        assert_eq!(buf.pending(), 6, "the held `\\r` counts as pending");
        assert_eq!(lines(&mut buf, b"\n"), vec![b"split".to_vec()]);
        assert_eq!(buf.pending(), 0);
    }

    #[test]
    fn an_over_long_line_is_split_at_the_limit() {
        let mut buf = LineBuffer::new(4);
        let chunks = lines(&mut buf, b"abcdefghijX\n");
        assert_eq!(
            chunks,
            vec![
                b"abcd".to_vec(),
                b"efgh".to_vec(),
                b"ijX".to_vec(),
            ],
            "an over-long line is split into `max_len`-byte chunks"
        );
        // Splitting is content-loss-free: the concatenation is the input, minus the
        // terminator, and the first two chunks are exactly `max_len` bytes long.
        let joined: Vec<u8> = chunks.concat();
        assert_eq!(joined, b"abcdefghijX");
        assert_eq!(chunks[0].len(), 4);
        assert_eq!(chunks[1].len(), 4);
    }

    #[test]
    fn a_line_exactly_at_the_limit_terminated_by_newline_is_one_frame() {
        let mut buf = LineBuffer::new(4);
        assert_eq!(lines(&mut buf, b"abcd\n"), vec![b"abcd".to_vec()]);
        // …and with a `\r\n` terminator it is still a single frame.
        assert_eq!(lines(&mut buf, b"wxyz\r\n"), vec![b"wxyz".to_vec()]);
    }

    #[test]
    fn a_zero_limit_disables_length_splitting() {
        let mut buf = LineBuffer::new(0);
        let long = vec![b'a'; 10_000];
        let mut data = long.clone();
        data.push(b'\n');
        assert_eq!(lines(&mut buf, &data), vec![long]);
    }

    #[test]
    fn only_a_complete_line_is_delivered_until_eof() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert_eq!(lines(&mut buf, b"complete\npartial"), vec![b"complete".to_vec()]);
        assert_eq!(buf.pending(), 7, "`partial` is held back");
        assert_eq!(buf.take_lines(), Vec::<Vec<u8>>::new());
        assert_eq!(buf.flush_all(), vec![b"partial".to_vec()]);
        assert_eq!(buf.pending(), 0);
        // A second flush finds nothing.
        assert!(buf.flush_all().is_empty());
    }

    #[test]
    fn flush_restores_a_held_carriage_return() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert!(lines(&mut buf, b"partial\r").is_empty());
        assert_eq!(buf.flush_all(), vec![b"partial\r".to_vec()]);
    }

    #[test]
    fn a_lone_carriage_return_is_flushed_as_a_frame() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert!(lines(&mut buf, b"\r").is_empty());
        assert_eq!(buf.pending(), 1, "the held `\\r` counts as pending");
        assert_eq!(buf.flush_all(), vec![b"\r".to_vec()]);
        assert_eq!(buf.pending(), 0);
    }

    #[test]
    fn flush_splits_a_full_line_and_its_held_carriage_return_like_the_c() {
        // A full line at the limit followed by a `\r` with no `\n` yet: the C's
        // `flush_linebuf` (`io.c:3221-3226`) delivers the full line and then the `\r`
        // through two separate calls, so two frames leave the buffer here.
        let mut buf = LineBuffer::new(4);
        assert!(lines(&mut buf, b"abcd").is_empty());
        assert!(lines(&mut buf, b"\r").is_empty());
        assert_eq!(buf.pending(), 5, "the full line plus the held `\\r`");
        assert_eq!(buf.flush_all(), vec![b"abcd".to_vec(), b"\r".to_vec()]);
        assert_eq!(buf.pending(), 0);
        assert!(buf.flush_all().is_empty());
    }

    #[test]
    fn flush_delivers_a_full_line_with_no_pending_carriage_return() {
        // The `LINEBUF_FULL`/`LINEBUF_MAIN` arms of `flush_linebuf` deliver the buffered
        // full line as one frame.
        let mut buf = LineBuffer::new(4);
        assert!(lines(&mut buf, b"abcd").is_empty());
        assert_eq!(buf.flush_all(), vec![b"abcd".to_vec()]);
        assert!(buf.flush_all().is_empty());
    }

    #[test]
    fn flush_is_a_noop_when_nothing_is_pending() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert_eq!(lines(&mut buf, b"done\n"), vec![b"done".to_vec()]);
        assert!(buf.flush_all().is_empty());
    }

    #[test]
    fn drain_hands_every_line_to_the_callback() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        buf.push(b"a\nbb\nccc\n");
        let mut seen = Vec::new();
        buf.drain(&mut |line| seen.push(line.to_vec()));
        assert_eq!(seen, vec![b"a".to_vec(), b"bb".to_vec(), b"ccc".to_vec()]);
        assert_eq!(buf.take_lines(), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn max_len_is_configurable_mid_stream() {
        let mut buf = LineBuffer::new(DEFAULT_MAX_LINE_LEN);
        assert_eq!(buf.max_len(), DEFAULT_MAX_LINE_LEN);
        buf.set_max_len(2);
        assert_eq!(buf.max_len(), 2);
        assert_eq!(lines(&mut buf, b"abc\n"), vec![b"ab".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn crlf_spread_across_chunks_at_the_limit() {
        // A full line, then `\r`, then `\n`: the `\r` is a terminator byte and the whole
        // `max_len`-byte line is delivered as one frame.
        let mut buf = LineBuffer::new(3);
        assert!(lines(&mut buf, b"abc").is_empty());
        assert!(lines(&mut buf, b"\r").is_empty());
        assert_eq!(lines(&mut buf, b"\n"), vec![b"abc".to_vec()]);
    }
}
