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
//! `inflate.c`'s `inflate()` — one pump step of the decompression state machine.
//!
//! [`inflate`] is a faithful port of the C function: the same `switch` over
//! [`Mode`], the same `NEEDBITS`/`PULLBYTE`/`BYTEBITS` accumulator discipline, the same
//! `inf_leave` epilogue (window refresh, check-value update, and the `Z_BUF_ERROR`
//! rules), and the same early returns (`Z_NEED_DICT`, `Z_MEM_ERROR`,
//! `Z_STREAM_ERROR`). Per-call output chunking is observable through the `zlib`
//! module's `inflate/2`/`safeInflate/2`, so the loop shape — not just the decoded
//! bytes — is part of the contract.
//!
//! # Why this is one long function
//!
//! In the C this is a single function whose states share twelve locals (`next`,
//! `have`, `put`, `left`, `hold`, `bits`, `in`, `out`, …). Splitting the states into
//! separate Rust functions would mean threading that whole bundle through every call
//! and would make the `goto inf_leave` edges (`break 'leave`) explicit parameters.
//! The states stay together, and the file is documented per state instead.
//!
//! The gzip member header/trailer states (`FLAGS`…`HCRC`, `LENGTH`) live here too:
//! `inflate.c` guards them with `#ifdef GUNZIP` inside the *same* switch, and they are
//! reached only through `wrap`, so separating them would produce a second machine
//! rather than a smaller file. A gzip member is therefore decoded by exactly this
//! state machine with `window_bits` `16 + 8 ..= 16 + 15` (or auto-detect,
//! `32 + 8 ..= 32 + 15`).
//!
//! # Interior differences from the C, all unobservable
//!
//! * `z_stream`'s pointer/length pairs become indices into the caller's slices, so
//!   `strm->next_in`/`avail_in` are `next`/`have` and every input/output touch is a
//!   bounds-checked slice operation. The C's "enough input"/"enough output" reasoning
//!   ([`super::inffast`]) is unchanged.
//! * `strm->msg` (the C's human-readable error string) has no counterpart: the
//!   observable is the `Z_DATA_ERROR` code, and the crate has no error-string surface.
//! * `strm->data_type` is not kept — nothing in the crate reads it (the `zlib` NIF
//!   layer overwrites `data_type` itself and never reads it back).
//! * `Z_TREES` does not exist in [`crate::Flush`], so the two `flush == Z_TREES`
//!   stopping points (`TYPE` and after a block header) are absent; `Z_BLOCK`'s is
//!   kept.
//! * `state->head` (`inflateGetHeader`) is not part of the crate's API: the gzip
//!   header is still *parsed and verified* (including its CRC-16), just not reported.
//! * `state->back`/`was` (only read by `inflateMark`) are not kept, so the
//!   "invalid distance too far back" test is the unconditional one of
//!   `--no-INFLATE_ALLOW_INVALID_DISTANCE_TOOFAR_ARRR`, which is how OTP builds zlib.

use beam_common::{BeamError, Result};

use crate::checksum::{adler32, crc32};
use crate::{Flush, Pump, Status};

use super::inffast::{copy_match, inflate_fast, FastIo};
use super::inftrees::{inflate_table, Code, TableType, INVALID};
use super::state::{InflateState, Mode};
use super::window;

/// `Z_DEFLATED` — the only compression method RFC 1950/1951 define.
const METHOD_DEFLATED: u32 = 8;

/// The permutation `datafmt` uses to store the code-length code lengths
/// (`inflate.c`'s `order[19]`).
const ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// `ZSWAP32` — the big-endian trailer/`DICTID` words, read out of the little-endian
/// bit accumulator.
fn swap32(word: u32) -> u32 {
    word.swap_bytes()
}

/// `CRC2` — fold the low two bytes of `word` into a CRC.
fn crc2(check: u32, word: u64) -> u32 {
    crc32(check, &[(word & 0xff) as u8, ((word >> 8) & 0xff) as u8])
}

/// `CRC4` — fold the low four bytes of `word` into a CRC.
fn crc4(check: u32, word: u64) -> u32 {
    crc32(
        check,
        &[
            (word & 0xff) as u8,
            ((word >> 8) & 0xff) as u8,
            ((word >> 16) & 0xff) as u8,
            ((word >> 24) & 0xff) as u8,
        ],
    )
}

/// `UPDATE_CHECK` — CRC-32 inside a gzip member (`flags != 0`), Adler-32 for zlib.
fn update_check(flags: i32, check: u32, buf: &[u8]) -> u32 {
    if flags != 0 {
        crc32(check, buf)
    } else {
        adler32(check, buf)
    }
}

/// The decoding-table entry at `index`, or the invalid-code marker if the index
/// leaves the table.
///
/// The index is `lenbits`/`distbits`/`code_bits` wide and those root widths are
/// chosen by [`inflate_table`] so that `2^root` entries exist; the `get` is a
/// belt-and-braces guard that turns a hypothetical out-of-range index into zlib's
/// data error (`INVALID` fails every `op & 16`/`op & 32`/`op & 64` test the same way
/// the C's garbage read would be intended to) rather than a panic.
fn entry(table: &[Code], index: u32) -> Code {
    table.get(index as usize).copied().unwrap_or(INVALID)
}

/// One `inflate()` call: consume `input`, fill `output`, return zlib's status and the
/// byte counts.
///
/// A stream-level failure is reported in [`Pump::status`] (`DataError`, `BufError`,
/// `StreamEnd`, `NeedDict`), never as `Err`; `Err` cannot arise from this function
/// (the configuration was already validated by `inflateReset2`), and the signature is
/// `Pump` rather than `Result<Pump>` for that reason.
///
/// # Panics
///
/// None on any input. Every slice index is either bounds-checked or covered by the
/// C's own invariant, and the invariants are written down where they are used.
pub(crate) fn inflate(
    st: &mut InflateState,
    input: &[u8],
    output: &mut [u8],
    flush: Flush,
) -> Pump {
    // `if (state->mode == TYPE) state->mode = TYPEDO;` — a stream left at `TYPE`
    // (block boundary) may not be re-reported as a `Z_BLOCK` stop.
    if st.mode == Mode::Type {
        st.mode = Mode::TypedO;
    }

    // LOAD(): the C's locals, mirrored as slice cursors. `have` input bytes are left
    // (so `next + have == input.len()`), `left` output bytes are left.
    let mut next = 0usize;
    let mut have = input.len();
    let mut put = 0usize;
    let mut left = output.len();
    let mut hold = st.hold;
    let mut bits = st.bits;
    // `in`/`out` in the C save the entry counts; `out` is reassigned by CHECK.
    let in_start = have;
    let mut out = left;
    let mut ret = Status::Ok;

    // PULLBYTE(): one input byte into the accumulator, or leave with what we have.
    macro_rules! pullbyte {
        // The label is passed in because `macro_rules!` labels are hygienic: a
        // `break 'leave` written in the macro body cannot name the caller's loop.
        ($l:lifetime) => {{
            if have == 0 {
                break $l;
            }
            let byte = match input.get(next) {
                Some(&byte) => byte,
                // Unreachable (`have` counts the bytes left); treated as "out of
                // input" rather than trusted.
                None => break $l,
            };
            next += 1;
            have -= 1;
            hold += u64::from(byte) << bits;
            bits += 8;
        }};
    }
    // NEEDBITS(n).
    macro_rules! needbits {
        ($l:lifetime, $n:expr) => {{
            let need: u32 = $n;
            while bits < need {
                pullbyte!($l);
            }
        }};
    }
    // BITS(n) — the low `n` bits of the accumulator.
    macro_rules! bits_of {
        ($n:expr) => {{
            let n: u32 = $n;
            (hold & ((1u64 << n) - 1)) as u32
        }};
    }
    // DROPBITS(n).
    macro_rules! dropbits {
        ($n:expr) => {{
            let n: u32 = $n;
            hold >>= n;
            bits -= n;
        }};
    }
    // INITBITS().
    macro_rules! initbits {
        () => {{
            hold = 0;
            bits = 0;
        }};
    }
    // BYTEBITS().
    macro_rules! bytebits {
        () => {{
            let n = bits & 7;
            hold >>= n;
            bits -= n;
        }};
    }

    'leave: loop {
        match st.mode {
            // Waiting for the zlib or gzip header (`wrap` decided which).
            Mode::Head => {
                if st.wrap == 0 {
                    // Raw RFC 1951: no header at all.
                    st.mode = Mode::TypedO;
                    continue;
                }
                needbits!('leave, 16);
                if st.wrap & 2 != 0 && hold == 0x8b1f {
                    // gzip magic: the header continues with ID1/ID2 already consumed.
                    if st.wbits == 0 {
                        st.wbits = 15;
                    }
                    st.check = crc32(0, &[]);
                    st.check = crc2(st.check, hold);
                    initbits!();
                    st.mode = Mode::Flags;
                    continue;
                }
                // zlib header: CMF/FLG must be a multiple of 31, and a gzip-only
                // stream must not be accepted here.
                if st.wrap & 1 == 0 || ((bits_of!(8) << 8) + ((hold >> 8) as u32)) % 31 != 0 {
                    st.mode = Mode::Bad;
                    continue;
                }
                if bits_of!(4) != METHOD_DEFLATED {
                    // "unknown compression method"
                    st.mode = Mode::Bad;
                    continue;
                }
                dropbits!(4);
                let len = bits_of!(4) + 8;
                if st.wbits == 0 {
                    // `inflateInit2(…, 0)`: take the window size from the header.
                    st.wbits = len;
                }
                if len > 15 || len > st.wbits {
                    // "invalid window size"
                    st.mode = Mode::Bad;
                    continue;
                }
                st.dmax = 1u32 << len;
                st.flags = 0; // a zlib header, not a gzip one
                st.check = 1;
                // FDICT is FLG bit 5, i.e. accumulator bit 13 before the 4-bit drop
                // above and bit 9 afterwards.
                st.mode = if hold & 0x200 != 0 {
                    Mode::DictId
                } else {
                    Mode::Type
                };
                initbits!();
            }

            // gzip member header: method and flags.
            Mode::Flags => {
                needbits!('leave, 16);
                st.flags = (hold & 0xffff) as i32;
                if st.flags & 0xff != METHOD_DEFLATED as i32 {
                    st.mode = Mode::Bad;
                    continue;
                }
                if st.flags & 0xe000 != 0 {
                    // "unknown header flags set" — reserved bits must be zero.
                    st.mode = Mode::Bad;
                    continue;
                }
                if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                    st.check = crc2(st.check, hold);
                }
                initbits!();
                st.mode = Mode::Time;
            }

            // gzip: modification time.
            Mode::Time => {
                needbits!('leave, 32);
                if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                    st.check = crc4(st.check, hold);
                }
                initbits!();
                st.mode = Mode::Os;
            }

            // gzip: extra flags and OS.
            Mode::Os => {
                needbits!('leave, 16);
                if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                    st.check = crc2(st.check, hold);
                }
                initbits!();
                st.mode = Mode::Exlen;
            }

            // gzip: the FEXTRA field's length.
            Mode::Exlen => {
                if st.flags & 0x0400 != 0 {
                    needbits!('leave, 16);
                    st.length = bits_of!(16);
                    if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                        st.check = crc2(st.check, hold);
                    }
                    initbits!();
                }
                st.mode = Mode::Extra;
            }

            // gzip: the FEXTRA field's bytes (only their CRC matters here).
            Mode::Extra => {
                if st.flags & 0x0400 != 0 {
                    // `copy` is at most `have`, so the slice is in bounds.
                    let mut copy = st.length as usize;
                    if copy > have {
                        copy = have;
                    }
                    if copy != 0 {
                        if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                            st.check = crc32(st.check, &input[next..next + copy]);
                        }
                        have -= copy;
                        next += copy;
                        st.length -= copy as u32;
                    }
                    if st.length != 0 {
                        break 'leave;
                    }
                }
                st.length = 0;
                st.mode = Mode::Name;
            }

            // gzip: the NUL-terminated FNAME.
            Mode::Name => {
                if st.flags & 0x0800 != 0 {
                    if have == 0 {
                        break 'leave;
                    }
                    // The C's `do { len = next[copy++] } while (len && copy < have)`:
                    // at least one byte (the caller checked `have != 0`), stopping at
                    // the NUL or when the available input runs out.
                    let mut copy = 0usize;
                    let mut len = 0u8;
                    while copy < have {
                        // `copy < have` means `next + copy < input.len()`.
                        len = match input.get(next + copy) {
                            Some(&byte) => byte,
                            None => break,
                        };
                        copy += 1;
                        if len == 0 {
                            break;
                        }
                    }
                    if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                        st.check = crc32(st.check, &input[next..next + copy]);
                    }
                    have -= copy;
                    next += copy;
                    if len != 0 {
                        break 'leave;
                    }
                }
                st.length = 0;
                st.mode = Mode::Comment;
            }

            // gzip: the NUL-terminated FCOMMENT.
            Mode::Comment => {
                if st.flags & 0x1000 != 0 {
                    if have == 0 {
                        break 'leave;
                    }
                    // The C's `do { len = next[copy++] } while (len && copy < have)`:
                    // at least one byte (the caller checked `have != 0`), stopping at
                    // the NUL or when the available input runs out.
                    let mut copy = 0usize;
                    let mut len = 0u8;
                    while copy < have {
                        // `copy < have` means `next + copy < input.len()`.
                        len = match input.get(next + copy) {
                            Some(&byte) => byte,
                            None => break,
                        };
                        copy += 1;
                        if len == 0 {
                            break;
                        }
                    }
                    if st.flags & 0x0200 != 0 && st.wrap & 4 != 0 {
                        st.check = crc32(st.check, &input[next..next + copy]);
                    }
                    have -= copy;
                    next += copy;
                    if len != 0 {
                        break 'leave;
                    }
                }
                st.length = 0;
                st.mode = Mode::Hcrc;
            }

            // gzip: the header CRC-16.
            Mode::Hcrc => {
                if st.flags & 0x0200 != 0 {
                    needbits!('leave, 16);
                    if st.wrap & 4 != 0 && hold != u64::from(st.check & 0xffff) {
                        // "header crc mismatch"
                        st.mode = Mode::Bad;
                        continue;
                    }
                    initbits!();
                }
                st.check = crc32(0, &[]);
                st.mode = Mode::Type;
            }

            // zlib `FDICT`: the 32-bit Adler-32 of the required dictionary.
            Mode::DictId => {
                needbits!('leave, 32);
                st.check = swap32(hold as u32);
                initbits!();
                st.mode = Mode::Dict;
            }

            // Ask for the dictionary, and stay here until `set_dictionary` supplies it.
            Mode::Dict => {
                if !st.havedict {
                    st.hold = hold;
                    st.bits = bits;
                    // The C returns straight out, without `inf_leave`: no window
                    // refresh and no counter update (no output can exist yet).
                    return Pump {
                        status: Status::NeedDict(st.check),
                        consumed: in_start - have,
                        produced: output.len() - left,
                    };
                }
                st.check = 1;
                st.mode = Mode::Type;
            }

            // `Z_BLOCK` stops on a block boundary.
            Mode::Type => {
                if flush == Flush::Block {
                    break 'leave;
                }
                st.mode = Mode::TypedO;
            }

            // The three block types.
            Mode::TypedO => {
                if st.last {
                    bytebits!();
                    st.mode = Mode::Check;
                    continue;
                }
                needbits!('leave, 3);
                st.last = bits_of!(1) != 0;
                dropbits!(1);
                st.mode = match bits_of!(2) {
                    0 => Mode::Stored,
                    1 => {
                        // Fixed Huffman codes: `inflate_fixed()`.
                        st.use_fixed_tables();
                        Mode::Len
                    }
                    2 => Mode::Table,
                    _ => Mode::Bad, // "invalid block type"
                };
                dropbits!(2);
            }

            // A stored (uncompressed) block.
            Mode::Stored => {
                bytebits!();
                needbits!('leave, 32);
                if (hold & 0xffff) != ((hold >> 16) ^ 0xffff) {
                    // "invalid stored block lengths"
                    st.mode = Mode::Bad;
                    continue;
                }
                st.length = bits_of!(16);
                initbits!();
                st.mode = Mode::Copy;
            }

            // Copy a stored block's bytes.
            Mode::Copy => {
                let mut copy = st.length as usize;
                if copy != 0 {
                    if copy > have {
                        copy = have;
                    }
                    if copy > left {
                        copy = left;
                    }
                    if copy == 0 {
                        break 'leave;
                    }
                    // `copy <= have` and `copy <= left` by the clips above.
                    output[put..put + copy].copy_from_slice(&input[next..next + copy]);
                    have -= copy;
                    next += copy;
                    left -= copy;
                    put += copy;
                    st.length -= copy as u32;
                } else {
                    st.mode = Mode::Type;
                }
            }

            // A dynamic block's table sizes.
            Mode::Table => {
                needbits!('leave, 14);
                st.nlen = bits_of!(5) + 257;
                dropbits!(5);
                st.ndist = bits_of!(5) + 1;
                dropbits!(5);
                st.ncode = bits_of!(4) + 4;
                dropbits!(4);
                // `PKZIP_BUG_WORKAROUND` is not defined in OTP's zlib.
                if st.nlen > 286 || st.ndist > 30 {
                    // "too many length or distance symbols"
                    st.mode = Mode::Bad;
                    continue;
                }
                st.have = 0;
                st.mode = Mode::LenLens;
            }

            // The code-length code's own lengths, in `ORDER` order.
            Mode::LenLens => {
                while st.have < st.ncode {
                    needbits!('leave, 3);
                    // `have < ncode <= 19 == ORDER.len()`.
                    let slot = ORDER[st.have as usize];
                    st.lens[slot] = bits_of!(3) as u16;
                    st.have += 1;
                    dropbits!(3);
                }
                while st.have < 19 {
                    let slot = ORDER[st.have as usize];
                    st.lens[slot] = 0;
                    st.have += 1;
                }
                // The C points both `lencode` and `distcode` at the shared arena and
                // builds the code-length table at the front of it.
                st.lencode_fixed = false;
                st.distcode_fixed = false;
                st.code_bits = 7;
                match inflate_table(
                    TableType::Codes,
                    &st.lens,
                    19,
                    &mut st.codetable,
                    st.code_bits,
                    &mut st.work,
                ) {
                    Ok((_used, root)) => st.code_bits = root,
                    Err(_) => {
                        // "invalid code lengths set"
                        st.mode = Mode::Bad;
                        continue;
                    }
                }
                st.have = 0;
                st.mode = Mode::CodeLens;
            }

            // The literal/length and distance code lengths.
            Mode::CodeLens => {
                while st.have < st.nlen + st.ndist {
                    // Decode one code-length symbol. The root index is only trusted
                    // once the entry says it fits in the bits we hold, exactly as the
                    // C's `do { … } while (here.bits > bits)` loop does.
                    let mut here;
                    loop {
                        here = entry(&st.codetable, bits_of!(st.code_bits));
                        if u32::from(here.bits) <= bits {
                            break;
                        }
                        pullbyte!('leave);
                    }
                    if here.val < 16 {
                        dropbits!(u32::from(here.bits));
                        // `have < nlen + ndist <= 316 < LENS_CAP`.
                        st.lens[st.have as usize] = here.val;
                        st.have += 1;
                        continue;
                    }
                    let (len, copy);
                    if here.val == 16 {
                        needbits!('leave, u32::from(here.bits) + 2);
                        dropbits!(u32::from(here.bits));
                        if st.have == 0 {
                            // "invalid bit length repeat"
                            st.mode = Mode::Bad;
                            break;
                        }
                        // `have >= 1`.
                        len = st.lens[st.have as usize - 1];
                        copy = 3 + bits_of!(2);
                        dropbits!(2);
                    } else if here.val == 17 {
                        needbits!('leave, u32::from(here.bits) + 3);
                        dropbits!(u32::from(here.bits));
                        len = 0;
                        copy = 3 + bits_of!(3);
                        dropbits!(3);
                    } else {
                        needbits!('leave, u32::from(here.bits) + 7);
                        dropbits!(u32::from(here.bits));
                        len = 0;
                        copy = 11 + bits_of!(7);
                        dropbits!(7);
                    }
                    if st.have + copy > st.nlen + st.ndist {
                        // "invalid bit length repeat"
                        st.mode = Mode::Bad;
                        break;
                    }
                    // `have + copy <= nlen + ndist <= 316 < LENS_CAP`, so every write
                    // below is in bounds.
                    let mut rest = copy;
                    while rest != 0 {
                        st.lens[st.have as usize] = len;
                        st.have += 1;
                        rest -= 1;
                    }
                }
                if st.mode == Mode::Bad {
                    continue;
                }
                // A literal/length tree without an end-of-block code can never
                // terminate a block.
                if st.lens[256] == 0 {
                    // "invalid code -- missing end-of-block"
                    st.mode = Mode::Bad;
                    continue;
                }
                // Build the two trees: root 9 for literals/lengths, 6 for distances
                // (the ENOUGH_* table sizes depend on those values — `inftrees.h`).
                st.lencode_fixed = false;
                st.distcode_fixed = false;
                match inflate_table(
                    TableType::Lens,
                    &st.lens[..st.nlen as usize],
                    st.nlen as usize,
                    &mut st.lens_table,
                    9,
                    &mut st.work,
                ) {
                    Ok((_used, root)) => st.lenbits = root,
                    Err(_) => {
                        // "invalid literal/lengths set"
                        st.mode = Mode::Bad;
                        continue;
                    }
                }
                let nlen = st.nlen as usize;
                match inflate_table(
                    TableType::Dists,
                    &st.lens[nlen..],
                    st.ndist as usize,
                    &mut st.dist_table,
                    6,
                    &mut st.work,
                ) {
                    Ok((_used, root)) => st.distbits = root,
                    Err(_) => {
                        // "invalid distances set"
                        st.mode = Mode::Bad;
                        continue;
                    }
                }
                st.mode = Mode::Len;
            }

            // Decode a literal or a length.
            Mode::Len => {
                // Enough input for any symbol and enough output for any match: hand
                // over to the bulk decoder.
                if have >= 6 && left >= 258 {
                    let mut io = FastIo {
                        next,
                        have,
                        put,
                        left,
                    };
                    st.hold = hold;
                    st.bits = bits;
                    inflate_fast(st, input, output, &mut io);
                    hold = st.hold;
                    bits = st.bits;
                    next = io.next;
                    have = io.have;
                    put = io.put;
                    left = io.left;
                    continue;
                }
                let mut here;
                loop {
                    here = st.lcode(bits_of!(st.lenbits) as usize);
                    if u32::from(here.bits) <= bits {
                        break;
                    }
                    pullbyte!('leave);
                }
                if here.op != 0 && here.op & 0xf0 == 0 {
                    // A link to a second-level table: `here` becomes `last`.
                    let last = here;
                    let second = u32::from(last.bits) + u32::from(last.op);
                    loop {
                        let index = u32::from(last.val)
                            + (bits_of!(second) >> u32::from(last.bits));
                        here = st.lcode(index as usize);
                        if u32::from(last.bits) + u32::from(here.bits) <= bits {
                            break;
                        }
                        pullbyte!('leave);
                    }
                    dropbits!(u32::from(last.bits));
                }
                dropbits!(u32::from(here.bits));
                st.length = u32::from(here.val);
                if here.op == 0 {
                    st.mode = Mode::Lit;
                    continue;
                }
                if here.op & 32 != 0 {
                    // End of block.
                    st.mode = Mode::Type;
                    continue;
                }
                if here.op & 64 != 0 {
                    // "invalid literal/length code"
                    st.mode = Mode::Bad;
                    continue;
                }
                st.extra = u32::from(here.op) & 15;
                st.mode = Mode::LenExt;
            }

            // A length's extra bits.
            Mode::LenExt => {
                if st.extra != 0 {
                    needbits!('leave, st.extra);
                    st.length += bits_of!(st.extra);
                    dropbits!(st.extra);
                }
                st.mode = Mode::Dist;
            }

            // Decode a distance.
            Mode::Dist => {
                let mut here;
                loop {
                    here = st.dcode(bits_of!(st.distbits) as usize);
                    if u32::from(here.bits) <= bits {
                        break;
                    }
                    pullbyte!('leave);
                }
                if here.op & 0xf0 == 0 {
                    let last = here;
                    let second = u32::from(last.bits) + u32::from(last.op);
                    loop {
                        let index = u32::from(last.val)
                            + (bits_of!(second) >> u32::from(last.bits));
                        here = st.dcode(index as usize);
                        if u32::from(last.bits) + u32::from(here.bits) <= bits {
                            break;
                        }
                        pullbyte!('leave);
                    }
                    dropbits!(u32::from(last.bits));
                }
                dropbits!(u32::from(here.bits));
                if here.op & 64 != 0 {
                    // "invalid distance code"
                    st.mode = Mode::Bad;
                    continue;
                }
                st.offset = u32::from(here.val);
                st.extra = u32::from(here.op) & 15;
                st.mode = Mode::DistExt;
            }

            // A distance's extra bits.
            Mode::DistExt => {
                if st.extra != 0 {
                    needbits!('leave, st.extra);
                    st.offset += bits_of!(st.extra);
                    dropbits!(st.extra);
                }
                // `INFLATE_STRICT`'s `dmax` check is not compiled into OTP's zlib (the
                // distance bound that *is* enforced is the window's `whave`, below).
                st.mode = Mode::Match;
            }

            // Copy a match's bytes, from the window and/or this call's output.
            Mode::Match => {
                if left == 0 {
                    break 'leave;
                }
                // `put` is the number of bytes this call has produced, i.e. the C's
                // `out - left`, because index 0 of `output` is this call's start.
                let len = (st.length as usize).min(left);
                if !copy_match(st, output, put, st.offset as usize, len) {
                    // "invalid distance too far back"
                    st.mode = Mode::Bad;
                    continue;
                }
                put += len;
                left -= len;
                st.length -= len as u32;
                if st.length == 0 {
                    st.mode = Mode::Len;
                }
            }

            // Write a literal byte.
            Mode::Lit => {
                if left == 0 {
                    break 'leave;
                }
                // `put < output.len()` because `left != 0`.
                output[put] = st.length as u8;
                put += 1;
                left -= 1;
                st.mode = Mode::Len;
            }

            // The zlib Adler-32 / gzip CRC-32 trailer.
            Mode::Check => {
                if st.wrap != 0 {
                    needbits!('leave, 32);
                    // Fold everything produced so far into the check value, then
                    // re-base `out` so the `inf_leave` bookkeeping only sees what
                    // follows the trailer.
                    out -= left;
                    st.total += out as u64;
                    if st.wrap & 4 != 0 && out != 0 {
                        // `out == put` here: both count this call's output.
                        st.check = update_check(st.flags, st.check, &output[..out]);
                    }
                    out = left;
                    if st.wrap & 4 != 0 {
                        let trailer = if st.flags != 0 {
                            hold & 0xffff_ffff
                        } else {
                            u64::from(swap32(hold as u32))
                        };
                        if trailer != u64::from(st.check) {
                            // "incorrect data check"
                            st.mode = Mode::Bad;
                            continue;
                        }
                    }
                    initbits!();
                }
                st.mode = Mode::Length;
            }

            // The gzip ISIZE trailer (output length modulo 2^32).
            Mode::Length => {
                if st.wrap != 0 && st.flags != 0 {
                    needbits!('leave, 32);
                    if st.wrap & 4 != 0 && (hold & 0xffff_ffff) != (st.total & 0xffff_ffff) {
                        // "incorrect length check"
                        st.mode = Mode::Bad;
                        continue;
                    }
                    initbits!();
                }
                st.mode = Mode::Done;
            }

            Mode::Done => {
                ret = Status::StreamEnd;
                break 'leave;
            }

            Mode::Bad => {
                ret = Status::DataError;
                break 'leave;
            }

            Mode::Mem => {
                // Unreachable: the window is a `Vec` (an allocation failure aborts,
                // it cannot be reported here) and no other allocation can fail.
                st.hold = hold;
                st.bits = bits;
                return Pump {
                    status: Status::MemError,
                    consumed: in_start - have,
                    produced: output.len() - left,
                };
            }

            // `SYNC` is only entered by the unported `inflateSync`; the C's `default`
            // is unreachable for the same reason. Both are `Z_STREAM_ERROR`.
            Mode::Sync => {
                st.hold = hold;
                st.bits = bits;
                return Pump {
                    status: Status::StreamError,
                    consumed: in_start - have,
                    produced: output.len() - left,
                };
            }
        }
    }

    // inf_leave: RESTORE(), refresh the window, update the counters and the check
    // value, then apply the `Z_BUF_ERROR` rules.
    st.hold = hold;
    st.bits = bits;

    // `out` is re-based by CHECK, so the byte count the window refresh copies is the
    // C's `out - strm->avail_out` (the bytes produced since that re-base).
    let since = out - left;
    let produced = output.len() - left;
    if st.wsize != 0
        || (produced != 0 && st.mode < Mode::Bad && (st.mode < Mode::Check || flush != Flush::Finish))
    {
        // `since <= put`: both count this call's output from the same origin.
        if !window::update(st, &output[..put], since) {
            st.mode = Mode::Mem;
            st.hold = hold;
            st.bits = bits;
            return Pump {
                status: Status::MemError,
                consumed: in_start - have,
                produced,
            };
        }
    }

    let consumed = in_start - have;
    out -= left;
    st.total += out as u64;
    if st.wrap & 4 != 0 && out != 0 {
        // The bytes produced since the CHECK re-base (or the whole output when CHECK
        // has not run): `put - out` is where that run starts.
        st.check = update_check(st.flags, st.check, &output[put - out..put]);
    }
    // A call that made no progress at all, or that could not finish under
    // `Z_FINISH`, is a buffer error rather than a success.
    if ((consumed == 0 && out == 0) || flush == Flush::Finish) && ret == Status::Ok {
        ret = Status::BufError;
    }

    Pump {
        status: ret,
        consumed,
        produced,
    }
}

/// `inflateSetDictionary` — install the preset dictionary `NeedDict` asked for.
///
/// The C has two failure modes here, both "the caller passed something unusable":
/// `Z_STREAM_ERROR` when a dictionary is offered to a framed stream that is not
/// waiting for one (or to any stream once decoding started), and `Z_DATA_ERROR` when
/// the dictionary's Adler-32 does not match the identifier the header carried. Both
/// are [`BeamError::BadArg`] — the crate's stream-level outcomes live in [`Status`],
/// and `Err` means "rejected configuration".
pub(crate) fn set_dictionary(st: &mut InflateState, dictionary: &[u8]) -> Result<()> {
    if st.wrap != 0 && st.mode != Mode::Dict {
        return Err(BeamError::BadArg);
    }
    if st.mode == Mode::Dict {
        // `adler32(0, Z_NULL, 0)`, the C's identity, is 1.
        let dictid = adler32(1, dictionary);
        if dictid != st.check {
            return Err(BeamError::BadArg);
        }
    }
    // `updatewindow(strm, dictionary + dictLength, dictLength)`.
    if !window::update(st, dictionary, dictionary.len()) {
        st.mode = Mode::Mem;
        return Err(BeamError::BadArg);
    }
    st.havedict = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty input cannot even produce a header: the C returns `Z_BUF_ERROR`
    /// (`in == 0 && out == 0`), never `Z_OK`.
    #[test]
    fn an_empty_input_is_a_buffer_error() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 16];
        let pump = inflate(&mut st, &[], &mut out, Flush::None);
        assert_eq!(pump.status, Status::BufError);
        assert_eq!((pump.consumed, pump.produced), (0, 0));
        assert_eq!(st.mode, Mode::Head, "still waiting for the header");
    }

    /// A truncated zlib header is a buffer error, and the bytes read so far stay in
    /// the accumulator so the next call can continue.
    #[test]
    fn a_truncated_header_waits_for_more_input() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 16];
        let pump = inflate(&mut st, &[0x78], &mut out, Flush::None);
        // Consuming a byte is progress, so the C reports `Z_OK` even though no output
        // exists yet.
        assert_eq!(pump.status, Status::Ok);
        assert_eq!(pump.consumed, 1);
        // `78 01 01 00 00 ff ff 00 00 00 01` (level 0, empty input) minus the byte
        // above: one stored block of length 0 and the Adler-32 of "".
        let rest = [0x01u8, 0x01, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01];
        let pump = inflate(&mut st, &rest, &mut out, Flush::None);
        assert_eq!(pump.status, Status::StreamEnd);
        assert_eq!(pump.consumed, rest.len());
        assert_eq!(pump.produced, 0);
        assert!(st.mode == Mode::Done);
    }

    /// A zlib stream carrying a stored block decodes through the pump, and the
    /// trailer's Adler-32 is verified.
    #[test]
    fn a_stored_block_decodes_and_verifies_its_adler32() {
        // zlib level 0 of "hello": 78 01 01 05 00 fa ff 68 65 6c 6c 6f 06 2c 02 15
        let image: [u8; 16] = [
            0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x06, 0x2c,
            0x02, 0x15,
        ];
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 32];
        let pump = inflate(&mut st, &image, &mut out, Flush::None);
        assert_eq!(pump.status, Status::StreamEnd);
        assert_eq!(&out[..pump.produced], b"hello");
        assert_eq!(pump.consumed, image.len());
        assert_eq!(st.check, adler32(1, b"hello"));
    }

    /// Flipping the trailer's checksum byte is a data error, not a silent accept.
    #[test]
    fn a_corrupt_adler32_is_a_data_error() {
        let mut image = [
            0x78u8, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x06, 0x2c,
            0x02, 0x15,
        ];
        image[15] ^= 0x01;
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 32];
        let pump = inflate(&mut st, &image, &mut out, Flush::None);
        assert_eq!(pump.status, Status::DataError);
        // The body still made it out before the trailer was rejected.
        assert_eq!(&out[..pump.produced], b"hello");
    }

    /// A zlib header whose check bits are wrong is rejected at the header.
    #[test]
    fn a_bad_zlib_header_check_is_a_data_error() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 8];
        // 0x78 0x00: (0x78 << 8) + 0x00 == 30720, which is not a multiple of 31.
        let pump = inflate(&mut st, &[0x78, 0x00, 0x00, 0x00], &mut out, Flush::None);
        assert_eq!(pump.status, Status::DataError);
    }

    /// Raw (`-15`) framing has no header and no trailer: the block bytes are the
    /// whole stream.
    #[test]
    fn a_raw_stream_has_no_framing() {
        // The level-0 "hello" body above, without the zlib wrapper or trailer.
        let raw = [0x01u8, 0x05, 0x00, 0xfa, 0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f];
        let mut st = InflateState::new();
        st.reset_with_window_bits(-15).expect("raw 15");
        let mut out = [0u8; 32];
        let pump = inflate(&mut st, &raw, &mut out, Flush::None);
        assert_eq!(pump.status, Status::StreamEnd);
        assert_eq!(&out[..pump.produced], b"hello");
        assert_eq!(pump.consumed, raw.len());
    }

    /// `Z_FINISH` never reports `Z_OK`: with the stream incomplete it is a buffer
    /// error even though bytes moved.
    #[test]
    fn finish_on_an_incomplete_stream_is_a_buffer_error() {
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 32];
        let pump = inflate(&mut st, &[0x78, 0x01], &mut out, Flush::Finish);
        assert_eq!(pump.status, Status::BufError);
        // And the completed case still reports the end of the stream.
        let rest = [0x01u8, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01];
        let pump = inflate(&mut st, &rest, &mut out, Flush::Finish);
        assert_eq!(pump.status, Status::StreamEnd);
    }

    /// A dictionary-carrying zlib stream reports `NeedDict` with the dictionary's
    /// Adler-32, and finishes once the dictionary is installed.
    #[test]
    fn a_preset_dictionary_is_requested_then_used() {
        let dict: &[u8] = b"the quick brown fox jumps over the lazy dog";
        // deflateSetDictionary over `dict`, then deflate("dict dict") with Z_FINISH.
        // (Generated with the oracle; see `tests/inflate_goldens.rs` for the same
        // stream in its golden form.)
        let image = golden_dict_stream();
        let mut st = InflateState::new();
        st.reset_with_window_bits(15).expect("zlib 15");
        let mut out = [0u8; 64];
        let pump = inflate(&mut st, &image, &mut out, Flush::None);
        let want = adler32(1, dict);
        assert_eq!(pump.status, Status::NeedDict(want));
        // Without the dictionary nothing can be produced.
        assert_eq!(pump.produced, 0);
        // The wrong dictionary is refused. `set_dictionary` also reports the error
        // through the stream state, which stays at DICT so a correct one can follow.
        assert!(
            matches!(set_dictionary(&mut st, b"wrong"), Err(BeamError::BadArg)),
            "a mismatched dictionary id is refused"
        );
        set_dictionary(&mut st, dict).expect("the stream's own dictionary");
        let pump = inflate(&mut st, &image[pump.consumed..], &mut out, Flush::None);
        assert_eq!(pump.status, Status::StreamEnd);
        assert_eq!(&out[..pump.produced], b"the quick brown fox");
    }

    /// The 15-byte zlib stream the oracle emits for
    /// `deflateSetDictionary("the quick brown fox jumps over the lazy dog")` followed
    /// by `deflate("the quick brown fox", finish)` at level 9.
    fn golden_dict_stream() -> Vec<u8> {
        vec![120, 249, 97, 60, 15, 250, 43, 193, 84, 10, 0, 71, 142, 7, 52]
    }
}
