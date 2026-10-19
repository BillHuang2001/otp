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
//! The `deflate` stream state — `deflate_state` of `../../../erts/emulator/zlib/deflate.h`
//! (zlib 1.3.2), plus every constant the compressor reads.
//!
//! One-to-one with the C struct, with these deliberate substitutions, none of which can
//! change an emitted bit:
//!
//! * **`sym_buf` is its own buffer.** The C overlays it on `pending_buf` (a memory
//!   optimisation analysed in the long comment at `deflate.c:466-503`); the bytes it holds
//!   are the same either way.
//! * **`pending_buf` is a growable `Vec`.** The C indexes a fixed `pending_buf_size`
//!   allocation. `pending_buf_size` is *still* computed and *still* drives
//!   `deflate_stored`'s block sizing (it is observable there), but the storage itself may
//!   grow, so an invariant slip can never write out of bounds.
//! * **`ct_data.freq`/`code` and `dad`/`len` are four separate fields.** The C overlays
//!   them in unions purely to reuse storage. The overlay is never observed through the
//!   other member: `init_block` zeroes every `freq` at the start of each block, so a node
//!   with `Len == 0` always has `Code == 0` in the C — [`DeflateState::send_bits`]
//!   therefore ignores a zero-length code, which is exactly the C's `bi_buf |= 0 << n`.
//! * **`slid` is gone.** Only `deflateCopy` (not ported) reads it.
//!
//! Everything else — every counter, the sliding-window bookkeeping
//! (`strstart`/`lookahead`/`insert`/`high_water`/`block_start`), the hash-chain discipline
//! and the `uInt`-wrapping quirks — is reproduced literally, because the compressed bytes
//! depend on all of it.

use super::tables::{EXTRA_BLBITS, EXTRA_DBITS, EXTRA_LBITS, STATIC_DTREE, STATIC_LTREE};

/// `MIN_MATCH` — the shortest LZ77 match (`zutil.h:92`).
pub(super) const MIN_MATCH: u32 = 3;
/// `MAX_MATCH` — the longest LZ77 match (`zutil.h:93`).
pub(super) const MAX_MATCH: u32 = 258;
/// `MIN_LOOKAHEAD` = `MAX_MATCH + MIN_MATCH + 1` (`deflate.h:296`).
pub(super) const MIN_LOOKAHEAD: u32 = MAX_MATCH + MIN_MATCH + 1;
/// `WIN_INIT` — bytes zeroed past the current data so `longest_match`'s over-read is
/// deterministic (`deflate.h:306`).
pub(super) const WIN_INIT: u64 = MAX_MATCH as u64;
/// `TOO_FAR` (`deflate.c:89`) — a length-3 match farther than this is discarded.
pub(super) const TOO_FAR: u32 = 4096;

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
/// `Buf_size` — the bit buffer's width (`deflate.h:55`).
pub(super) const BUF_SIZE: i32 = 16;
/// `END_BLOCK` (`trees.c:50`).
pub(super) const END_BLOCK: usize = 256;
/// `REP_3_6` (`trees.c:53`).
pub(super) const REP_3_6: usize = 16;
/// `REPZ_3_10` (`trees.c:56`).
pub(super) const REPZ_3_10: usize = 17;
/// `REPZ_11_138` (`trees.c:59`).
pub(super) const REPZ_11_138: usize = 18;

/// `STORED_BLOCK` (`zutil.h:87`).
pub(super) const STORED_BLOCK: u32 = 0;
/// `STATIC_TREES` (`zutil.h:88`).
pub(super) const STATIC_TREES: u32 = 1;
/// `DYN_TREES` (`zutil.h:89`).
pub(super) const DYN_TREES: u32 = 2;

/// `NIL` — the tail of every hash chain (`deflate.c:85`).
pub(super) const NIL: u32 = 0;
/// `PRESET_DICT` — the zlib header's preset-dictionary flag (`zutil.h:96`).
pub(super) const PRESET_DICT: u32 = 0x20;
/// `OS_CODE` for Unix (`zutil.h:188`) — the gzip header's operating-system byte.
pub(super) const OS_CODE: u8 = 3;

/// `INIT_STATE` (`deflate.h:58`).
pub(super) const INIT_STATE: i32 = 42;
/// `GZIP_STATE` (`deflate.h:59`).
pub(super) const GZIP_STATE: i32 = 57;
/// `EXTRA_STATE` (`deflate.h:61`).
pub(super) const EXTRA_STATE: i32 = 69;
/// `NAME_STATE` (`deflate.h:62`).
pub(super) const NAME_STATE: i32 = 73;
/// `COMMENT_STATE` (`deflate.h:63`).
pub(super) const COMMENT_STATE: i32 = 91;
/// `HCRC_STATE` (`deflate.h:64`).
pub(super) const HCRC_STATE: i32 = 103;
/// `BUSY_STATE` (`deflate.h:66`).
pub(super) const BUSY_STATE: i32 = 113;
/// `FINISH_STATE` (`deflate.h:67`).
pub(super) const FINISH_STATE: i32 = 666;

/// `Z_NO_FLUSH`.
pub(super) const Z_NO_FLUSH: i32 = 0;
/// `Z_PARTIAL_FLUSH`.
pub(super) const Z_PARTIAL_FLUSH: i32 = 1;
/// `Z_FULL_FLUSH`.
pub(super) const Z_FULL_FLUSH: i32 = 3;
/// `Z_FINISH`.
pub(super) const Z_FINISH: i32 = 4;
/// `Z_BLOCK`.
pub(super) const Z_BLOCK: i32 = 5;
/// `Z_DEFAULT_STRATEGY` (`zlib.h:204`).
pub(super) const Z_DEFAULT_STRATEGY: i32 = 0;
/// `Z_FILTERED` (`zlib.h:205`).
pub(super) const Z_FILTERED: i32 = 1;
/// `Z_HUFFMAN_ONLY` (`zlib.h:206`).
pub(super) const Z_HUFFMAN_ONLY: i32 = 2;
/// `Z_RLE` (`zlib.h:207`).
pub(super) const Z_RLE: i32 = 3;
/// `Z_FIXED` (`zlib.h:208`).
pub(super) const Z_FIXED: i32 = 4;
/// `Z_BINARY` (`zlib.h:207`) — `detect_data_type`'s answer.
pub(super) const Z_BINARY: i32 = 0;
/// `Z_TEXT` (`zlib.h:208`).
pub(super) const Z_TEXT: i32 = 1;
/// `Z_UNKNOWN` (`zlib.h:210`) — `data_type` before `detect_data_type` runs.
pub(super) const Z_UNKNOWN: i32 = 2;
/// `Z_DEFLATED` (`zlib.h:200`) — the only compression method zlib emits.
pub(super) const Z_DEFLATED: u32 = 8;

/// `configuration_table[level].func` — which strategy function a level selects
/// (`deflate.c:112-125`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum StrategyFn {
    /// `deflate_stored`.
    Stored,
    /// `deflate_fast`.
    Fast,
    /// `deflate_slow`.
    Slow,
}

/// One row of `configuration_table` (`deflate.c:98-104`).
#[derive(Clone, Copy, Debug)]
pub(super) struct Config {
    /// `good_length` — reduce the lazy search above this match length.
    pub good_length: u32,
    /// `max_lazy` — do not perform the lazy search above this match length.
    pub max_lazy: u32,
    /// `nice_length` — quit the search above this match length.
    pub nice_length: i32,
    /// `max_chain` — the hash-chain walk limit.
    pub max_chain: u32,
    /// The strategy function.
    pub func: StrategyFn,
}

/// `configuration_table[10]` (`deflate.c:112-125`), transcribed row for row.
pub(super) const CONFIGURATION_TABLE: [Config; 10] = [
    //  good lazy nice chain
    Config { good_length: 0, max_lazy: 0, nice_length: 0, max_chain: 0, func: StrategyFn::Stored },
    Config { good_length: 4, max_lazy: 4, nice_length: 8, max_chain: 4, func: StrategyFn::Fast },
    Config { good_length: 4, max_lazy: 5, nice_length: 16, max_chain: 8, func: StrategyFn::Fast },
    Config { good_length: 4, max_lazy: 6, nice_length: 32, max_chain: 32, func: StrategyFn::Fast },
    Config { good_length: 4, max_lazy: 4, nice_length: 16, max_chain: 16, func: StrategyFn::Slow },
    Config { good_length: 8, max_lazy: 16, nice_length: 32, max_chain: 32, func: StrategyFn::Slow },
    Config { good_length: 8, max_lazy: 16, nice_length: 128, max_chain: 128, func: StrategyFn::Slow },
    Config { good_length: 8, max_lazy: 32, nice_length: 128, max_chain: 256, func: StrategyFn::Slow },
    Config { good_length: 32, max_lazy: 128, nice_length: 258, max_chain: 1024, func: StrategyFn::Slow },
    Config { good_length: 32, max_lazy: 258, nice_length: 258, max_chain: 4096, func: StrategyFn::Slow },
];

/// One `ct_data` (`deflate.h:72-81`), with the C's two unions kept as separate fields
/// (see the module docs for why that is not observable).
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

/// The mutable state of one `deflate` run — `deflate_state` (`deflate.h:104-288`) plus the
/// `z_stream` fields the compressor itself reads (`adler`, `total_in`, `data_type`).
pub(super) struct DeflateState {
    // ----- configuration (`deflate.c:387-464`) -----
    /// `s->status` — one of the `*_STATE` constants.
    pub status: i32,
    /// `s->wrap` — 0 raw, 1 zlib, 2 gzip; negative once the trailer has been written.
    pub wrap: i32,
    /// `s->level`.
    pub level: i32,
    /// `s->strategy`.
    pub strategy: i32,
    /// `s->w_bits` — 9..=15 after `deflateInit2_`'s 8 → 9 bump.
    pub w_bits: u32,
    /// `s->w_size = 1 << w_bits`.
    pub w_size: u32,
    /// `s->w_mask = w_size - 1`.
    pub w_mask: u32,
    /// `s->window_size` — `2 * w_size` once `lm_init` runs.
    pub window_size: u64,
    /// `s->hash_bits = mem_level + 7`.
    pub hash_bits: u32,
    /// `s->hash_size = 1 << hash_bits`.
    pub hash_size: usize,
    /// `s->hash_mask = hash_size - 1`.
    pub hash_mask: u32,
    /// `s->hash_shift = (hash_bits + MIN_MATCH - 1) / MIN_MATCH`.
    pub hash_shift: u32,
    /// `s->lit_bufsize = 1 << (mem_level + 6)`.
    pub lit_bufsize: usize,
    /// `s->pending_buf_size = lit_bufsize * 4`.
    pub pending_buf_size: usize,

    // ----- output (`deflate.h:107-115`) -----
    /// `s->pending_buf` — output still pending. Live bytes are
    /// `[pending_out, pending_out + pending)`; compression only ever appends while
    /// `pending_out == 0` (the C relies on the same invariant).
    pub pending_buf: Vec<u8>,
    /// `s->pending` — bytes in the pending buffer.
    pub pending: usize,
    /// `s->pending_out` — next pending byte to hand to the caller.
    pub pending_out: usize,
    /// `s->last_flush` — the flush mode of the previous `deflate()` call.
    pub last_flush: i32,
    /// `strm->adler` — the running Adler-32 (zlib) or CRC-32 (gzip).
    pub adler: u32,
    /// `strm->total_in`.
    pub total_in: u64,
    /// `strm->total_out`.
    pub total_out: u64,
    /// `strm->data_type` — `Z_UNKNOWN` until `_tr_flush_block` detects it.
    pub data_type: i32,

    // ----- deflate.c -----
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
    /// `matches` — `_tr_tally`'s match counter; `deflate_stored` reuses it to remember how
    /// many hash slides a later switch to a compressing level owes (`deflate.c:1656-1664`).
    pub matches: u32,
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

    // ----- trees.c -----
    /// `dyn_ltree`.
    pub dyn_ltree: Vec<CtData>,
    /// `dyn_dtree`.
    pub dyn_dtree: Vec<CtData>,
    /// `bl_tree`.
    pub bl_tree: Vec<CtData>,
    /// `l_desc.max_code`.
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
    /// `sym_end` — `(lit_bufsize - 1) * 3`.
    pub sym_end: usize,
    /// `opt_len` — the block's bit length with the optimal trees.
    pub opt_len: u64,
    /// `static_len` — the block's bit length with the static trees.
    pub static_len: u64,
    /// `bi_buf` — the bit accumulator, filled from the least significant bit.
    pub bi_buf: u16,
    /// `bi_valid` — how many low bits of [`Self::bi_buf`] are live.
    pub bi_valid: i32,
    /// `bi_used` — the bits used at the last byte-boundary crossing (`deflateUsed`).
    pub bi_used: i32,
    /// `high_water` — how far into `window` bytes are known initialized.
    pub high_water: u64,

    // ----- gzip header -----
    /// `s->gzhead` — the user-supplied gzip member header, if any.
    pub gzhead: Option<super::GzHeader>,
    /// `s->gzindex` — where in `extra`/`name`/`comment` the header writer is.
    pub gzindex: usize,
}

impl DeflateState {
    /// `deflateInit2_` + `deflateReset` (`deflate.c:387-533`, `:644-711`): every buffer
    /// sized and zeroed exactly as the C's `ZALLOC` + `CLEAR_HASH`/`zmemzero` sequence
    /// leaves them.
    ///
    /// The caller ([`super::Deflate::new`]) must have validated and normalized the
    /// parameters (`level` resolved to `0..=9`, `w_bits` to `9..=15`, `mem_level` to
    /// `1..=9`, `wrap` to `0..=2`).
    pub(super) fn new(
        level: i32,
        strategy: i32,
        wrap: i32,
        w_bits: u32,
        mem_level: u32,
    ) -> DeflateState {
        let w_size = 1u32 << w_bits;
        let hash_bits = mem_level + 7;
        let hash_size = 1usize << hash_bits;
        let lit_bufsize = 1usize << (mem_level + 6);
        let mut st = DeflateState {
            status: INIT_STATE,
            wrap,
            level,
            strategy,
            w_bits,
            w_size,
            w_mask: w_size - 1,
            window_size: 2 * u64::from(w_size),
            hash_bits,
            hash_size,
            hash_mask: hash_size as u32 - 1,
            hash_shift: (hash_bits + MIN_MATCH - 1) / MIN_MATCH,
            lit_bufsize,
            pending_buf_size: lit_bufsize * 4,
            pending_buf: vec![0; lit_bufsize * 4],
            pending: 0,
            pending_out: 0,
            last_flush: 0,
            adler: 1,
            total_in: 0,
            total_out: 0,
            data_type: Z_UNKNOWN,
            window: vec![0; 2 * w_size as usize],
            prev: vec![0; w_size as usize],
            head: vec![0; hash_size],
            ins_h: 0,
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            strstart: 0,
            match_start: 0,
            lookahead: 0,
            prev_length: MIN_MATCH - 1,
            matches: 0,
            insert: 0,
            max_chain_length: 0,
            max_lazy_match: 0,
            good_match: 0,
            nice_match: 0,
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
            // `sym_buf` overlays `pending_buf[lit_bufsize ..]` in the C, i.e. there are
            // `3 * lit_bufsize` bytes available to it; `sym_end` keeps `sym_next` below
            // that.
            sym_buf: vec![0; 3 * lit_bufsize],
            sym_next: 0,
            sym_end: (lit_bufsize - 1) * 3,
            opt_len: 0,
            static_len: 0,
            bi_buf: 0,
            bi_valid: 0,
            bi_used: 0,
            high_water: 0,
            gzhead: None,
            gzindex: 0,
        };
        st.reset_keep();
        st.lm_init();
        st
    }

    /// `deflateResetKeep` (`deflate.c:644-677`) — reset the stream, keeping the
    /// configuration (`def->text`/`header` included).
    pub(super) fn reset_keep(&mut self) {
        self.total_in = 0;
        self.total_out = 0;
        self.data_type = Z_UNKNOWN;
        self.pending = 0;
        self.pending_out = 0;
        if self.wrap < 0 {
            self.wrap = -self.wrap; // was made negative by deflate(..., Z_FINISH)
        }
        self.status = if self.wrap == 2 { GZIP_STATE } else { INIT_STATE };
        self.adler = if self.wrap == 2 { 0 } else { 1 }; // crc32(0)/adler32(0) identities
        self.last_flush = -2;
        self.tr_init();
    }

    /// `_tr_init` (`trees.c:456-478`): the tree descriptors, the bit buffer, and the first
    /// block's frequencies.
    fn tr_init(&mut self) {
        self.bi_buf = 0;
        self.bi_valid = 0;
        self.bi_used = 0;
        self.init_block();
    }

    /// `lm_init` (`deflate.c:682-701`): the longest-match setup for a new stream.
    pub(super) fn lm_init(&mut self) {
        self.window_size = 2 * u64::from(self.w_size);
        self.clear_hash();
        // `deflateInit2_`'s derived sizes (`deflate.c:453-521`) are fixed at construction
        // and never change afterwards; every buffer this state hands out is sized by them,
        // so a mismatch would silently mis-index the hash chains or the output. The three
        // that only the *construction* reads are pinned here.
        debug_assert_eq!(self.hash_size, 1usize << self.hash_bits, "hash_size");
        debug_assert_eq!(
            self.hash_shift,
            (self.hash_bits + MIN_MATCH - 1) / MIN_MATCH,
            "hash_shift"
        );
        debug_assert_eq!(self.head.len(), self.hash_size, "head is `hash_size` long");
        debug_assert_eq!(
            self.pending_buf.len(),
            self.lit_bufsize * 4,
            "pending_buf_size is `lit_bufsize * 4` (`LIT_BUFS`)"
        );
        debug_assert_eq!(self.sym_end, (self.lit_bufsize - 1) * 3, "sym_end");
        let cfg = CONFIGURATION_TABLE[self.level as usize];
        self.max_lazy_match = cfg.max_lazy;
        self.good_match = cfg.good_length;
        self.nice_match = cfg.nice_length;
        self.max_chain_length = cfg.max_chain;
        self.strstart = 0;
        self.block_start = 0;
        self.lookahead = 0;
        self.insert = 0;
        self.match_length = MIN_MATCH - 1;
        self.prev_length = MIN_MATCH - 1;
        self.match_available = false;
        self.ins_h = 0;
    }

    /// `CLEAR_HASH` (`deflate.c:170`) — `prev[]` is initialized on the fly.
    pub(super) fn clear_hash(&mut self) {
        // The C writes `head[hash_size - 1]` first and `zmemzero`s the other `hash_size - 1`
        // entries; both are `NIL`, so this is one fill over exactly `hash_size` heads.
        self.head[..self.hash_size].fill(NIL as u16);
    }

    /// The tree a [`TreeKind`] names.
    pub(super) fn tree(&self, kind: TreeKind) -> &[CtData] {
        match kind {
            TreeKind::Lit => &self.dyn_ltree,
            TreeKind::Dist => &self.dyn_dtree,
            TreeKind::Bl => &self.bl_tree,
        }
    }

    /// The tree a [`TreeKind`] names, mutably.
    pub(super) fn tree_mut(&mut self, kind: TreeKind) -> &mut [CtData] {
        match kind {
            TreeKind::Lit => &mut self.dyn_ltree,
            TreeKind::Dist => &mut self.dyn_dtree,
            TreeKind::Bl => &mut self.bl_tree,
        }
    }

    /// `desc->max_code` for `kind`.
    pub(super) fn max_code(&self, kind: TreeKind) -> i32 {
        match kind {
            TreeKind::Lit => self.lit_max_code,
            TreeKind::Dist => self.dist_max_code,
            TreeKind::Bl => self.bl_max_code,
        }
    }

    /// Set `desc->max_code` for `kind`.
    pub(super) fn set_max_code(&mut self, kind: TreeKind, max_code: i32) {
        match kind {
            TreeKind::Lit => self.lit_max_code = max_code,
            TreeKind::Dist => self.dist_max_code = max_code,
            TreeKind::Bl => self.bl_max_code = max_code,
        }
    }

    /// `MAX_DIST(s) = w_size - MIN_LOOKAHEAD` (`deflate.h:301`).
    pub(super) fn max_dist(&self) -> u32 {
        self.w_size - MIN_LOOKAHEAD
    }

    /// `UPDATE_HASH(s, h, c)` (`deflate.c:141`).
    pub(super) fn update_hash(&mut self, c: u8) {
        self.ins_h = ((self.ins_h << self.hash_shift) ^ u32::from(c)) & self.hash_mask;
    }

    /// `slide_hash` (`deflate.c:187-210`): every hash-chain entry moves down by `w_size`,
    /// and anything that would fall off the bottom becomes `NIL`.
    pub(super) fn slide_hash(&mut self) {
        let wsize = self.w_size as u16;
        // The C walks `n = s->hash_size` heads and then `n = w_size` chain links.
        for h in self.head[..self.hash_size].iter_mut() {
            *h = if *h >= wsize { *h - wsize } else { NIL as u16 };
        }
        for p in self.prev[..self.w_size as usize].iter_mut() {
            *p = if *p >= wsize { *p - wsize } else { NIL as u16 };
        }
    }

    /// `INSERT_STRING(s, str, match_head)` (`deflate.c:160`) — returns `match_head`, the
    /// previous head of the chain `str` hashes into.
    pub(super) fn insert_string(&mut self, str: u32) -> u32 {
        let b = self.window[str as usize + MIN_MATCH as usize - 1];
        self.update_hash(b);
        let h = (self.ins_h & self.hash_mask) as usize;
        let slot = (str & self.w_mask) as usize;
        let match_head = self.head[h];
        self.prev[slot] = match_head;
        self.head[h] = str as u16;
        u32::from(match_head)
    }

    /// `put_byte(s, c)` (`deflate.h:293`) — append one byte to the pending buffer.
    pub(super) fn put_byte(&mut self, c: u8) {
        let at = self.pending;
        if at == self.pending_buf.len() {
            // The C's `pending_buf` is exactly `pending_buf_size` bytes and its invariants
            // keep `pending_out + pending` below that. Growing instead of indexing makes a
            // violated invariant a leak rather than a panic; the emitted bytes are
            // unchanged.
            self.pending_buf.push(c);
        } else {
            self.pending_buf[at] = c;
        }
        self.pending += 1;
    }

    /// `putShortMSB` (`deflate.c:939`) — a 16-bit value, most significant byte first.
    pub(super) fn put_short_msb(&mut self, b: u32) {
        self.put_byte((b >> 8) as u8);
        self.put_byte((b & 0xff) as u8);
    }

    /// `_tr_stored_block`'s `put_short` (`trees.c:144`) — a 16-bit value, LSB first.
    pub(super) fn put_short(&mut self, w: u16) {
        self.put_byte((w & 0xff) as u8);
        self.put_byte((w >> 8) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configuration table is what decides which strategy function and which search
    /// limits each level gets; a transposed row would compress at the wrong level, and the
    /// `max_lazy`/`max_chain` floors are required by the C's own comment
    /// (`deflate.c:127-129`).
    #[test]
    fn the_configuration_table_matches_the_c_and_its_floors() {
        assert_eq!(CONFIGURATION_TABLE[0].func, StrategyFn::Stored);
        for level in 1..=3 {
            assert_eq!(CONFIGURATION_TABLE[level].func, StrategyFn::Fast, "level {level}");
        }
        for level in 4..=9 {
            assert_eq!(CONFIGURATION_TABLE[level].func, StrategyFn::Slow, "level {level}");
        }
        for (level, cfg) in CONFIGURATION_TABLE.iter().enumerate().skip(1) {
            assert!(cfg.max_lazy >= MIN_MATCH, "level {level}: max_lazy");
            assert!(cfg.max_chain >= 4, "level {level}: max_chain");
            assert!(cfg.nice_length <= MAX_MATCH as i32, "level {level}: nice_length");
        }
        assert_eq!(CONFIGURATION_TABLE[9].nice_length, MAX_MATCH as i32);
        assert_eq!(CONFIGURATION_TABLE[9].max_chain, 4096);
    }

    /// `deflateInit2_`'s derived sizes: `hash_bits = mem_level + 7`,
    /// `lit_bufsize = 1 << (mem_level + 6)`, `pending_buf_size = 4 * lit_bufsize`,
    /// `sym_end = (lit_bufsize - 1) * 3`, `window_size = 2 * w_size` — all of which feed
    /// the compressor's decisions.
    #[test]
    fn the_derived_sizes_follow_deflate_init2() {
        for mem_level in 1..=9u32 {
            let st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 1, 15, mem_level);
            assert_eq!(st.hash_bits, mem_level + 7);
            assert_eq!(st.hash_size, 1 << (mem_level + 7));
            assert_eq!(st.hash_mask as usize, st.hash_size - 1);
            assert_eq!(st.lit_bufsize, 1 << (mem_level + 6));
            assert_eq!(st.pending_buf_size, st.lit_bufsize * 4);
            assert_eq!(st.sym_end, (st.lit_bufsize - 1) * 3);
            assert!(st.sym_buf.len() >= st.sym_end, "memLevel {mem_level}");
            assert_eq!(st.window.len(), 2 * 32768);
        }
        let narrow = DeflateState::new(6, Z_DEFAULT_STRATEGY, 1, 9, 8);
        assert_eq!(narrow.w_size, 512);
        assert_eq!(narrow.window.len(), 1024);
        assert_eq!(narrow.max_dist(), 512 - MIN_LOOKAHEAD);
        assert_eq!(narrow.hash_shift, (15 + 2) / 3);
    }

    /// A fresh stream is reset before use: `lm_init`'s configuration row, a cleared hash
    /// table and `INIT_STATE`.
    #[test]
    fn a_new_state_has_run_reset_and_lm_init() {
        let st = DeflateState::new(9, Z_DEFAULT_STRATEGY, 1, 15, 8);
        assert_eq!(st.status, INIT_STATE);
        assert_eq!(st.adler, 1);
        assert_eq!(st.last_flush, -2);
        assert_eq!(st.max_chain_length, 4096);
        assert_eq!(st.nice_match, 258);
        assert_eq!(st.max_lazy_match, 258);
        assert_eq!(st.good_match, 32);
        assert!(st.head.iter().all(|h| *h == 0));
        let gz = DeflateState::new(6, Z_DEFAULT_STRATEGY, 2, 15, 8);
        assert_eq!(gz.status, GZIP_STATE);
        assert_eq!(gz.adler, 0, "gzip starts from crc32(0)");
    }

    /// `insert_string` maintains a linked list per hash bucket: the second insertion of the
    /// same string must return the first as its chain head. The running `ins_h` is only
    /// meaningful for **consecutive** insertions (that is the C's `UPDATE_HASH` contract),
    /// so the probe walks positions 0..=3 of `"abcabc"`.
    #[test]
    fn insert_string_chains_equal_strings() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 0, 15, 8);
        st.window[..6].copy_from_slice(b"abcabc");
        assert_eq!(st.insert_string(0), NIL);
        assert_eq!(st.insert_string(1), NIL, "a different 3-byte string");
        assert_eq!(st.insert_string(2), NIL, "a third 3-byte string");
        assert_eq!(st.insert_string(3), 0, "the same string three bytes later");
        assert_eq!(st.prev[3], 0);
        assert_eq!(st.head[(st.ins_h & st.hash_mask) as usize], 3);
    }

    /// `slide_hash` moves every entry down by `w_size` and clips the rest to `NIL`; it is
    /// what keeps the chains consistent after the window slides.
    #[test]
    fn slide_hash_shifts_and_clips() {
        let mut st = DeflateState::new(6, Z_DEFAULT_STRATEGY, 0, 9, 8);
        st.head[0] = 0;
        st.head[1] = 511;
        st.head[2] = 512;
        st.head[3] = 700;
        st.prev[0] = 600;
        st.prev[1] = 100;
        st.slide_hash();
        assert_eq!(st.head[..4], [0, 0, 0, 188]);
        assert_eq!(st.prev[..2], [88, 0]);
    }
}
