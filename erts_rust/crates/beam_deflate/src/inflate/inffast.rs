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
//! `inffast.c` — the bulk literal/match decoder.
//!
//! Entered from [`super::pump`] at `LEN` when at least six input bytes and 258 output
//! bytes are available, it decodes whole literal/length/distance symbols without
//! re-checking either buffer, then returns the unused whole input bytes to the bit
//! accumulator. That is why it needs no per-symbol bounds checks: a length/distance
//! pair can consume at most 48 bits (15 + 5 for the length, 15 + 13 for the distance)
//! and produce at most 258 bytes, both covered by the entry conditions.
//!
//! It leaves `st.mode` at `LEN` (out of input, or fewer than 258 bytes of output left),
//! `TYPE` (end of block) or `BAD` (invalid code, or a distance reaching further back
//! than the window holds) — the same three outcomes as the C.
//!
//! Two interior simplifications, both unobservable:
//!
//! * the C's three window-copy cases (`wnext == 0`, `wnext < distance`, else) collapse
//!   into one, because the window is stored mirrored ([`super::window`]) — all three
//!   compute the same byte sequence;
//! * the unrolled three-at-a-time copies are byte loops. They write the same bytes in
//!   the same order (and, for an overlapping match, rely on the same
//!   read-before-overwrite order).

use crate::inflate::state::{InflateState, Mode};
use crate::inflate::window;

/// The stream's cursor pair, shared with [`super::pump`]'s locals.
///
/// `next`/`have` index the input (`have == input.len() - next`), `put`/`left` the output
/// (`left == output.len() - put`).
pub(crate) struct FastIo {
    /// Next input byte.
    pub next: usize,
    /// Input bytes left.
    pub have: usize,
    /// Next output byte.
    pub put: usize,
    /// Output bytes left.
    pub left: usize,
}

/// Decode literals and matches until the input or output runs out, the block ends, or
/// the data is invalid (`inflate_fast`).
pub(crate) fn inflate_fast(
    st: &mut InflateState,
    input: &[u8],
    output: &mut [u8],
    io: &mut FastIo,
) {
    debug_assert!(io.have >= 6 && io.left >= 258, "inflate_fast entry conditions");

    let mut next = io.next;
    let mut put = io.put;
    let mut hold = st.hold;
    let mut bits = st.bits;
    let lmask = (1u64 << st.lenbits) - 1;
    let dmask = (1u64 << st.distbits) - 1;
    // `beg` (the output position at `inflate()` entry) is index 0: `put` counts the
    // bytes this call has produced, so `put - beg` is `put`.
    let last = next + (io.have - 5);
    let end = put + (io.left - 257);

    'fast: {
        // One byte of input, or a data error if the 6-byte guarantee ever failed.
        macro_rules! byte {
            () => {
                match input.get(next) {
                    Some(&byte) => {
                        next += 1;
                        byte
                    }
                    None => {
                        st.mode = Mode::Bad;
                        break 'fast;
                    }
                }
            };
        }

        loop {
            if bits < 15 {
                let b = byte!();
                hold += u64::from(b) << bits;
                bits += 8;
                let b = byte!();
                hold += u64::from(b) << bits;
                bits += 8;
            }
            let mut here = st.lcode((hold & lmask) as usize);

            // Decode one literal, or one length/distance pair. `break 'dolen` ends
            // the symbol; `continue 'dolen` follows a second-level code.
            'dolen: loop {
                // At least 15 bits are loaded, which covers the longest code and the
                // longest root-plus-sub-table pair (9 + 6 for literals/lengths, 6 + 9
                // for distances).
                let drop = u32::from(here.bits);
                hold >>= drop;
                bits -= drop;
                let op = u32::from(here.op);

                if op == 0 {
                    // A literal. `put < end <= output.len() - 257`.
                    output[put] = here.val as u8;
                    put += 1;
                    // The symbol is complete; the C's `if` chain falls through to the
                    // `while (in < last && out < end)` condition here.
                    break 'dolen;
                }
                if op & 16 != 0 {
                    // A length, then a distance.
                    let mut len = u32::from(here.val);
                    let nextra = op & 15;
                    if nextra != 0 {
                        if bits < nextra {
                            let b = byte!();
                            hold += u64::from(b) << bits;
                            bits += 8;
                        }
                        len += (hold & ((1u64 << nextra) - 1)) as u32;
                        hold >>= nextra;
                        bits -= nextra;
                    }
                    if bits < 15 {
                        let b = byte!();
                        hold += u64::from(b) << bits;
                        bits += 8;
                        let b = byte!();
                        hold += u64::from(b) << bits;
                        bits += 8;
                    }
                    let mut dhere = st.dcode((hold & dmask) as usize);

                    // The distance code, possibly through a second-level table.
                    loop {
                        let drop = u32::from(dhere.bits);
                        hold >>= drop;
                        bits -= drop;
                        let dop = u32::from(dhere.op);

                        if dop & 16 != 0 {
                            let mut dist = u32::from(dhere.val);
                            let dextra = dop & 15;
                            if bits < dextra {
                                let b = byte!();
                                hold += u64::from(b) << bits;
                                bits += 8;
                                if bits < dextra {
                                    let b = byte!();
                                    hold += u64::from(b) << bits;
                                    bits += 8;
                                }
                            }
                            dist += (hold & ((1u64 << dextra) - 1)) as u32;
                            hold >>= dextra;
                            bits -= dextra;

                            if !copy_match(st, output, put, dist as usize, len as usize) {
                                // "invalid distance too far back"
                                st.mode = Mode::Bad;
                                break 'fast;
                            }
                            put += len as usize;
                            break;
                        }
                        if dop & 64 == 0 {
                            // A second-level distance code.
                            let index =
                                dhere.val as usize + (hold & ((1u64 << dop) - 1)) as usize;
                            dhere = st.dcode(index);
                            continue;
                        }
                        // "invalid distance code"
                        st.mode = Mode::Bad;
                        break 'fast;
                    }
                    break 'dolen;
                }
                if op & 64 == 0 {
                    // A second-level literal/length code.
                    let index = here.val as usize + (hold & ((1u64 << op) - 1)) as usize;
                    here = st.lcode(index);
                    continue 'dolen;
                }
                if op & 32 != 0 {
                    // End of block: the next `inflate()` iteration reads the next type.
                    st.mode = Mode::Type;
                    break 'fast;
                }
                // "invalid literal/length code"
                st.mode = Mode::Bad;
                break 'fast;
            }

            if !(next < last && put < end) {
                break;
            }
        }

        // Return the whole unused bytes to the caller (`bits < 8` on entry, so this
        // never rewinds past the symbol just decoded).
        let unused = (bits >> 3) as usize;
        next -= unused;
        bits -= (unused as u32) << 3;
        hold &= (1u64 << bits) - 1;
    }

    st.hold = hold;
    st.bits = bits;
    io.next = next;
    io.have = input.len() - next;
    io.put = put;
    io.left = output.len() - put;
}

/// Copy a `len`-byte match of `dist` bytes back, from the window and/or the output
/// already written. `false` when `dist` reaches further back than the window holds.
///
/// `put + len <= output.len()` and `dist <= put + whave` hold on entry, so the writes
/// stay inside `output` and the reads inside `output`-or-window.
///
/// Shared with [`super::pump`]'s `MATCH` state: the C splits the window case into three
/// `wnext` variants there and into the same three in `inflate_fast`, but with a mirrored
/// window ([`super::window`]) all six compute the same byte sequence — the window part
/// is the `back` bytes behind the write position and the remainder continues in the
/// output at `to - dist`.
pub(crate) fn copy_match(st: &InflateState, output: &mut [u8], put: usize, dist: usize, len: usize) -> bool {
    if dist <= put {
        // Entirely inside this call's output (`from = out - dist`).
        let mut from = put - dist;
        let mut to = put;
        for _ in 0..len {
            output[to] = output[from];
            to += 1;
            from += 1;
        }
        return true;
    }

    // Part of the match is in the window; `back` is how far behind the window's write
    // position it starts.
    let back = dist - put;
    let start = match window::match_start(st, back) {
        Some(start) => start,
        None => return false,
    };
    let from_window = back.min(len);
    let mut to = put;
    if !window::copy_out(st, start, &mut output[to..to + from_window]) {
        return false;
    }
    to += from_window;
    // The window part is followed by the rest, which by then lies in this call's
    // output: when `back < len` the window's write position *is* output index 0, so
    // the continuation starts at `to - dist == 0`.
    // The continuation starts at output index 0 (`to - dist` evaluated at the top of
    // the loop is 0), and reads its own writes as it goes.
    while to < put + len {
        output[to] = output[to - dist];
        to += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::state::InflateState;

    /// A match that reaches into the window and then continues in the output is the
    /// case the C splits into `wnext` variants; here it must simply produce the bytes
    /// of the last `dist` output bytes, repeated.
    #[test]
    fn window_matches_continue_in_the_output() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(8).expect("zlib 8");
        // Seed the window with 6 bytes, then "produce" 2 more bytes in this call.
        assert!(window::update(&mut st, b"abcdef", 6));
        let mut output = [0u8; 32];
        output[0] = b'g';
        output[1] = b'h';
        let put = 2;
        // Distance 8 == put(2) + back(6): starts 6 bytes back in the window.
        assert!(copy_match(&st, &mut output, put, 8, 4));
        assert_eq!(&output[..6], b"ghabcd");
        // Distance 1 is pure overlap inside the output.
        let mut output = [b'a'; 8];
        assert!(copy_match(&st, &mut output, 1, 1, 6));
        assert_eq!(&output[..8], b"aaaaaaaa");
    }

    /// A distance past the window's history is refused, never approximated.
    #[test]
    fn too_far_back_is_refused() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(8).expect("zlib 8");
        assert!(window::update(&mut st, b"abc", 3));
        let mut output = [0u8; 8];
        assert!(!copy_match(&st, &mut output, 0, 4, 3), "whave is only 3");
        assert!(copy_match(&st, &mut output, 0, 3, 3));
        assert_eq!(&output[..3], b"abc");
    }
}
