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

//! The `deflate` stream state: the sliding window, the hash chains, the symbol
//! buffer, the Huffman-tree scratch space and the bit-level output sink.
//!
//! One-to-one with `deflate_state` in `../../erts/emulator/zlib/deflate.h`, with two
//! deliberate substitutions that cannot change a single output bit:
//!
//! * **There is no `pending_buf`.** The C emits into a fixed 4×`lit_bufsize` buffer and
//!   `flush_pending`s it to the caller's output buffer. Here the bytes go straight into
//!   [`DeflateState::out`], because the one-shot entry point never runs out of output
//!   space — the C's own `compress2` behaves the same way (`avail_out` never reaches 0,
//!   so `deflate` never returns `need_more`). The bit-level writer ([`send_bits`],
//!   [`put_short`], [`bi_windup`]) is otherwise the C's, bit for bit.
//! * **`sym_buf` is its own buffer** rather than an overlay of `pending_buf` — the
//!   overlay is a C memory optimisation (see the long comment at `deflate.c:466-503`)
//!   with no effect on the emitted stream.
//!
//! Everything else — every counter, every pointer-named field, the sliding-window
//! bookkeeping (`strstart`/`lookahead`/`insert`/`high_water`/`block_start`), the hash
//! chain discipline and the `uInt`-wrapping quirks of `match_start` — is reproduced
//! literally, because the compressed bytes depend on all of it.
//!
//! [`send_bits`]: DeflateState::send_bits
//! [`put_short`]: DeflateState::put_short
//! [`bi_windup`]: DeflateState::bi_windup


use super::tables::{EXTRA_BLBITS, EXTRA_DBITS, EXTRA_LBITS, STATIC_DTREE, STATIC_LTREE};
/// `MIN_MATCH` — the shortest LZ77 match (`zutil.h:92`).
pub(super) const MIN_MATCH: u32 = 3;
/// `MAX_MATCH` — the longest LZ77 match (`zutil.h:93`).
pub(super) const MAX_MATCH: u32 = 258;
/// `MIN_LOOKAHEAD` = `MAX_MATCH + MIN_MATCH + 1` (`deflate.h:296`).
pub(super) const MIN_LOOKAHEAD: u32 = MAX_MATCH + MIN_MATCH + 1;
/// `WIN_INIT` — bytes zeroed past the current data so `longest_match`'s over-read is
/// deterministic (`deflate.h:306`).
pub(super) const WIN_INIT: usize = MAX_MATCH as usize;

/// `w_bits` for every entry point here: `deflateInit2_(…, MAX_WBITS = 15, …)`
/// (`deflate.c:381`), the window the six `zlib` convenience functions use.
pub(super) const W_BITS: u32 = 15;
/// `w_size = 1 << w_bits`.
pub(super) const W_SIZE: u32 = 1 << W_BITS;
/// `w_mask = w_size - 1`.
pub(super) const W_MASK: u32 = W_SIZE - 1;
/// `window_size = 2 * w_size` (`lm_init`, `deflate.c:683`).
pub(super) const WINDOW_SIZE: u32 = 2 * W_SIZE;
/// `MAX_DIST(s) = w_size - MIN_LOOKAHEAD` (`deflate.h:301`).
pub(super) const MAX_DIST: u32 = W_SIZE - MIN_LOOKAHEAD;

/// `hash_bits = mem_level + 7` with `DEF_MEM_LEVEL = 8` (`deflate.c:453`).
pub(super) const HASH_BITS: u32 = 8 + 7;
/// `hash_size = 1 << hash_bits`.
pub(super) const HASH_SIZE: usize = 1 << HASH_BITS;
/// `hash_mask = hash_size - 1`.
pub(super) const HASH_MASK: u32 = HASH_SIZE as u32 - 1;
/// `hash_shift = (hash_bits + MIN_MATCH - 1) / MIN_MATCH` (`deflate.c:456`).
pub(super) const HASH_SHIFT: u32 = (HASH_BITS + MIN_MATCH - 1) / MIN_MATCH;

/// `lit_bufsize = 1 << (mem_level + 6)` = 16384 symbols (`deflate.c:464`).
pub(super) const LIT_BUFSIZE: usize = 1 << (8 + 6);
/// `sym_end = (lit_bufsize - 1) * 3` (`deflate.c:521`) — three bytes per symbol.
pub(super) const SYM_END: usize = (LIT_BUFSIZE - 1) * 3;

/// `LENGTH_CODES` — length codes, excluding `END_BLOCK` (`deflate.h:34`).
pub(super) const LENGTH_CODES: usize = 29;
/// `LITERALS` (`deflate.h:37`).
pub(super) const LITERALS: usize = 256;
/// `L_CODES = LITERALS + 1 + LENGTH_CODES` (`deflate.h:40`).
pub(super) const L_CODES: usize = LITERALS + 1 + LENGTH_CODES;
/// `D_CODES` (`deflate.h:43`).
pub(super) const D_CODES: usize = 30;
/// `BL_CODES` (`deflate.h:46`).
pub(super) const BL_CODES: usize = 19;
/// `HEAP_SIZE = 2 * L_CODES + 1` (`deflate.h:49`) — the shared heap/depth size, used
/// for all three trees exactly as the C does.
pub(super) const HEAP_SIZE: usize = 2 * L_CODES + 1;
/// `MAX_BITS` (`deflate.h:52`).
pub(super) const MAX_BITS: usize = 15;
/// `MAX_BL_BITS` (`trees.c:47`).
pub(super) const MAX_BL_BITS: usize = 7;
/// `END_BLOCK` (`trees.c:50`).
pub(super) const END_BLOCK: usize = 256;
/// `REP_3_6` (`trees.c:53`).
pub(super) const REP_3_6: usize = 16;
/// `REPZ_3_10` (`trees.c:56`).
pub(super) const REPZ_3_10: usize = 17;
/// `REPZ_11_138` (`trees.c:59`).
pub(super) const REPZ_11_138: usize = 18;
/// `TOO_FAR` (`deflate.c:89`) — a length-3 match farther than this is discarded.
pub(super) const TOO_FAR: u32 = 4096;
/// `Buf_size` — the bit buffer's width (`deflate.h:55`).
const BUF_SIZE: i32 = 16;
/// `STORED_BLOCK` (`zutil.h:87`).
pub(super) const STORED_BLOCK: u32 = 0;
/// `STATIC_TREES` (`zutil.h:88`).
pub(super) const STATIC_TREES: u32 = 1;
/// `DYN_TREES` (`zutil.h:89`).
pub(super) const DYN_TREES: u32 = 2;
/// `NIL` — the tail of every hash chain (`deflate.c:85`).
pub(super) const NIL: u32 = 0;

/// One `ct_data` (`deflate.h:72-81`). The C overlays `code` on `freq` and `len` on
/// `dad` in a union; keeping them as separate fields is a superset of the information
/// and is never observable — the C only relies on the overlay to *reuse* storage, never
/// to observe a value through the other member.
#[derive(Clone, Copy, Default)]
pub(super) struct CtData {
    /// `Freq` — the frequency count.
    pub freq: u16,
    /// `Code` — the reversed bit string.
    pub code: u16,
    /// `Dad` — the father node in the Huffman tree.
    pub dad: u16,
    /// `Len` — the bit length.
    pub len: u16,
}

/// Which of the three trees a tree routine is working on (`l_desc`/`d_desc`/`bl_desc`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TreeKind {
    /// `dyn_ltree` — the literal/length tree.
    Lit,
    /// `dyn_dtree` — the distance tree.
    Dist,
    /// `bl_tree` — the tree of the two trees' bit lengths.
    Bl,
}

impl TreeKind {
    /// `desc->stat_desc->elems` — the number of leaf slots in the tree.
    pub(super) fn elems(self) -> usize {
        match self {
            TreeKind::Lit => L_CODES,
            TreeKind::Dist => D_CODES,
            TreeKind::Bl => BL_CODES,
        }
    }

    /// `desc->stat_desc->static_tree` — the matching static tree, if it has one.
    pub(super) fn static_tree(self) -> Option<&'static [(u16, u16)]> {
        match self {
            TreeKind::Lit => Some(&STATIC_LTREE),
            TreeKind::Dist => Some(&STATIC_DTREE),
            TreeKind::Bl => None,
        }
    }

    /// `desc->stat_desc->extra_bits`.
    pub(super) fn extra_bits(self) -> &'static [u8] {
        match self {
            TreeKind::Lit => &EXTRA_LBITS,
            TreeKind::Dist => &EXTRA_DBITS,
            TreeKind::Bl => &EXTRA_BLBITS,
        }
    }

    /// `desc->stat_desc->extra_base`.
    pub(super) fn extra_base(self) -> usize {
        match self {
            TreeKind::Lit => LITERALS + 1,
            TreeKind::Dist | TreeKind::Bl => 0,
        }
    }

    /// `desc->stat_desc->max_length`.
    pub(super) fn max_length(self) -> i32 {
        match self {
            TreeKind::Lit | TreeKind::Dist => MAX_BITS as i32,
            TreeKind::Bl => MAX_BL_BITS as i32,
        }
    }
}

/// The level-6 configuration row (`configuration_table[6]`, `deflate.c:121`):
/// `{good, lazy, nice, chain} = {8, 16, 128, 128}` with `deflate_slow`.
///
/// Level 6 is zlib's `Z_DEFAULT_COMPRESSION` — the only level the six `zlib`
/// convenience functions use (`deflateInit(Z, default)`).
pub(super) const CONFIG_GOOD_LENGTH: u32 = 8;
/// `configuration_table[6].max_lazy`.
pub(super) const CONFIG_MAX_LAZY: u32 = 16;
/// `configuration_table[6].nice_length`.
pub(super) const CONFIG_NICE_LENGTH: i32 = 128;
/// `configuration_table[6].max_chain`.
pub(super) const CONFIG_MAX_CHAIN: u32 = 128;

/// The mutable state of one `deflate` run — `deflate_state` (`deflate.h:104-288`).
pub(super) struct DeflateState<'a> {
    /// The whole input, indexed by [`Self::in_pos`] (the C's `strm->next_in` always
    /// points at all of it, since the one-shot entry point hands over the entire body).
    input: &'a [u8],
    /// `strm->avail_in` is `input.len() - in_pos`.
    in_pos: usize,
    /// The emitted stream (`strm->next_out`).
    pub out: Vec<u8>,

    /// `window` — the sliding LZ77 window, `2 * w_size` bytes.
    pub window: Vec<u8>,
    /// `prev` — the hash chains, indexed by `str & w_mask`.
    pub prev: Vec<u16>,
    /// `head` — the heads of the hash chains.
    pub head: Vec<u16>,
    /// `ins_h` — the running hash of the string being inserted.
    pub ins_h: u32,

    /// `block_start` — window position of the current block's first byte, **signed**
    /// (it goes negative when the window slides).
    pub block_start: i64,
    /// `match_length`.
    pub match_length: u32,
    /// `prev_match`.
    pub prev_match: u32,
    /// `match_available`.
    pub match_available: bool,
    /// `strstart`.
    pub strstart: u32,
    /// `match_start`.
    pub match_start: u32,
    /// `lookahead`.
    pub lookahead: u32,
    /// `prev_length`.
    pub prev_length: u32,
    /// `insert` — bytes at the end of the window still to be inserted in the hash.
    pub insert: u32,

    /// `max_chain_length`.
    pub max_chain_length: u32,
    /// `max_lazy_match`.
    pub max_lazy_match: u32,
    /// `good_match`.
    pub good_match: u32,
    /// `nice_match`.
    pub nice_match: i32,

    /// `dyn_ltree`.
    pub dyn_ltree: Vec<CtData>,
    /// `dyn_dtree`.
    pub dyn_dtree: Vec<CtData>,
    /// `bl_tree`.
    pub bl_tree: Vec<CtData>,
    /// `l_desc.max_code` / `d_desc.max_code` / `bl_desc.max_code`.
    pub lit_max_code: i32,
    /// `d_desc.max_code`.
    pub dist_max_code: i32,
    /// `bl_desc.max_code`.
    pub bl_max_code: i32,
    /// `bl_count`.
    pub bl_count: [u16; MAX_BITS + 1],
    /// `heap`.
    pub heap: [i32; HEAP_SIZE],
    /// `heap_len`.
    pub heap_len: i32,
    /// `heap_max`.
    pub heap_max: i32,
    /// `depth`.
    pub depth: [u8; HEAP_SIZE],

    /// `sym_buf` — three bytes per symbol: distance (LE) then literal/length.
    pub sym_buf: Vec<u8>,
    /// `sym_next` — the running index into [`Self::sym_buf`].
    pub sym_next: usize,
    /// `sym_end`.
    pub sym_end: usize,
    /// `opt_len` — the block's bit length with the optimal trees.
    pub opt_len: u64,
    /// `static_len` — the block's bit length with the static trees.
    pub static_len: u64,

    /// `bi_buf` — the bit accumulator, filled from the least significant bit.
    bi_buf: u16,
    /// `bi_valid` — how many low bits of [`Self::bi_buf`] are live.
    bi_valid: i32,
    /// `high_water` — how far into `window` bytes are known initialized.
    high_water: usize,
}

impl<'a> DeflateState<'a> {
    /// `deflateInit2_` + `deflateReset`/`lm_init` for `level` (`deflate.c:387-533`,
    /// `:682-701`): every buffer sized and zeroed exactly as the C's `ZALLOC` +
    /// `CLEAR_HASH`/`zmemzero` sequence leaves them.
    ///
    /// The caller must have validated that `level` is 6 — the only configuration row
    /// [`CONFIG_GOOD_LENGTH`] … [`CONFIG_MAX_CHAIN`] describe.
    pub fn new(input: &'a [u8]) -> Self {
        let mut st = DeflateState {
            input,
            in_pos: 0,
            out: Vec::new(),
            window: vec![0; WINDOW_SIZE as usize],
            prev: vec![0; W_SIZE as usize],
            head: vec![0; HASH_SIZE],
            ins_h: 0,
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            strstart: 0,
            match_start: 0,
            lookahead: 0,
            prev_length: MIN_MATCH - 1,
            insert: 0,
            max_chain_length: CONFIG_MAX_CHAIN,
            max_lazy_match: CONFIG_MAX_LAZY,
            good_match: CONFIG_GOOD_LENGTH,
            nice_match: CONFIG_NICE_LENGTH,
            dyn_ltree: vec![CtData::default(); HEAP_SIZE],
            dyn_dtree: vec![CtData::default(); 2 * D_CODES + 1],
            bl_tree: vec![CtData::default(); 2 * BL_CODES + 1],
            lit_max_code: 0,
            dist_max_code: 0,
            bl_max_code: 0,
            bl_count: [0; MAX_BITS + 1],
            heap: [0; HEAP_SIZE],
            heap_len: 0,
            heap_max: 0,
            depth: [0; HEAP_SIZE],
            sym_buf: vec![0; LIT_BUFSIZE * 3],
            sym_next: 0,
            sym_end: SYM_END,
            opt_len: 0,
            static_len: 0,
            bi_buf: 0,
            bi_valid: 0,
            high_water: 0,
        };
        // `CLEAR_HASH(s)` (`deflate.c:170`) then `init_block(s)` (`trees.c:440`), the
        // two pieces of `deflateReset`'s `lm_init` + `_tr_init`.
        st.head.iter_mut().for_each(|h| *h = NIL as u16);
        st.init_block();
        st
    }

    /// The tree a [`TreeKind`] names.
    pub fn tree(&self, kind: TreeKind) -> &[CtData] {
        match kind {
            TreeKind::Lit => &self.dyn_ltree,
            TreeKind::Dist => &self.dyn_dtree,
            TreeKind::Bl => &self.bl_tree,
        }
    }

    /// The tree a [`TreeKind`] names, mutably.
    pub fn tree_mut(&mut self, kind: TreeKind) -> &mut [CtData] {
        match kind {
            TreeKind::Lit => &mut self.dyn_ltree,
            TreeKind::Dist => &mut self.dyn_dtree,
            TreeKind::Bl => &mut self.bl_tree,
        }
    }

    /// `desc->max_code` for `kind`.
    pub fn max_code(&self, kind: TreeKind) -> i32 {
        match kind {
            TreeKind::Lit => self.lit_max_code,
            TreeKind::Dist => self.dist_max_code,
            TreeKind::Bl => self.bl_max_code,
        }
    }

    /// Set `desc->max_code` for `kind`.
    pub fn set_max_code(&mut self, kind: TreeKind, max_code: i32) {
        match kind {
            TreeKind::Lit => self.lit_max_code = max_code,
            TreeKind::Dist => self.dist_max_code = max_code,
            TreeKind::Bl => self.bl_max_code = max_code,
        }
    }

    /// `read_buf(s->strm, buf, size)` (`deflate.c:219`): copy up to `size` bytes of the
    /// remaining input into `window[at..]` and report how many were copied. The Adler-32
    /// the C accumulates here is computed once over the whole body by the caller — the
    /// checksum is over the same bytes either way.
    fn read_buf(&mut self, at: usize, size: usize) -> usize {
        let avail = self.input.len() - self.in_pos;
        let len = avail.min(size);
        if len == 0 {
            return 0;
        }
        self.window[at..at + len].copy_from_slice(&self.input[self.in_pos..self.in_pos + len]);
        self.in_pos += len;
        len
    }

    /// `fill_window` (`deflate.c:252-376`): top the window up to `MIN_LOOKAHEAD` bytes
    /// of lookahead, sliding it down by `w_size` when it fills.
    pub fn fill_window(&mut self) {
        let wsize = W_SIZE;
        loop {
            let mut more = (WINDOW_SIZE - self.lookahead - self.strstart) as usize;

            if self.strstart >= wsize + MAX_DIST {
                // `zmemcpy(window, window + wsize, wsize - more)`: slide the upper half
                // down, keeping the `wsize` bytes that are not still empty.
                self.window.copy_within(wsize as usize..wsize as usize + (wsize as usize - more), 0);
                self.match_start = self.match_start.wrapping_sub(wsize);
                self.strstart -= wsize;
                self.block_start -= i64::from(wsize);
                if self.insert > self.strstart {
                    self.insert = self.strstart;
                }
                self.slide_hash();
                more += wsize as usize;
            }
            if self.in_pos == self.input.len() {
                break;
            }

            let at = (self.strstart + self.lookahead) as usize;
            let n = self.read_buf(at, more);
            self.lookahead += n as u32;

            // Initialize the hash value now that we have some input.
            if self.lookahead + self.insert >= MIN_MATCH {
                let mut str = self.strstart - self.insert;
                self.ins_h = u32::from(self.window[str as usize]);
                self.update_hash(self.window[str as usize + 1]);
                while self.insert != 0 {
                    let b = self.window[str as usize + MIN_MATCH as usize - 1];
                    self.update_hash(b);
                    let slot = (str & W_MASK) as usize;
                    self.prev[slot] = self.head[(self.ins_h & HASH_MASK) as usize];
                    self.head[(self.ins_h & HASH_MASK) as usize] = str as u16;
                    str += 1;
                    self.insert -= 1;
                    if self.lookahead + self.insert < MIN_MATCH {
                        break;
                    }
                }
            }

            if !(self.lookahead < MIN_LOOKAHEAD && self.in_pos != self.input.len()) {
                break;
            }
        }

        // Zero the WIN_INIT bytes past the end of the data so `longest_match`'s over-read
        // is deterministic (`deflate.c:347-372`).
        if self.high_water < WINDOW_SIZE as usize {
            let curr = (self.strstart + self.lookahead) as usize;
            if self.high_water < curr {
                let init = (WINDOW_SIZE as usize - curr).min(WIN_INIT);
                self.window[curr..curr + init].fill(0);
                self.high_water = curr + init;
            } else if self.high_water < curr + WIN_INIT {
                let init = (curr + WIN_INIT - self.high_water).min(WINDOW_SIZE as usize - self.high_water);
                let from = self.high_water;
                self.window[from..from + init].fill(0);
                self.high_water += init;
            }
        }
    }

    /// `UPDATE_HASH(s, h, c)` (`deflate.c:141`).
    pub fn update_hash(&mut self, c: u8) {
        self.ins_h = ((self.ins_h << HASH_SHIFT) ^ u32::from(c)) & HASH_MASK;
    }

    /// `slide_hash` (`deflate.c:187-210`): every hash-chain entry moves down by
    /// `w_size`, and anything that would fall off the bottom becomes `NIL`.
    fn slide_hash(&mut self) {
        let wsize = W_SIZE as u16;
        for h in self.head.iter_mut() {
            *h = if *h >= wsize { *h - wsize } else { NIL as u16 };
        }
        for p in self.prev.iter_mut() {
            *p = if *p >= wsize { *p - wsize } else { NIL as u16 };
        }
    }

    /// `INSERT_STRING(s, str, match_head)` (`deflate.c:160`) — returns `match_head`, the
    /// previous head of the chain `str` hashes into.
    pub fn insert_string(&mut self, str: u32) -> u32 {
        let b = self.window[str as usize + MIN_MATCH as usize - 1];
        self.update_hash(b);
        let h = (self.ins_h & HASH_MASK) as usize;
        let slot = (str & W_MASK) as usize;
        let match_head = self.head[h];
        self.prev[slot] = match_head;
        self.head[h] = str as u16;
        u32::from(match_head)
    }

    /// `put_short(s, w)` (`trees.c:144`): a 16-bit value, least significant byte first.
    pub fn put_short(&mut self, w: u16) {
        self.out.push((w & 0xff) as u8);
        self.out.push((w >> 8) as u8);
    }

    /// `send_bits(s, value, length)` (`trees.c:274`): append `length` bits of `value`
    /// (least significant first) to the bit buffer, flushing whole 16-bit words as the
    /// buffer fills. The `u32` shift reproduces the C's `int` promotion, where a shift by
    /// a full 16 contributes nothing to the `ush` accumulator.
    pub fn send_bits(&mut self, value: u32, length: i32) {
        debug_assert!(length > 0 && length <= 15);
        if self.bi_valid > BUF_SIZE - length {
            self.bi_buf |= ((value << self.bi_valid) & 0xffff) as u16;
            let buf = self.bi_buf;
            self.put_short(buf);
            self.bi_buf = (value >> (BUF_SIZE - self.bi_valid)) as u16;
            self.bi_valid += length - BUF_SIZE;
        } else {
            self.bi_buf |= ((value << self.bi_valid) & 0xffff) as u16;
            self.bi_valid += length;
        }
    }


    /// `bi_windup` (`trees.c:181`): flush the bit buffer and align on a byte boundary.
    pub fn bi_windup(&mut self) {
        if self.bi_valid > 8 {
            let buf = self.bi_buf;
            self.put_short(buf);
        } else if self.bi_valid > 0 {
            self.out.push(self.bi_buf as u8);
        }
        self.bi_buf = 0;
        self.bi_valid = 0;
    }

    /// `_tr_stored_block` (`trees.c:860`): a stored (uncompressed) deflate block.
    ///
    /// `buf` is the window offset of the block's bytes (`None` after a slide, when the C
    /// passes a NULL buffer and the stored form cannot be used), and `stored_len` its
    /// length.
    pub fn stored_block(&mut self, buf: Option<usize>, stored_len: u64, last: bool) {
        self.send_bits(STORED_BLOCK + u32::from(last), 3);
        self.bi_windup();
        self.put_short(stored_len as u16);
        self.put_short(!(stored_len as u16));
        // The stored bytes are the window range `[block_start, block_start + len)`; a
        // `None` buffer therefore cannot be copied, and the C never asks for it.
        if let Some(from) = buf {
            let len = stored_len as usize;
            let bytes = self.window[from..from + len].to_vec();
            self.out.extend_from_slice(&bytes);
        }
    }
}
