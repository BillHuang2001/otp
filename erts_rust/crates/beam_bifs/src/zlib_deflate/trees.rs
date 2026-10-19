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

//! Huffman coding and block emission — the port of `../../erts/emulator/zlib/trees.c`.
//!
//! This is the half of the encoder that turns a block's LZ77 symbols into bits, and it
//! is where almost all of zlib's output shape is decided: the frequencies are built into
//! optimal trees ([`DeflateState::build_tree`] over the shared `heap`), the *bit-length*
//! tree is grown on top of the two trees' code lengths
//! ([`DeflateState::build_bl_tree`]), and [`DeflateState::flush_block`] then compares the
//! three possible encodings of the block — stored, fixed-Huffman and dynamic-Huffman —
//! and emits the smallest (with zlib's exact tie-break: `static_lenb <= opt_lenb` picks
//! the fixed tree, `stored_len + 4 <= opt_lenb` picks the stored one).
//!
//! Every routine keeps the C's name, so a reader can put the two side by side; the only
//! differences are Rust bookkeeping (explicit indices instead of pointers).

use super::state::{
    DeflateState, TreeKind, BL_CODES, DYN_TREES, END_BLOCK, HEAP_SIZE, LITERALS, L_CODES,
    MAX_BITS, REPZ_11_138, REPZ_3_10, REP_3_6, STATIC_TREES,
};
use super::tables::{
    BASE_DIST, BASE_LENGTH, BL_ORDER, DIST_CODE, EXTRA_DBITS, EXTRA_LBITS, LENGTH_CODE,
    STATIC_DTREE, STATIC_LTREE,
};

/// `SMALLEST` — the heap slot of the least frequent node (`trees.c:480`).
const SMALLEST: usize = 1;

/// Which code table a symbol is sent through: the two static trees or a dynamic one.
#[derive(Clone, Copy)]
enum Codes {
    /// `static_ltree`.
    StaticLit,
    /// `static_dtree`.
    StaticDist,
    /// One of [`DeflateState`]'s three trees.
    Dyn(TreeKind),
}

impl Codes {
    /// `tree[c].Code` / `tree[c].Len`.
    fn entry(self, st: &DeflateState, c: usize) -> (u16, u16) {
        match self {
            Codes::StaticLit => STATIC_LTREE[c],
            Codes::StaticDist => STATIC_DTREE[c],
            Codes::Dyn(kind) => {
                let e = st.tree(kind)[c];
                (e.code, e.len)
            }
        }
    }
}

/// `d_code(dist)` (`deflate.h:320`): the distance code of a **distance minus one**.
fn d_code(dist: u32) -> usize {
    if dist < 256 {
        usize::from(DIST_CODE[dist as usize])
    } else {
        usize::from(DIST_CODE[256 + (dist >> 7) as usize])
    }
}

/// `bi_reverse` (`trees.c:154`): reverse the low `len` bits of `code`.
fn bi_reverse(code: u32, len: i32) -> u16 {
    let mut res = 0u32;
    let mut code = code;
    let mut len = len;
    loop {
        res |= code & 1;
        code >>= 1;
        res <<= 1;
        len -= 1;
        if len <= 0 {
            break;
        }
    }
    (res >> 1) as u16
}

impl<'a> DeflateState<'a> {
    /// `init_block` (`trees.c:440`): reset the three trees' frequencies for a new block.
    pub fn init_block(&mut self) {
        for n in 0..L_CODES {
            self.dyn_ltree[n].freq = 0;
        }
        for n in 0..super::state::D_CODES {
            self.dyn_dtree[n].freq = 0;
        }
        for n in 0..BL_CODES {
            self.bl_tree[n].freq = 0;
        }
        self.dyn_ltree[END_BLOCK].freq = 1;
        self.opt_len = 0;
        self.static_len = 0;
        self.sym_next = 0;
    }

    /// `send_code(s, c, tree)` (`trees.c:239`).
    fn emit_code(&mut self, codes: Codes, c: usize) {
        let (code, len) = codes.entry(self, c);
        self.send_bits(u32::from(code), i32::from(len));
    }

    /// `smaller(tree, n, m, depth)` (`trees.c:499`): frequency first, then the tree
    /// depth as the tie-breaker.
    fn smaller(&self, kind: TreeKind, n: i32, m: i32) -> bool {
        let tree = self.tree(kind);
        let (fn_, fm) = (tree[n as usize].freq, tree[m as usize].freq);
        fn_ < fm || (fn_ == fm && self.depth[n as usize] <= self.depth[m as usize])
    }

    /// `pqdownheap` (`trees.c:509`).
    fn pqdownheap(&mut self, kind: TreeKind, mut k: i32) {
        let v = self.heap[k as usize];
        let mut j = k << 1;
        while j <= self.heap_len {
            if j < self.heap_len
                && self.smaller(kind, self.heap[j as usize + 1], self.heap[j as usize])
            {
                j += 1;
            }
            if self.smaller(kind, v, self.heap[j as usize]) {
                break;
            }
            self.heap[k as usize] = self.heap[j as usize];
            k = j;
            j <<= 1;
        }
        self.heap[k as usize] = v;
    }

    /// `gen_bitlen` (`trees.c:540`): the optimal bit lengths of a tree, and the running
    /// `opt_len`/`static_len` totals.
    fn gen_bitlen(&mut self, kind: TreeKind) {
        let max_code = self.max_code(kind);
        let stree = kind.static_tree();
        let extra = kind.extra_bits();
        let base = kind.extra_base();
        let max_length = kind.max_length();
        let mut overflow: i32 = 0;

        self.bl_count = [0; MAX_BITS + 1];

        // In a first pass, compute the optimal bit lengths (which may overflow in the
        // case of the bit length tree).
        let root = self.heap[self.heap_max as usize];
        self.tree_mut(kind)[root as usize].len = 0;

        let mut h = self.heap_max + 1;
        while h < HEAP_SIZE as i32 {
            let n = self.heap[h as usize];
            let dad = self.tree(kind)[n as usize].dad;
            let mut bits = i32::from(self.tree(kind)[dad as usize].len) + 1;
            if bits > max_length {
                bits = max_length;
                overflow += 1;
            }
            self.tree_mut(kind)[n as usize].len = bits as u16;
            // We overwrite tree[n].Dad which is no longer needed (the C overlays the two
            // in a union; here both fields are kept, which is not observable).
            h += 1;
            if n > max_code {
                continue;
            }
            self.bl_count[bits as usize] += 1;
            let mut xbits = 0u32;
            if n as usize >= base {
                xbits = u32::from(extra[n as usize - base]);
            }
            let f = u64::from(self.tree(kind)[n as usize].freq);
            self.opt_len = self
                .opt_len
                .wrapping_add(f.wrapping_mul(u64::from(bits as u32 + xbits)));
            if let Some(stree) = stree {
                self.static_len = self
                    .static_len
                    .wrapping_add(f.wrapping_mul(u64::from(u32::from(stree[n as usize].1) + xbits)));
            }
        }
        if overflow == 0 {
            return;
        }

        // Find the first bit length which could increase.
        loop {
            let mut bits = max_length - 1;
            while self.bl_count[bits as usize] == 0 {
                bits -= 1;
            }
            self.bl_count[bits as usize] -= 1;
            self.bl_count[bits as usize + 1] += 2;
            self.bl_count[max_length as usize] -= 1;
            overflow -= 2;
            if overflow <= 0 {
                break;
            }
        }

        // Now recompute all bit lengths, scanning in increasing frequency. `h` is still
        // equal to HEAP_SIZE.
        let mut bits = max_length;
        while bits != 0 {
            let mut n = self.bl_count[bits as usize];
            while n != 0 {
                h -= 1;
                let m = self.heap[h as usize];
                if m > max_code {
                    continue;
                }
                if i32::from(self.tree(kind)[m as usize].len) != bits {
                    // `s->opt_len += ((ulg)bits - tree[m].Len) * tree[m].Freq;` — the
                    // unsigned difference wraps when `bits < Len`, which modulo 2^64 is
                    // the same as the signed difference the `i64` here computes.
                    let delta = i64::from(bits) - i64::from(self.tree(kind)[m as usize].len);
                    let adj = (delta * i64::from(self.tree(kind)[m as usize].freq)) as u64;
                    self.opt_len = self.opt_len.wrapping_add(adj);
                    self.tree_mut(kind)[m as usize].len = bits as u16;
                }
                n -= 1;
            }
            bits -= 1;
        }
    }

    /// `gen_codes` (`trees.c:203`): the canonical (bit-reversed) codes for a tree whose
    /// lengths are set.
    fn gen_codes(&mut self, kind: TreeKind, max_code: i32) {
        let mut next_code = [0u16; MAX_BITS + 1];
        let mut code = 0u32;
        for bits in 1..=MAX_BITS {
            code = (code + u32::from(self.bl_count[bits - 1])) << 1;
            next_code[bits] = code as u16;
        }
        for n in 0..=max_code {
            let len = i32::from(self.tree(kind)[n as usize].len);
            if len == 0 {
                continue;
            }
            let code = bi_reverse(u32::from(next_code[len as usize]), len);
            next_code[len as usize] = next_code[len as usize].wrapping_add(1);
            self.tree_mut(kind)[n as usize].code = code;
        }
    }

    /// `build_tree` (`trees.c:627`): one Huffman tree, its lengths, its codes and the
    /// `opt_len`/`static_len` contributions.
    pub fn build_tree(&mut self, kind: TreeKind) {
        let elems = kind.elems();
        let stree = kind.static_tree();
        let mut max_code: i32 = -1;

        self.heap_len = 0;
        self.heap_max = HEAP_SIZE as i32;

        for n in 0..elems {
            if self.tree(kind)[n].freq != 0 {
                self.heap_len += 1;
                self.heap[self.heap_len as usize] = n as i32;
                max_code = n as i32;
                self.depth[n] = 0;
            } else {
                self.tree_mut(kind)[n].len = 0;
            }
        }

        // The pkzip format requires that at least one distance code exists, and that at
        // least one bit should be sent even if there is only one possible code.
        while self.heap_len < 2 {
            self.heap_len += 1;
            let node = if max_code < 2 {
                max_code += 1;
                max_code
            } else {
                0
            };
            self.heap[self.heap_len as usize] = node;
            self.tree_mut(kind)[node as usize].freq = 1;
            self.depth[node as usize] = 0;
            // `s->opt_len--; if (stree) s->static_len -= stree[node].Len;` — `opt_len`
            // and `static_len` are `ulg` (`deflate.h:256`), so both wrap modulo 2^64.
            // The arithmetic is a ring throughout (`+=`/`-=` and a multiply), and
            // `_tr_flush_block` only ever shifts the wrapped value, so the wrap is
            // reproduced rather than "fixed": a small block can drive `opt_len` below
            // zero here, and the C's final `(opt_len+10)>>3` sees the wrapped total.
            self.opt_len = self.opt_len.wrapping_sub(1);
            if let Some(stree) = stree {
                self.static_len = self.static_len.wrapping_sub(u64::from(stree[node as usize].1));
            }
        }
        self.set_max_code(kind, max_code);

        // The elements heap[heap_len/2 + 1 .. heap_len] are leaves of the tree.
        let mut n = self.heap_len / 2;
        while n >= 1 {
            self.pqdownheap(kind, n);
            n -= 1;
        }

        // Construct the Huffman tree by repeatedly combining the least two frequent nodes.
        let mut node = elems as i32;
        loop {
            // pqremove: n = node of least frequency.
            let n = self.heap[SMALLEST];
            self.heap[SMALLEST] = self.heap[self.heap_len as usize];
            self.heap_len -= 1;
            self.pqdownheap(kind, SMALLEST as i32);

            let m = self.heap[SMALLEST];
            self.heap_max -= 1;
            self.heap[self.heap_max as usize] = n;
            self.heap_max -= 1;
            self.heap[self.heap_max as usize] = m;

            // Create a new node father of n and m.
            let (fn_, fm) = (
                self.tree(kind)[n as usize].freq,
                self.tree(kind)[m as usize].freq,
            );
            self.tree_mut(kind)[node as usize].freq = fn_ + fm;
            let d = self.depth[n as usize].max(self.depth[m as usize]) + 1;
            self.depth[node as usize] = d;
            self.tree_mut(kind)[n as usize].dad = node as u16;
            self.tree_mut(kind)[m as usize].dad = node as u16;
            node += 1;
            self.heap[SMALLEST] = node - 1;
            self.pqdownheap(kind, SMALLEST as i32);

            if self.heap_len < 2 {
                break;
            }
        }

        self.heap_max -= 1;
        let top = self.heap[SMALLEST];
        self.heap[self.heap_max as usize] = top;

        self.gen_bitlen(kind);
        self.gen_codes(kind, max_code);
    }

    /// `scan_tree` (`trees.c:712`): the bit-length-code frequencies of one tree.
    fn scan_tree(&mut self, kind: TreeKind, max_code: i32) {
        let mut prevlen: i32 = -1;
        let mut nextlen = i32::from(self.tree(kind)[0].len);
        let mut count = 0;
        let mut max_count = 7;
        let mut min_count = 4;

        if nextlen == 0 {
            max_count = 138;
            min_count = 3;
        }
        self.tree_mut(kind)[max_code as usize + 1].len = 0xffff;

        let mut n = 0;
        while n <= max_code {
            let curlen = nextlen;
            nextlen = i32::from(self.tree(kind)[n as usize + 1].len);
            count += 1;
            if count < max_count && curlen == nextlen {
                n += 1;
                continue;
            } else if count < min_count {
                self.bl_tree[curlen as usize].freq += count as u16;
            } else if curlen != 0 {
                if curlen != prevlen {
                    self.bl_tree[curlen as usize].freq += 1;
                }
                self.bl_tree[REP_3_6].freq += 1;
            } else if count <= 10 {
                self.bl_tree[REPZ_3_10].freq += 1;
            } else {
                self.bl_tree[REPZ_11_138].freq += 1;
            }
            count = 0;
            prevlen = curlen;
            if nextlen == 0 {
                max_count = 138;
                min_count = 3;
            } else if curlen == nextlen {
                max_count = 6;
                min_count = 3;
            } else {
                max_count = 7;
                min_count = 4;
            }
            n += 1;
        }
    }

    /// `send_tree` (`trees.c:753`): one tree, run-length coded through `bl_tree`.
    fn send_tree(&mut self, kind: TreeKind, max_code: i32) {
        let mut prevlen: i32 = -1;
        let mut nextlen = i32::from(self.tree(kind)[0].len);
        let mut count = 0;
        let mut max_count = 7;
        let mut min_count = 4;

        if nextlen == 0 {
            max_count = 138;
            min_count = 3;
        }

        let mut n = 0;
        while n <= max_code {
            let curlen = nextlen;
            nextlen = i32::from(self.tree(kind)[n as usize + 1].len);
            count += 1;
            if count < max_count && curlen == nextlen {
                n += 1;
                continue;
            } else if count < min_count {
                loop {
                    self.emit_code(Codes::Dyn(TreeKind::Bl), curlen as usize);
                    count -= 1;
                    if count == 0 {
                        break;
                    }
                }
            } else if curlen != 0 {
                if curlen != prevlen {
                    self.emit_code(Codes::Dyn(TreeKind::Bl), curlen as usize);
                    count -= 1;
                }
                self.emit_code(Codes::Dyn(TreeKind::Bl), REP_3_6);
                self.send_bits((count - 3) as u32, 2);
            } else if count <= 10 {
                self.emit_code(Codes::Dyn(TreeKind::Bl), REPZ_3_10);
                self.send_bits((count - 3) as u32, 3);
            } else {
                self.emit_code(Codes::Dyn(TreeKind::Bl), REPZ_11_138);
                self.send_bits((count - 11) as u32, 7);
            }
            count = 0;
            prevlen = curlen;
            if nextlen == 0 {
                max_count = 138;
                min_count = 3;
            } else if curlen == nextlen {
                max_count = 6;
                min_count = 3;
            } else {
                max_count = 7;
                min_count = 4;
            }
            n += 1;
        }
    }

    /// `build_bl_tree` (`trees.c:800`): the bit-length tree, returning the index in
    /// `bl_order` of the last code to send.
    fn build_bl_tree(&mut self) -> i32 {
        let lit_max = self.lit_max_code;
        let dist_max = self.dist_max_code;
        self.scan_tree(TreeKind::Lit, lit_max);
        self.scan_tree(TreeKind::Dist, dist_max);
        self.build_tree(TreeKind::Bl);

        let mut max_blindex = BL_CODES as i32 - 1;
        while max_blindex >= 3 {
            if self.bl_tree[usize::from(BL_ORDER[max_blindex as usize])].len != 0 {
                break;
            }
            max_blindex -= 1;
        }
        self.opt_len += 3 * (max_blindex as u64 + 1) + 5 + 5 + 4;
        max_blindex
    }

    /// `send_all_trees` (`trees.c:833`): the dynamic block's header.
    fn send_all_trees(&mut self, lcodes: i32, dcodes: i32, blcodes: i32) {
        self.send_bits((lcodes - 257) as u32, 5);
        self.send_bits((dcodes - 1) as u32, 5);
        self.send_bits((blcodes - 4) as u32, 4);
        for rank in 0..blcodes as usize {
            let len = self.bl_tree[usize::from(BL_ORDER[rank])].len;
            self.send_bits(u32::from(len), 3);
        }
        self.send_tree(TreeKind::Lit, lcodes - 1);
        self.send_tree(TreeKind::Dist, dcodes - 1);
    }

    /// `compress_block` (`trees.c:900`): the block's symbols through `ltree`/`dtree`.
    fn compress_block(&mut self, dyn_trees: bool) {
        let (ltree, dtree) = if dyn_trees {
            (Codes::Dyn(TreeKind::Lit), Codes::Dyn(TreeKind::Dist))
        } else {
            (Codes::StaticLit, Codes::StaticDist)
        };
        let mut sx = 0usize;
        while sx < self.sym_next {
            let dist = u32::from(self.sym_buf[sx]) | (u32::from(self.sym_buf[sx + 1]) << 8);
            let mut lc = u32::from(self.sym_buf[sx + 2]);
            sx += 3;
            if dist == 0 {
                self.emit_code(ltree, lc as usize);
            } else {
                let code = usize::from(LENGTH_CODE[lc as usize]);
                self.emit_code(ltree, code + LITERALS + 1);
                let extra = i32::from(EXTRA_LBITS[code]);
                if extra != 0 {
                    lc -= u32::from(BASE_LENGTH[code]);
                    self.send_bits(lc, extra);
                }
                let mut dist = dist - 1;
                let dcode = d_code(dist);
                self.emit_code(dtree, dcode);
                let extra = i32::from(EXTRA_DBITS[dcode]);
                if extra != 0 {
                    dist -= u32::from(BASE_DIST[dcode]);
                    self.send_bits(dist, extra);
                }
            }
        }
        self.emit_code(ltree, END_BLOCK);
    }

    /// `_tr_flush_block` (`trees.c:997`): pick the cheapest encoding of the current block
    /// (stored, fixed-Huffman or dynamic-Huffman), emit it, and reset for the next block.
    ///
    /// `buf` is the window offset of the block's bytes (`None` when `block_start` has gone
    /// negative — the C's NULL buffer, which disables the stored form) and `stored_len`
    /// its length.
    pub fn flush_block(&mut self, buf: Option<usize>, stored_len: u64, last: bool) {
        let mut opt_lenb;
        let static_lenb;

        self.build_tree(TreeKind::Lit);
        self.build_tree(TreeKind::Dist);
        let max_blindex = self.build_bl_tree();

        // `opt_lenb = (s->opt_len+3+7)>>3;` — on the wrapped `ulg` totals (`trees.c:1027`).
        opt_lenb = self.opt_len.wrapping_add(3 + 7) >> 3;
        static_lenb = self.static_len.wrapping_add(3 + 7) >> 3;

        if static_lenb <= opt_lenb {
            opt_lenb = static_lenb;
        }

        if stored_len + 4 <= opt_lenb && buf.is_some() {
            self.stored_block(buf, stored_len, last);
        } else if static_lenb == opt_lenb {
            // `send_bits(s, (STATIC_TREES<<1)+last, 3)` — the block type sits one bit
            // above `BFINAL`, so the `<<1` is not decoration: `STATIC_TREES` alone would
            // clear `BFINAL` and leave the stream unterminated.
            self.send_bits((STATIC_TREES << 1) + u32::from(last), 3);
            self.compress_block(false);
        } else {
            self.send_bits((DYN_TREES << 1) + u32::from(last), 3);
            let (lcodes, dcodes) = (self.lit_max_code + 1, self.dist_max_code + 1);
            self.send_all_trees(lcodes, dcodes, max_blindex + 1);
            self.compress_block(true);
        }
        self.init_block();

        if last {
            self.bi_windup();
        }
    }

    /// `_tr_tally_lit` (`deflate.h:357`): record a literal byte; `true` when the symbol
    /// buffer is full and the block must be flushed.
    pub fn tr_tally_lit(&mut self, c: u8) -> bool {
        let i = self.sym_next;
        self.sym_buf[i] = 0;
        self.sym_buf[i + 1] = 0;
        self.sym_buf[i + 2] = c;
        self.sym_next += 3;
        self.dyn_ltree[usize::from(c)].freq += 1;
        self.sym_next == self.sym_end
    }

    /// `_tr_tally_dist` (`deflate.h:365`): record a match of `length` (3..=258) at
    /// `distance`; `true` when the symbol buffer is full.
    pub fn tr_tally_dist(&mut self, distance: u32, length: u32) -> bool {
        let i = self.sym_next;
        self.sym_buf[i] = (distance & 0xff) as u8;
        self.sym_buf[i + 1] = ((distance >> 8) & 0xff) as u8;
        self.sym_buf[i + 2] = length as u8;
        self.sym_next += 3;
        let dist = distance - 1;
        self.dyn_ltree[usize::from(LENGTH_CODE[length as usize]) + LITERALS + 1].freq += 1;
        self.dyn_dtree[d_code(dist)].freq += 1;
        self.sym_next == self.sym_end
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::{LENGTH_CODES, MAX_MATCH, MIN_MATCH};
    use super::*;

    /// The generated tables must be RFC 1951's: every match length and every distance has
    /// to fall inside the `base`/`extra_bits` bracket of the code the table maps it to.
    /// One wrong row changes the bits of every match that reaches it — and, because
    /// `sym_buf` stores `length - MIN_MATCH` and `distance - 1`, the indices are easy to
    /// get off by one.
    ///
    /// A code with no extra bits sends nothing but itself, so its `base` is the whole
    /// bracket; the reserved code 285 (`_length_code`'s last entry, `extra_lbits` 0) is the
    /// one row whose unused `base_length` is 0, and the C never reads it.
    #[test]
    fn the_length_and_distance_tables_cover_their_whole_ranges() {
        let last_length_code = LENGTH_CODES - 1;
        for len in MIN_MATCH..=MAX_MATCH {
            let value = len - MIN_MATCH;
            let code = usize::from(LENGTH_CODE[value as usize]);
            assert!(code < LENGTH_CODES, "length {len} -> code {code}");
            let (base, extra) = (u32::from(BASE_LENGTH[code]), EXTRA_LBITS[code]);
            if extra == 0 {
                assert!(
                    base == value || code == last_length_code,
                    "length {len} -> code {code} with no extra bits, but base {base}"
                );
                continue;
            }
            let span = 1u32 << extra;
            assert!(
                (base..base + span).contains(&value),
                "length {len} -> code {code}, which covers {base}..{}",
                base + span
            );
        }
        assert_eq!(
            usize::from(LENGTH_CODE[(MAX_MATCH - MIN_MATCH) as usize]),
            last_length_code,
            "MAX_MATCH is the reserved code"
        );

        for dist in 0..=32767u32 {
            let code = d_code(dist);
            let (base, extra) = (u32::from(BASE_DIST[code]), EXTRA_DBITS[code]);
            let span = 1u32 << extra;
            assert!(
                (base..base + span).contains(&dist),
                "distance-1 {dist} -> code {code}, which covers {base}..{}",
                base + span
            );
        }
    }
}
