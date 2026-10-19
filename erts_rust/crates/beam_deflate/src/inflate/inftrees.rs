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
//! `inftrees.c` — the canonical-Huffman decoding-table builder.
//!
//! [`inflate_table`] is `inflate_table()`: given the code lengths of a symbol set it
//! builds zlib's two-level decoding table — a root table of `2^root` entries indexed by
//! the low `root` bits of the bit accumulator, plus one sub-table per root entry whose
//! codes are longer than `root`. Every entry carries the operation (`op`), the number of
//! bits to drop (`bits`) and the value (`val`), exactly as `inftrees.h` documents:
//!
//! ```text
//! op = 0x00        literal (val is the byte)
//! op = 0b0000tttt  link to a sub-table with tttt index bits (val is its offset)
//! op = 0b0001eeee  length/distance with eeee extra bits (val is the base)
//! op = 0b01100000  end of block
//! op = 0b01000000  invalid code
//! ```
//!
//! The acceptance rules are zlib's, not an approximation of them: an over-subscribed
//! set is rejected, an incomplete set is rejected with the single exception of a
//! one-symbol set for a literal/length or distance tree (`max == 1`, the degenerate
//! code deflate emits for "only one distance is ever used"), and a table space
//! overflow past `ENOUGH_LENS`/`ENOUGH_DISTS` is reported so the caller can raise
//! `Z_DATA_ERROR`.
//!
//! The two fixed tables live in [`super::inffixed`] (`inffixed.h`'s data); a *test*
//! proves this builder reproduces them, so the shipped tables are the ground truth and
//! no table is built at run time for a fixed block.

/// `MAXBITS` — the longest code the deflate format allows.
pub(crate) const MAXBITS: usize = 15;

/// `ENOUGH_LENS` — table space a literal/length tree may use (`enough 286 9 15`).
pub(crate) const ENOUGH_LENS: usize = 852;

/// `ENOUGH_DISTS` — table space a distance tree may use (`enough 30 6 15`).
pub(crate) const ENOUGH_DISTS: usize = 592;

/// Table space a code-length tree needs. Its code lengths are 3-bit, so no code is
/// longer than the 7-bit root: the table is exactly `2^root ≤ 128` entries and can
/// never grow a sub-table. (The C shares the `codes[ENOUGH]` arena; the three tables
/// are separate arrays here, which the `ENOUGH_*` checks are independent of.)
pub(crate) const ENOUGH_CODES: usize = 128;

/// One decoding-table entry (`inftrees.h`'s `code`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Code {
    /// Operation, extra-bit count, sub-table index bits, or one of the two markers.
    pub op: u8,
    /// Bits of this code (or of the link) to drop after indexing.
    pub bits: u8,
    /// Literal byte, length/distance base, or sub-table offset.
    pub val: u16,
}

/// The invalid-code marker (`op = 0b01000000`).
pub(crate) const INVALID: Code = Code {
    op: 64,
    bits: 1,
    val: 0,
};

/// Which code is being built (`codetype`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TableType {
    /// The code-length alphabet of a dynamic block (`CODES`).
    Codes,
    /// A literal/length alphabet (`LENS`).
    Lens,
    /// A distance alphabet (`DISTS`).
    Dists,
}

/// Why [`inflate_table`] refused a set of code lengths.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TableError {
    /// Over-subscribed or incomplete: `inflate_table` returned `-1`.
    Invalid,
    /// The table does not fit in `ENOUGH_*`: `inflate_table` returned `+1`.
    ///
    /// Unreachable for a well-formed tree (the `ENOUGH_*` constants are the proven
    /// maxima), so it only guards against a future change of the root-table sizes.
    NotEnough,
}

/// Length codes 257..285 base values (`lbase`).
const LBASE: [u16; 31] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258, 0, 0,
];

/// Length codes 257..285 extra-bit counts (`lext`).
///
/// `199` (symbol 286) and `75` (symbol 287) are the reserved codes: both are
/// invalid-code markers (`op & 64` set, `op & 16`/`op & 32` clear).
const LEXT: [u16; 31] = [
    16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 18, 18, 18, 18, 19, 19, 19, 19, 20, 20, 20, 20,
    21, 21, 21, 21, 16, 199, 75,
];

/// Distance codes 0..29 base values (`dbase`).
const DBASE: [u16; 32] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049,
    3073, 4097, 6145, 8193, 12289, 16385, 24577, 0, 0,
];

/// Distance codes 0..29 extra-bit counts (`dext`).
const DEXT: [u16; 32] = [
    16, 16, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20, 21, 21, 22, 22, 23, 23, 24, 24, 25, 25, 26, 26,
    27, 27, 28, 28, 29, 29, 64, 64,
];

/// Build the decoding table for `lens[..codes]` (zlib's `inflate_table`).
///
/// `root_bits` is the requested root-table width; on success it is returned as the
/// *actual* width, which differs when the request exceeds the longest code or is
/// shorter than the shortest one. The return value is `(used, root)`, where `used` is
/// the number of entries written to `table` — the C's `*table += used`.
///
/// `table` must be at least `ENOUGH_LENS`/`ENOUGH_DISTS`/`ENOUGH_CODES` long for the
/// matching `table_type`, and `work` at least `codes`.
pub(crate) fn inflate_table(
    table_type: TableType,
    lens: &[u16],
    codes: usize,
    table: &mut [Code],
    root_bits: u32,
    work: &mut [u16],
) -> Result<(usize, u32), TableError> {
    // `count[len]` — how many codes of each length there are.
    let mut count = [0u16; MAXBITS + 1];
    for &len in lens.iter().take(codes) {
        let len = len as usize;
        // The callers only ever store 0..=15 (a 3-bit code length, a decoded
        // literal/length value below 16, or a repeat of one of those). The C
        // documents this as the caller's obligation; refusing here keeps a violation
        // a typed error instead of an out-of-bounds write.
        if len > MAXBITS {
            return Err(TableError::Invalid);
        }
        count[len] += 1;
    }

    // Bound the code lengths and keep the root inside them.
    let mut root = root_bits as usize;
    let mut max = MAXBITS;
    while max >= 1 && count[max] == 0 {
        max -= 1;
    }
    if root > max {
        root = max;
    }
    if max == 0 {
        // No symbols at all: a two-entry table of invalid codes, so decoding fails
        // with a data error rather than reading an empty table.
        if table.len() < 2 {
            return Err(TableError::NotEnough);
        }
        table[0] = INVALID;
        table[1] = INVALID;
        return Ok((2, 1));
    }
    let mut min = 1;
    while min < max && count[min] == 0 {
        min += 1;
    }
    if root < min {
        root = min;
    }

    // Over-subscribed or incomplete?
    let mut left: i32 = 1;
    for len in 1..=MAXBITS {
        left <<= 1;
        left -= count[len] as i32;
        if left < 0 {
            return Err(TableError::Invalid);
        }
    }
    // An incomplete set is only allowed for a single one-bit code in a literal/length
    // or distance alphabet — deflate's degenerate "one symbol" tree.
    if left > 0 && (table_type == TableType::Codes || max != 1) {
        return Err(TableError::Invalid);
    }

    // Sort the symbols by code length, keeping symbol order within a length.
    let mut offs = [0usize; MAXBITS + 1];
    offs[1] = 0;
    for len in 1..MAXBITS {
        offs[len + 1] = offs[len] + count[len] as usize;
    }
    for (sym, &len) in lens.iter().enumerate().take(codes) {
        let len = len as usize;
        if len != 0 {
            let slot = offs[len];
            match work.get_mut(slot) {
                Some(w) => *w = sym as u16,
                // `work` holds one slot per code, so this cannot happen.
                None => return Err(TableError::NotEnough),
            }
            offs[len] += 1;
        }
    }

    let (base, extra, match_sym): (&[u16], &[u16], usize) = match table_type {
        TableType::Codes => (&[], &[], 20),
        TableType::Lens => (&LBASE, &LEXT, 257),
        // `DISTS` leaves `match` at its initial 0 in the C, so every symbol takes the
        // base/extra branch.
        TableType::Dists => (&DBASE, &DEXT, 0),
    };

    // Fill the tables.
    let mut huff: usize = 0;
    let mut sym: usize = 0;
    let mut len: usize = min;
    let mut next: usize = 0;
    let mut curr: usize = root;
    let mut drop: usize = 0;
    let mut low: usize = usize::MAX;
    let mut used: usize = 1 << root;
    let mask: usize = used - 1;
    if (table_type == TableType::Lens && used > ENOUGH_LENS)
        || (table_type == TableType::Dists && used > ENOUGH_DISTS)
    {
        return Err(TableError::NotEnough);
    }

    loop {
        // The entry this symbol's code stands for.
        let mut here = Code {
            op: 0,
            bits: (len - drop) as u8,
            val: 0,
        };
        let symbol = match work.get(sym) {
            Some(&symbol) => symbol as usize,
            None => return Err(TableError::NotEnough),
        };
        if symbol + 1 < match_sym {
            here.val = symbol as u16;
        } else if symbol >= match_sym {
            let index = symbol - match_sym;
            here.op = *extra.get(index).ok_or(TableError::Invalid)? as u8;
            here.val = *base.get(index).ok_or(TableError::Invalid)?;
        } else {
            // End of block (`0b01100000`).
            here.op = 32 + 64;
            here.val = 0;
        }

        // Replicate the entry over every index whose low `len` bits are `huff`.
        let incr = 1usize << (len - drop);
        let mut fill = 1usize << curr;
        let table_step = fill; // the C reuses `min` for this
        loop {
            fill -= incr;
            match table.get_mut(next + (huff >> drop) + fill) {
                Some(slot) => *slot = here,
                None => return Err(TableError::NotEnough),
            }
            if fill == 0 {
                break;
            }
        }

        // Increment the code backwards (deflate stores Huffman codes bit-reversed).
        let mut incr = 1usize << (len - 1);
        while huff & incr != 0 {
            incr >>= 1;
        }
        if incr != 0 {
            huff &= incr - 1;
            huff += incr;
        } else {
            huff = 0;
        }

        // Next symbol.
        sym += 1;
        count[len] -= 1;
        if count[len] == 0 {
            if len == max {
                break;
            }
            let next_sym = match work.get(sym) {
                Some(&s) => s as usize,
                None => return Err(TableError::NotEnough),
            };
            len = match lens.get(next_sym) {
                Some(&l) => l as usize,
                None => return Err(TableError::NotEnough),
            };
        }

        // Start a sub-table when a code no longer fits the current one.
        if len > root && (huff & mask) != low {
            if drop == 0 {
                drop = root;
            }
            next += table_step;

            // Size the sub-table from the lengths that are left.
            curr = len - drop;
            let mut left = 1i32 << curr;
            while curr + drop < max {
                left -= count[curr + drop] as i32;
                if left <= 0 {
                    break;
                }
                curr += 1;
                left <<= 1;
            }

            used += 1 << curr;
            if (table_type == TableType::Lens && used > ENOUGH_LENS)
                || (table_type == TableType::Dists && used > ENOUGH_DISTS)
            {
                return Err(TableError::NotEnough);
            }

            low = huff & mask;
            match table.get_mut(low) {
                Some(link) => {
                    link.op = curr as u8;
                    link.bits = root as u8;
                    link.val = next as u16;
                }
                None => return Err(TableError::NotEnough),
            }
        }
    }

    // An incomplete code leaves exactly one entry to mark invalid.
    if huff != 0 {
        let here = Code {
            op: 64,
            bits: (len - drop) as u8,
            val: 0,
        };
        match table.get_mut(next + huff) {
            Some(slot) => *slot = here,
            None => return Err(TableError::NotEnough),
        }
    }

    Ok((used, root as u32))
}

#[cfg(test)]
mod tests {
    use super::super::inffixed::{DISTFIX, LENFIX};
    use super::*;

    /// The RFC 1951 fixed literal/length lengths.
    fn fixed_lens() -> Vec<u16> {
        let mut lens = vec![0u16; 288];
        for (sym, len) in lens.iter_mut().enumerate() {
            *len = match sym {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        lens
    }

    /// Building the fixed tables must reproduce `inffixed.h` entry for entry, after
    /// the single substitution `makefixed` applies (`index & 127 == 99` → `op = 64`,
    /// which the header documents as unobservable: both are invalid-code markers).
    #[test]
    fn the_fixed_tables_match_inffixed_h() {
        let mut table = [Code::default(); ENOUGH_LENS];
        let mut work = [0u16; 288];
        let lens = fixed_lens();
        let (used, root) = inflate_table(TableType::Lens, &lens, 288, &mut table, 9, &mut work)
            .expect("the fixed literal/length lengths are a valid tree");
        assert_eq!((used, root), (512, 9), "the fixed lens tree is one root table");
        for low in 0..512 {
            // `makefixed` writes `op = 64` where the builder emits `199`/`75` — the
            // two reserved length symbols 286/287, all three invalid-code markers. It is
            // the only difference, so it is the only substitution the comparison makes.
            let mut built = table[low];
            if built.op == 199 || built.op == 75 {
                built.op = 64;
            }
            assert_eq!(built, LENFIX[low], "lenfix[{low}]");
        }

        let mut dtable = [Code::default(); ENOUGH_DISTS];
        let mut work = [0u16; 288];
        let dlens = [5u16; 32];
        let (used, root) = inflate_table(TableType::Dists, &dlens, 32, &mut dtable, 5, &mut work)
            .expect("32 five-bit distances are a valid tree");
        assert_eq!((used, root), (32, 5));
        assert_eq!(&dtable[..32], &DISTFIX[..]);
    }

    /// The tables decode the fixed codes: bit-reversed Huffman codes of the shortest
    /// length land in the root table with their own bit counts.
    #[test]
    fn the_fixed_tables_decode_the_fixed_codes() {
        // Symbol 256 (end of block) has code 0000000 (7 bits) → index 0 → the
        // end-of-block marker.
        assert_eq!(LENFIX[0].op, 96, "end of block");
        assert_eq!(LENFIX[0].bits, 7);
        // Literal 0 is code 00110000 (8 bits) reversed to 00001100 = 0x0c.
        assert_eq!(LENFIX[0x0c], Code { op: 0, bits: 8, val: 0 });
        // Literal 'a' (97): deflate's canonical code for 97 is 10010001 (8 bits),
        // reversed to 10001001 = 0x89.
        assert_eq!(LENFIX[0x89], Code { op: 0, bits: 8, val: 97 });
    }

    /// An empty alphabet builds the two-entry invalid table and never a zero-length
    /// root table.
    #[test]
    fn an_empty_alphabet_yields_an_invalid_table() {
        let mut table = [Code::default(); ENOUGH_CODES];
        let mut work = [0u16; 288];
        let lens = [0u16; 19];
        let (used, root) =
            inflate_table(TableType::Codes, &lens, 19, &mut table, 7, &mut work).expect("empty");
        assert_eq!((used, root), (2, 1));
        assert_eq!(table[0], INVALID);
        assert_eq!(table[1], INVALID);
        assert_eq!(table[0].op & 64, 64, "the invalid marker");
    }

    /// Over-subscribed (more codes than the length budget allows) is rejected, as is
    /// an incomplete set — except the one-symbol degenerate case.
    #[test]
    fn oversubscribed_and_incomplete_sets_are_rejected() {
        let mut table = [Code::default(); ENOUGH_LENS];
        let mut work = [0u16; 288];

        // Three one-bit codes cannot exist.
        let mut lens = [0u16; 4];
        lens[0] = 1;
        lens[1] = 1;
        lens[2] = 1;
        assert_eq!(
            inflate_table(TableType::Lens, &lens, 3, &mut table, 9, &mut work),
            Err(TableError::Invalid)
        );

        // Two two-bit codes leave two of the four prefixes unused: incomplete.
        let mut lens = [0u16; 4];
        lens[0] = 2;
        lens[1] = 2;
        assert_eq!(
            inflate_table(TableType::Lens, &lens, 2, &mut table, 9, &mut work),
            Err(TableError::Invalid)
        );

        // The same shape for a CODES tree is incomplete as well (CODES has no
        // degenerate exception).
        assert_eq!(
            inflate_table(TableType::Codes, &lens, 2, &mut table, 7, &mut work),
            Err(TableError::Invalid)
        );

        // A single one-bit code IS accepted for a literal/length tree (deflate's
        // one-distance tree) and decodes to that symbol.
        let mut lens = [0u16; 2];
        lens[0] = 1;
        let (used, root) =
            inflate_table(TableType::Dists, &lens, 1, &mut table, 6, &mut work).expect("degenerate");
        assert_eq!((used, root), (2, 1));
        assert_eq!(table[0].op & 64, 0, "the present code is not invalid");
        assert_eq!(table[1], INVALID, "the unused prefix is an invalid code");
    }

    /// A code length longer than `MAXBITS` is a typed error rather than a panic (the C
    /// documents it as the caller's responsibility; no caller can produce it).
    #[test]
    fn an_over_long_code_is_refused() {
        let mut table = [Code::default(); ENOUGH_LENS];
        let mut work = [0u16; 288];
        let mut lens = [0u16; 4];
        lens[0] = 16;
        assert_eq!(
            inflate_table(TableType::Lens, &lens, 1, &mut table, 9, &mut work),
            Err(TableError::Invalid)
        );
    }

    /// A tree that needs sub-tables (codes longer than the root) produces link entries
    /// whose `op` is the sub-table's index width and whose indexes reach the sub-table.
    #[test]
    fn long_codes_get_sub_tables() {
        let mut table = [Code::default(); ENOUGH_LENS];
        let mut work = [0u16; 288];
        // Two one-bit codes already account for the whole code space (2 x 2^-1), so
        // adding 9-bit codes on top is over-subscribed.
        let mut lens = vec![9u16; 256];
        lens[0] = 1;
        lens[1] = 1;
        assert_eq!(
            inflate_table(TableType::Lens, &lens, 256, &mut table, 6, &mut work),
            Err(TableError::Invalid)
        );

        // A valid deep tree: 1 code of length 1, 1 of length 2, … 1 of length 8, and
        // two of length 9 (sum 2^-1+…+2^-8 + 2*2^-9 == 1).
        let mut lens = vec![0u16; 10];
        for (i, len) in lens.iter_mut().enumerate().take(9) {
            *len = (i + 1) as u16;
        }
        lens[9] = 9;
        let (used, root) = inflate_table(TableType::Lens, &lens, 10, &mut table, 6, &mut work)
            .expect("complete tree");
        assert_eq!(root, 6);
        assert_eq!(used, 64 + 8, "the root table plus one 3-bit sub-table");
        // The length-7/8/9 codes do not fit the 6-bit root, so the root slots they share
        // are links with a 3-bit index pointing past the root table.
        // The single code of each length 1..=6 fills 32+16+8+4+2+1 == 63 of the 64 root
        // slots, so exactly one 6-bit prefix leads into the sub-table.
        let links: Vec<&Code> = table[..64].iter().filter(|c| c.op > 0 && c.op < 16).collect();
        assert_eq!(links.len(), 1, "one 6-bit prefix leads into the sub-table");
        for link in links {
            assert_eq!(link.op, 3, "a 3-bit sub-table index");
            assert_eq!(link.bits, 6);
            assert!(link.val as usize >= 64, "the link points past the root table");
            assert!((link.val as usize) < used, "and inside the table");
        }
    }
}
