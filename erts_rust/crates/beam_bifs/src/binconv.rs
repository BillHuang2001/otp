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

//! `erlang` binary/atom/integer conversions and term (de)serialization:
//! `list_to_binary/1`, `binary_to_list/1,3`, `byte_size/1`, `bit_size/1`,
//! `binary_part/2,3`, `integer_to_binary/1,2`, `binary_to_integer/1,2`,
//! `float_to_binary/1,2`, `binary_to_float/1`, `atom_to_binary/1,2`,
//! `binary_to_atom/1,2`, `binary_to_existing_atom/1,2`, `term_to_binary/1,2`
//! and `binary_to_term/1,2`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/bif.tab` (the roster),
//! `binary.c` (`binary_to_list/1,3`), `erl_bif_guard.c` (`byte_size/1` and the
//! `binary_part/2,3` guards), `erl_bif_binary.c` (`erts_binary_part`),
//! `erl_unicode.c` (`atom_to_binary/2`, `binary_to_atom/2`),
//! `../../erts/emulator/beam/external.c` (the term wire format) and
//! `../../erts/preloaded/src/erlang.erl` (the `atom_to_binary/1`,
//! `binary_to_atom/1`, `binary_to_existing_atom/1` and `binary_to_integer/1`
//! wrappers, which forward to the `/2` BIFs with `utf8`/`10`).
//!
//! The heap-bitstring helpers live in [`crate::bits`]; term (de)serialization
//! routes through `beam_codec`.
//!
//! `list_to_binary/1`'s iolist walk ([`iolist_bytes`]) validates and measures the
//! iolist in one iterative pass and then fills **one** exactly-sized buffer in a
//! second pass, reading a binary element through the closure-scoped borrow
//! [`crate::access::with_binary_bytes`] — so one call costs one allocation (the byte
//! buffer) whatever the iolist's shape, as `tests/binconv_alloc.rs` pins.
//!
//! # Pinned against the OTP 29 (erts-17.1) oracle
//!
//! * `byte_size/1` is `ceil(bit_size / 8)` — **not** `badarg` for a
//!   non-byte-aligned operand: `byte_size(<<1:1>>)` is `1` and
//!   `byte_size(<<1:17>>)` is `3` (`NBYTES(bitstring_size(x))`).
//!   `binary_to_list/1,3` *does* require a byte-aligned binary.
//! * `binary_to_list(Bin, Start, Stop)` is 1-based **and inclusive**, and
//!   requires `1 =< Start =< Stop =< byte_size(Bin)` — so
//!   `binary_to_list(<<1,2,3>>, 3, 2)` and `binary_to_list(<<>>, 1, 1)` are both
//!   `badarg` (an empty range is *not* accepted).
//! * `binary_part(Subject, Start, Len)` accepts a **negative** `Len`, which
//!   selects the `Len` bytes *ending* at `Start`
//!   (`binary_part(<<1,2,3,4,5>>, 4, -2)` is `<<3,4>>`); a byte-aligned subject
//!   is required, as is `Start + Len =< byte_size`.
//! * `list_to_binary/1` takes an **iolist** (a deep list of bytes/binaries, with
//!   a binary or `[]` as the improper tail) and rejects a bare binary
//!   (`list_to_binary(<<1,2>>)` is `badarg`, unlike `iolist_to_binary/1`).
//! * `integer_to_binary/2` renders uppercase digits `A`..`Z` for 10..35
//!   (`integer_to_binary(255, 16)` is `<<"FF">>`); `binary_to_integer/2` accepts
//!   both cases (`<<"ff">>` and `<<"Zz">>` are both `255`/`1295` in base 16/36) and
//!   an optional `+`/`-`. A base outside `2..=36` is `badarg`.
//! * `float_to_binary(Float)` is `float_to_binary(Float, [{scientific, 20}])`
//!   (`float_to_binary(1.5)` is `<<"1.50000000000000000000e+00">>`);
//!   `float_to_binary(Float, Options)` shares `float_to_list/2`'s option set
//!   (`{decimals, N}`, `{scientific, N}`, `compact`) and its renderings.
//! * `atom_to_binary/2` and `binary_to_atom/2` accept the encoding atoms
//!   `latin1`, `utf8` **and** `unicode` (`unicode` is an alias of `utf8` in
//!   `erl_unicode.c`); anything else is `badarg`. `latin1` fails on a code point
//!   above `U+00FF` (`atom_to_binary('π', latin1)` is `badarg`) and accepts every
//!   byte (`binary_to_atom(<<200>>, latin1)` is the atom `'È'`); `utf8` rejects an
//!   invalid sequence (`binary_to_atom(<<200>>, utf8)` is `badarg`). The empty
//!   binary is the empty atom `''`.
//! * `binary_to_atom/2` reports `system_limit` for a name longer than 255
//!   *characters*, while `binary_to_existing_atom/2` reports `badarg` for such a
//!   name (it cannot exist) — the `must_exist` split in `erl_unicode.c`.
//! * `term_to_binary/1` is the `beam_codec` image of the term;
//!   `term_to_binary/2` accepts exactly the option set `parse_t2b_opts` does —
//!   `compressed` (zlib level 6, the oracle's `Z_DEFAULT_COMPRESSION`),
//!   `{compressed, 0..=9}`, `{minor_version, 0|1|2}` (`3`, `-1` and a non-small
//!   version are `badarg`) and the bare `deterministic`/`local` atoms (see the
//!   option section below — both are honoured; `local` needs the runtime's node
//!   hash through the [`BifContext::local_node_hash`] seam).
//!   `binary_to_term/1` ignores trailing bytes; `binary_to_term/2` takes `safe` and
//!   `used` (`binary_to_term(<<131,97,17>>, [used])` is `{17, 3}` — the count
//!   includes the `131` version byte; for a top-level `COMPRESSED` image it stops at
//!   the zlib stream's end, so bytes after the image are *not* counted — the count
//!   comes straight from `beam_codec`'s
//!   [`decode_term_with_options_used`]). Under `safe` the codec itself refuses a
//!   wire atom name that is not already interned — a `badarg` raised **before** any
//!   interning, so nothing reaches the receiving node's atom table.
//!
//! # The `term_to_binary/2` options
//!
//! * **`compressed`/`{compressed, Level}`.** The term is encoded as an *image*
//!   (version byte first) whose **body** — the image minus that byte — is deflated;
//!   the compressed wrapper
//!   `[131][80][body_len:4 BE][zlib]` is emitted **only** when the zlib stream fits
//!   the C's `real_size - 5` budget, otherwise the plain uncompressed image is
//!   returned (`external.c:2250-2301`). That is why
//!   `term_to_binary([1,2,3], [compressed])` is the *uncompressed*
//!   `<<131,107,0,3,1,2,3>>` — a small body cannot pay for zlib's framing — while a
//!   large one (`lists:duplicate(500, hello)`) becomes a real `COMPRESSED` image.
//!   The deflate bytes themselves are implementation-defined (miniz_oxide vs zlib),
//!   so only the framing and the round-trip are pinned.
//! * **`{minor_version, N}`** selects the tag set `parse_t2b_opts` builds
//!   (`external.c:1499-1514`): `2` (the default) is UTF-8 atoms + `NEW_FLOAT_EXT`;
//!   `1` clears `DFLAG_UTF8_ATOMS` (Latin-1 `ATOM_EXT` atoms, `NEW_FLOAT_EXT` floats);
//!   `0` additionally clears `DFLAG_NEW_FLOATS` (legacy 31-byte `FLOAT_EXT` floats).
//!   The three flavours are [`MinorVersion`]'s, so the body is encoded with
//!   [`Encoder::with_minor_version`] and `compressed` then deflates *that* body — the
//!   two options combine. A version outside `0..=2`, a non-small version and a
//!   non-small compression level are `badarg`.
//! * **`deterministic`.** Selects `DFLAG_DETERMINISTIC`, which re-sorts every map's
//!   keys into full Erlang term order — **atoms by name**, not by atom index
//!   (`map_key_compare` = `erts_cmp`, `external.c:3217-3227`, applied at
//!   `:3433-3470`/`:3757-3804`). The flag is installed on the encoder
//!   ([`Encoder::with_deterministic`]), so it applies to **both** a ≤ 32-pair flat map
//!   (whose ERTS storage order is `erts_cmp_flatmap_keys`/index order) and a > 32-pair
//!   HAMT (whose storage order is hash order) — including a map nested in a
//!   tuple/list. Oracle-pinned: `term_to_binary(#{b=>1,a=>2,c=>3},[deterministic])`
//!   is `<<131,116,0,0,0,3,119,1,97,97,2,119,1,98,97,1,119,1,99,97,3>>` (name order)
//!   while the plain image of the same map is index-ordered.
//! * **`local`.** Selects `DFLAG_LOCAL_EXT`, which wraps the image in
//!   `[131][LOCAL_EXT:121][4-byte node hash]` (`external.c:3305-3317`) and replaces
//!   an internal pid/port/ref's node atom with `NIL_EXT` (`:5589`). `beam_codec`
//!   writes the wrapper (`Encoder::finish_local(hash)`) and the internal-node marker
//!   (`Encoder::with_local_ext`), so the **shape** is byte-identical to the oracle's;
//!   the four payload bytes are the node's `local_node_hash` — a Bob-Jenkins block
//!   hash of the node's hostname/pid/time-offset and the VM's monotonic clock
//!   (`external.c:136-200`) — so they are node-local by design and differ between VM
//!   starts (two `erl` runs of `term_to_binary(self(), [local])` differ in exactly
//!   those bytes). The value comes from the runtime through the
//!   [`BifContext::local_node_hash`] seam ([`LocalNode::local_node_hash`]); with no
//!   seam installed `[local]` is the typed `unsupported_bif` gap — the hash is
//!   **never** fabricated (four invented bytes would produce an image the oracle and
//!   every other node reject). A differential case must therefore **mask** those four
//!   bytes rather than compare them.
//! * **`iovec`/`fragment` (and any other unknown atom or tuple) are `badarg`.**
//!   `parse_t2b_opts` accepts `iovec` only when the caller can receive an iovec
//!   (`iovecp != NULL`) and `{fragment, N}` only when it has a fragment size
//!   (`fsizep != NULL`); `term_to_binary/2` passes `NULL` for both
//!   (`external.c:1565-1617`), so the oracle answers `badarg`, as it does for
//!   `[{iovec, N}]`/`[{fragment, N}]`. `local` combined with `deterministic` is
//!   `badarg` too — the two flag sets are mutually exclusive (`external.c:1543-1545`)
//!   — while repeating a single option is fine.
//!
//! # Documented Milestone-1 gaps (never a silently wrong result)
//!
//! * **`float_to_binary/3` does not exist.** The objective's brief listed it, but
//!   erts-17.1 has no such function (`erlang:function_exported(erlang,
//!   float_to_binary, 3)` is `false`, and `bif.tab` has only `/1` and `/2`), so
//!   registering it would *invent* a BIF arity the oracle answers with `undef`.
//!   Following the same rule as `io:format`'s `~d` (see [`crate::io`]), the
//!   oracle wins over the illustrative list: only `/1` and `/2` are registered.
//! * **`local` needs the runtime's node hash.** `[local]` reports `unsupported_bif`
//!   when no [`LocalNode::local_node_hash`] seam answers, because the `LOCAL_EXT`
//!   payload is the node-local, per-VM-start hash (`external.c:136-200`). With a seam
//!   installed the image is byte-identical to the oracle's apart from those four
//!   documented bytes. `[deterministic]` is **fully** honoured. Neither ever returns
//!   a wrong image.
//! * **Internal pid/port/ref ids need a resolvable local node, in both
//!   directions.** `self()` and an internal port are 60-bit `Eterm` immediates
//!   carrying no node identity, so `term_to_binary` writes them as the oracle's
//!   `NEW_PID_EXT` (`88`) / `V4_PORT_EXT` (`120`) — and an internal reference as
//!   `NEWER_REFERENCE_EXT` (`90`) — only when the context can name its local node:
//!   the [`LocalNode`](crate::context::LocalNode) hook, else the M1 runtime's fixed
//!   `nonode@nohost`/creation-0 identity when that atom is interned (see
//!   [`BifContext::local_node`]). With no resolvable local node an internal id
//!   stays the codec's `unsupported_external_term` error, never a wrong image.
//!   The same identity is installed on the **decode** side, so a `NEW_PID_EXT` /
//!   `V4_PORT_EXT` / reference whose node and creation name this node decodes to
//!   the **internal** form (the oracle's `dec_is_this_node` branches) —
//!   `binary_to_term(term_to_binary(self())) =:= self()`. With no identity the wire
//!   form (an external id) is produced. The reference demotion is
//!   **representability-gated**: the M1 internal-reference substitution stores its id in
//!   a single 60-bit small (`beam_term`'s frozen 4-word block), so a wire reference whose
//!   numbers pack past 60 bits — every real `make_ref()` image — keeps the faithful
//!   **external** reference form (a byte-exact round-trip), never a truncated or
//!   substituted id (`beam_codec::ref_wire::representable_ref_id`; the C's
//!   `ErtsORefThing` holds 3+ 32-bit numbers, which this representation does not model).
//! * **`integer_to_binary/1,2` and `binary_to_integer/1,2` promote to bignums.**
//!   A value outside the 60-bit small range becomes an arbitrary-precision bignum
//!   exactly as the oracle's does; only a magnitude past the bignum limb bound is
//!   `system_limit` (the same documented gap as
//!   [`crate::convert`]'s `integer_to_list`/`list_to_integer`).
//! * **`float_to_binary/2` option `short`** is the shortest round-trip
//!   rendering, byte-identical to [`crate::convert`]'s `float_to_list/2` with
//!   the same option (`float_to_binary(1.0e10, [short])` ⇒ `<<"1.0e10">>`); see
//!   [`crate::numeric`] for the notation and exact-tie rules.//! * **`binary_to_atom`/`binary_to_existing_atom` and `binary_to_term` need the
//!   [`AtomInterner`](crate::context::AtomInterner) hook to *create* an atom.**
//!   The context only borrows the atom table immutably, so a name that is not
//!   already interned reports
//!   [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when no
//!   hook is installed (`beam_rt` always installs one) — never a fabricated
//!   index.
//! * **Sub-binaries are copied.** C makes `binary_part/2,3` a sub-binary
//!   reference; Milestone-1 keeps every binary on the heap and copies the range,
//!   so the value is identical but no shared storage is created (the placement
//!   deviation [`crate::bits`] documents).
//!
//! # The atom-index coupling of `binary_to_term`
//!
//! `beam_codec::Decoder::decode` needs a **mutable** [`AtomTable`] to intern the
//! atoms it meets, while [`BifContext::atoms`] only hands out `&AtomTable`. This
//! module therefore decodes into a **clone of the context's table** and then hands
//! every name the decoder appended to the runtime through
//! [`BifContext::intern_atom`]. That is sound because `AtomTable` assigns indices
//! in insertion order: the clone has the context's indices for every existing
//! name, and the names the decoder appends are the ones the runtime then interns,
//! in the same order and therefore at the same indices — so the decoded term's atom
//! words already refer to the runtime's atoms. The precondition (the interner must
//! extend the very table `ctx.atoms()` borrows) is the one
//! [`crate::context`] documents for the hook.

use std::cell::RefCell;

use beam_codec::compress::compress as zlib_compress;
use beam_codec::{
    decode_term_with_options_used, DecodeOptions, Encoder, MinorVersion, COMPRESSED_EXT,
    VERSION_MAGIC,
};
use beam_common::Result;
use beam_term::{atom_val, signed_value, Atom, AtomTable, Eterm, NIL};

use crate::access;
use crate::arith;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, system_limit, unsupported_bif};
use crate::numeric::{
    base_arg, format_scientific, integer_digits, parse_float, parse_float_options, parse_integer,
    render_float, small_arg, DEFAULT_FLOAT_DECIMALS,
};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// The maximum atom-name length in *characters* (erts-17.1, `ATOM_MAX_CHARS`).
const MAX_ATOM_CHARS: usize = 255;

/// The largest `{minor_version, N}` the oracle accepts (`3` is `badarg`).
const MAX_MINOR_VERSION: i64 = 2;

// ---------------------------------------------------------------------------
// Shared argument helpers
// ---------------------------------------------------------------------------

/// The value of a non-negative integer argument (`term_to_Uint`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-integer or a
/// negative value.
fn uint_arg(term: Eterm) -> Result<usize> {
    usize::try_from(small_arg(term)?).map_err(|_| badarg())
}

/// The bytes of a byte-aligned bitstring argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything that is not
/// a binary (including a non-byte-aligned bitstring) — the oracle's
/// `is_not_bitstring`/`TAIL_BITS` rejections.
///
/// # Why this copies (measured, deliberately not "optimised")
///
/// This is the one **payload-proportional** allocation left on the
/// `binary_to_term`/`binary_to_list`/`binary_to_atom`/`integer_from_binary` path: the
/// whole input image is copied into a fresh `Vec` before the codec sees it
/// (`tests/binconv_alloc.rs` measures 1 allocation / 4102 bytes for a 4 KiB image, and
/// 101 of the 285 bytes the five-key map's decode allocates).
///
/// It stays a copy because a **borrowing** accessor cannot be written inside this
/// crate's safe-Rust boundary:
///
/// * The only route to a term's payload bytes is
///   [`beam_term::bitstring::bitstring_parts`] — an `unsafe` raw-pointer API. An
///   `Eterm` is a bare machine word, so the returned `*const u8` carries no lifetime
///   tied to the term; producing `&[u8]` from it needs an unchecked lifetime
///   extension, which is exactly the kind of `unsafe` that `binconv.rs`/`bits.rs` must
///   not carry (`src/access.rs` is the crate's only sanctioned `unsafe` module, and
///   even there the lifetime would be unprovable, not merely unwritten).
/// * The bytes may also live in an **off-heap refc binary**, whose payload is
///   refcounted and released by the runtime's `beam_gc::OffHeap` sweep — the term does
///   not express that ownership as a Rust lifetime, so a borrowed slice could outlive
///   it in principle.
/// * Two borrow hazards would have to be excluded at *every* call site: the decoder
///   writes the result onto [`BifContext::heap`] **while** reading the source bytes
///   (a bump arena that never reallocates, so the write cannot move them — but the
///   aliasing is only sound because the two are distinct arenas, which a `&[u8]`
///   derived from a raw pointer would not record), and several sites
///   (`binary_to_list/3`, `binary_part/3`) return a term built from a *slice* of the
///   buffer.
///
/// That closure-scoped accessor now exists —
/// [`access::with_binary_bytes`](crate::access::with_binary_bytes) lends the bytes to
/// a closure instead of returning them — and the iolist encode ([`iolist_bytes`]) uses
/// it, so a binary element is copied straight into the destination with no temporary.
/// This function keeps the **owned** form deliberately: its callers (the codec's
/// decoder, `binary_to_list/3`, `binary_part/3`) need a buffer that outlives a single
/// closure (or a slice of one), so those sites' copies are unchanged and their
/// measured cost stands (`tests/binconv_alloc.rs` pins 1 allocation / image bytes for
/// the 4 KiB decode). Adopting the borrowing form on the decode path is a recorded
/// follow-up, not part of this module's iolist work.
fn bytes_arg(term: Eterm) -> Result<Vec<u8>> {
    bits::bytes(term)
}
/// A binary of `bytes` built on the context heap.
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn make_bin(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    bits::make_binary(ctx.heap(), bytes)
}

/// The byte list of `bytes`, built on the context heap.
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// The [`Eterm`] slots the iolist walk keeps **inline** — a plain array on the Rust
/// stack — before it spills to a heap `Vec` (see [`WalkStack`]).
///
/// One list level costs one frame (its pending tail) plus one for the element being
/// expanded, so 128 frames cover an iolist nested roughly 127 levels deep: far past
/// any real iolist, while the whole stack stays a 1 KiB stack footprint. A deeper
/// iolist is not rejected — the frames past this depth go into the spill `Vec`, the
/// walk's only possible allocation.
const INLINE_WALK_FRAMES: usize = 128;

/// The iolist walk's work stack: [`INLINE_WALK_FRAMES`] slots inline, spilling to a
/// `Vec` only when a deeper iolist needs them — so a flat or moderately nested iolist
/// allocates **nothing** for the walk itself.
///
/// LIFO order is preserved across the spill: a frame reaches the `Vec` only once the
/// inline array is full, and is popped from the `Vec` first for the same reason.
struct WalkStack {
    inline: [Eterm; INLINE_WALK_FRAMES],
    len: usize,
    spill: Vec<Eterm>,
}

impl WalkStack {
    /// An empty stack (each pass pushes its own root).
    fn empty() -> Self {
        Self {
            inline: [NIL; INLINE_WALK_FRAMES],
            len: 0,
            spill: Vec::new(),
        }
    }

    fn push(&mut self, term: Eterm) {
        if self.len < INLINE_WALK_FRAMES {
            self.inline[self.len] = term;
            self.len += 1;
        } else {
            self.spill.push(term);
        }
    }

    fn pop(&mut self) -> Option<Eterm> {
        if let Some(term) = self.spill.pop() {
            return Some(term);
        }
        self.len = self.len.checked_sub(1)?;
        Some(self.inline[self.len])
    }

    /// Empties the stack for the next pass, **keeping** the spill's capacity, so the
    /// second pass of a deep iolist never re-grows it.
    fn reset(&mut self) {
        self.len = 0;
        self.spill.clear();
    }
}

/// The iolist shapes: an integer in `0..=255`, a binary, or a (possibly improper)
/// list of iolists whose tail is a binary or `[]`.
///
/// The walk is iterative (a work stack) so a deeply nested iolist cannot overflow the
/// Rust stack — C's `erts_iolist_to_buf` is iterative for the same reason — and the
/// stack is [`WalkStack`], which keeps the ordinary case allocation-free.
///
/// This module walks an iolist **twice** over one [`WalkStack`], both times
/// iteratively: [`iolist_byte_len`] validates and measures it, [`append_iolist`] then
/// fills a single buffer reserved with that exact capacity. The pre-pass's validation
/// is **authoritative for both**: a term graph cannot change between the passes (the
/// walk only reads, and no BIF runs in between), so the second pass's `badarg` arms
/// are unreachable and exist only so an impossible shape is a typed error rather than
/// a silent skip or a panic.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an element that is
/// none of the three shapes (including a non-byte-aligned bitstring, an integer
/// outside `0..=255` and any improper tail that is not a binary). The single
/// allocation is the byte buffer, sized to the iolist's exact byte count — see
/// `tests/binconv_alloc.rs` for the pinned counts.
fn iolist_bytes(root: Eterm) -> Result<Vec<u8>> {
    let mut stack = WalkStack::empty();
    let total = iolist_byte_len(root, &mut stack)?;
    let mut out = Vec::with_capacity(total);
    append_iolist(root, &mut stack, &mut out)?;
    Ok(out)
}

/// The byte length of the iolist `root`, **validating** every element on the way.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) exactly where
/// [`iolist_bytes`] documents it; `system_limit` only for a byte count that cannot
/// fit a `usize` (an iolist of more than `usize::MAX` bytes, which no heap could
/// hold).
fn iolist_byte_len(root: Eterm, stack: &mut WalkStack) -> Result<usize> {
    stack.reset();
    stack.push(root);
    let mut total = 0usize;
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            // A byte. The range check is the validation, not just a conversion.
            u8::try_from(signed_value(term)).map_err(|_| badarg())?;
            total = total.checked_add(1).ok_or_else(system_limit)?;
            continue;
        }
        if bits::is_binary(term) {
            // `byte_size` rejects a non-byte-aligned bitstring, as C's `erts_iolist_size`
            // does; it reads the size word without copying the payload.
            total = total
                .checked_add(bits::byte_size(term)?)
                .ok_or_else(system_limit)?;
            continue;
        }
        if access::is_list(term) {
            // The tail of a list is a *tail* position: for `list_to_binary/1` (as in
            // C's `erts_iolist_to_buf`) it may only be another list, `[]` or a binary
            // — `list_to_binary([1|2])` is `badarg`, `list_to_binary([1|<<2>>])` is not.
            let tail = access::cdr(term);
            if !access::is_list(tail) && !access::is_nil(tail) && !bits::is_binary(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is popped (and appended) first.
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    Ok(total)
}

/// Appends the bytes of the iolist `root` to `out` — the write half of the walk, run
/// only after [`iolist_byte_len`] has validated `root`, so `out` is already reserved to
/// the exact byte count and never reallocates.
///
/// A binary element is copied **straight into `out`** through the closure-scoped
/// borrow [`bits::with_bytes`], not through a per-element temporary `Vec`.
///
/// # Errors
///
/// As [`iolist_byte_len`]; the `badarg` arms are unreachable (the pre-pass is
/// authoritative — the term cannot change between the passes) and are kept so an
/// impossible shape can never be silently skipped.
fn append_iolist(root: Eterm, stack: &mut WalkStack, out: &mut Vec<u8>) -> Result<()> {
    stack.reset();
    stack.push(root);
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            out.push(u8::try_from(signed_value(term)).map_err(|_| badarg())?);
            continue;
        }
        if bits::is_binary(term) {
            bits::with_bytes(term, |bytes| {
                out.extend_from_slice(bytes);
                Ok(())
            })?;
            continue;
        }
        if access::is_list(term) {
            let tail = access::cdr(term);
            if !access::is_list(tail) && !access::is_nil(tail) && !bits::is_binary(tail) {
                return Err(badarg());
            }
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list_to_binary/1 and binary_to_list/1,3
// ---------------------------------------------------------------------------

/// `list_to_binary(IoList)`.
///
/// Unlike `iolist_to_binary/1`, a bare binary is *not* an iolist here
/// (`list_to_binary(<<1,2>>)` is `badarg` in the oracle too).
fn list_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_list(args[0]) && !access::is_nil(args[0]) {
        return Err(badarg());
    }
    let bytes = iolist_bytes(args[0])?;
    make_bin(ctx, &bytes)
}

/// `binary_to_list(Binary)` — the bytes, in order.
fn binary_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let above = bytes_arg(args[0])?;
    byte_list(ctx, &above)
}

/// `binary_to_list(Binary, Start, Stop)` — 1-based and **inclusive**, requiring
/// `1 =< Start =< Stop =< byte_size(Binary)`.
fn binary_to_list_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let above = bytes_arg(args[0])?;
    let size = above.len();
    let start = uint_arg(args[1])?;
    let stop = uint_arg(args[2])?;
    if start < 1 || start > size || stop < 1 || stop > size || stop < start {
        return Err(badarg());
    }
    byte_list(ctx, &above[start - 1..stop])
}

// ---------------------------------------------------------------------------
// byte_size/1 and bit_size/1
// ---------------------------------------------------------------------------

/// `bit_size(Bitstring)` — the size in bits.
fn bit_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bits_size = bits::bit_size(args[0])?;
    let value = i64::try_from(bits_size).map_err(|_| system_limit())?;
    ctx.int(value)
}

/// `byte_size(Bitstring)` — `NBYTES(bit_size)`, i.e. the size rounded **up** to
/// whole bytes (`byte_size(<<1:1>>)` is `1`).
fn byte_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bits_size = bits::bit_size(args[0])?;
    let value = i64::try_from(bits_size.div_ceil(8)).map_err(|_| system_limit())?;
    ctx.int(value)
}

// ---------------------------------------------------------------------------
// binary_part/2,3
// ---------------------------------------------------------------------------

/// `binary_part(Subject, {Start, Length})`.
fn binary_part_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_tuple(args[1]) || access::tuple_size(args[1]) != 2 {
        return Err(badarg());
    }
    let start = access::tuple_element(args[1], 0);
    let length = access::tuple_element(args[1], 1);
    binary_part_range(ctx, args[0], start, length)
}

/// `binary_part(Subject, Start, Length)`.
fn binary_part_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    binary_part_range(ctx, args[0], args[1], args[2])
}

/// The shared `erts_binary_part` core.
///
/// A negative `Length` selects the `|Length|` bytes *ending* at `Start` (so
/// `binary_part(<<1,2,3,4,5>>, 4, -2)` is `<<3,4>>`), and `Start` itself is a
/// non-negative offset. The subject must be byte-aligned and the whole range must
/// lie inside it.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary
/// subject, a non-integer `Start`/`Length`, a `|Length|` larger than `Start`, or a
/// range that leaves the binary.
fn binary_part_range(
    ctx: &mut BifContext<'_>,
    subject: Eterm,
    start_term: Eterm,
    length_term: Eterm,
) -> Result<Eterm> {
    let data = bytes_arg(subject)?;
    let start = uint_arg(start_term)?;
    let length = small_arg(length_term)?;
    let size = data.len();
    let (start, length) = if length < 0 {
        let length = length.unsigned_abs() as usize;
        if length > start {
            return Err(badarg());
        }
        (start - length, length)
    } else {
        (start, length as usize)
    };
    let end = start.checked_add(length).ok_or_else(badarg)?;
    if end > size {
        return Err(badarg());
    }
    make_bin(ctx, &data[start..end])
}

// ---------------------------------------------------------------------------
// integer_to_binary/1,2 and binary_to_integer/1,2
// ---------------------------------------------------------------------------

/// `integer_to_binary(Integer)` — base 10.
fn integer_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let rendered = integer_digits(args[0], 10)?;
    make_bin(ctx, rendered.as_bytes())
}

/// `integer_to_binary(Integer, Base)`.
fn integer_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let base = base_arg(args[1])?;
    let rendered = integer_digits(args[0], base)?;
    make_bin(ctx, rendered.as_bytes())
}

/// `binary_to_integer(Binary)` — base 10.
fn binary_to_integer_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    integer_from_binary(ctx, args[0], 10)
}

/// `binary_to_integer(Binary, Base)`.
fn binary_to_integer_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let base = base_arg(args[1])?;
    integer_from_binary(ctx, args[0], base)
}

/// The shared `erts_internal:binary_to_integer/2` core.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary or a
/// string that is not a base-`base` integer; `system_limit` only for a magnitude
/// past the bignum limb bound (the oracle's own limit).
fn integer_from_binary(ctx: &mut BifContext<'_>, input: Eterm, base: u32) -> Result<Eterm> {
    let data = bytes_arg(input)?;
    let text = core::str::from_utf8(&data).map_err(|_| badarg())?;
    let (negative, magnitude) = parse_integer(text, base)?;
    arith::int_from_mag(ctx, negative, &magnitude)
}

// ---------------------------------------------------------------------------
// float_to_binary/1,2 and binary_to_float/1
// ---------------------------------------------------------------------------

/// `float_to_binary(Float)` — `[{scientific, 20}]`.
fn float_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_float(args[0]) {
        return Err(badarg());
    }
    let rendered = format_scientific(access::float_value(args[0]), DEFAULT_FLOAT_DECIMALS)?;
    make_bin(ctx, rendered.as_bytes())
}

/// `float_to_binary(Float, Options)`.
fn float_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_float(args[0]) {
        return Err(badarg());
    }
    let value = access::float_value(args[0]);
    let (format, decimals, compact) = parse_float_options(ctx.atoms(), args[1])?;
    let rendered = render_float(value, format, decimals, compact)?;
    make_bin(ctx, rendered.as_bytes())
}

/// `binary_to_float(Binary)` — the same grammar as `list_to_float/1`, on a
/// binary.
fn binary_to_float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let data = bytes_arg(args[0])?;
    let text = core::str::from_utf8(&data).map_err(|_| badarg())?;
    let value = parse_float(text)?;
    ctx.make_float(value)
}

// ---------------------------------------------------------------------------
// atom_to_binary/1,2, binary_to_atom/1,2 and binary_to_existing_atom/1,2
// ---------------------------------------------------------------------------

/// The text encoding selected by an `Encoding` argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TextEncoding {
    /// One byte per code point; only `U+0000..=U+00FF` is representable.
    Latin1,
    /// UTF-8.
    Utf8,
}

/// The `Encoding` argument: `latin1`, `utf8` or `unicode` (`unicode` is an alias
/// of `utf8` in `erl_unicode.c`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-atom or any
/// other atom.
fn encoding_arg(atoms: &AtomTable, term: Eterm) -> Result<TextEncoding> {
    if !access::is_atom(term) {
        return Err(badarg());
    }
    match atoms.name(atom_val(term)) {
        Some("latin1") => Ok(TextEncoding::Latin1),
        Some("utf8") | Some("unicode") => Ok(TextEncoding::Utf8),
        _ => Err(badarg()),
    }
}

/// Encodes `name` in `encoding`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `latin1` is asked
/// for a code point above `U+00FF` (`erts_utf8_to_latin1`'s failure).
fn encode_text(name: &str, encoding: TextEncoding) -> Result<Vec<u8>> {
    match encoding {
        TextEncoding::Utf8 => Ok(name.as_bytes().to_vec()),
        TextEncoding::Latin1 => {
            let mut out = Vec::with_capacity(name.len());
            for ch in name.chars() {
                let code = ch as u32;
                if code > 0xFF {
                    return Err(badarg());
                }
                out.push(code as u8);
            }
            Ok(out)
        }
    }
}

/// Decodes `bytes` in `encoding`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `utf8` is given an
/// invalid UTF-8 sequence.
fn decode_text(bytes: &[u8], encoding: TextEncoding) -> Result<String> {
    match encoding {
        // Latin-1 is one byte per code point, `U+0000..=U+00FF`.
        TextEncoding::Latin1 => Ok(bytes.iter().map(|&byte| char::from(byte)).collect()),
        TextEncoding::Utf8 => core::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| badarg()),
    }
}

/// `atom_to_binary(Atom)` — UTF-8.
fn atom_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    atom_to_bin(ctx, args[0], TextEncoding::Utf8)
}

/// `atom_to_binary(Atom, Encoding)`.
fn atom_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let encoding = encoding_arg(ctx.atoms(), args[1])?;
    atom_to_bin(ctx, args[0], encoding)
}

/// The shared `atom_to_binary/2` core.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-atom or a
/// `latin1` encoding the name cannot be written in.
fn atom_to_bin(ctx: &mut BifContext<'_>, term: Eterm, encoding: TextEncoding) -> Result<Eterm> {
    if !access::is_atom(term) {
        return Err(badarg());
    }
    let name = ctx
        .atoms()
        .name(atom_val(term))
        .ok_or_else(badarg)?
        .to_string();
    let bytes = encode_text(&name, encoding)?;
    make_bin(ctx, &bytes)
}

/// `binary_to_atom(Binary)` — UTF-8.
fn binary_to_atom_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    atom_from_binary(ctx, args[0], TextEncoding::Utf8, false)
}

/// `binary_to_atom(Binary, Encoding)`.
fn binary_to_atom_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let encoding = encoding_arg(ctx.atoms(), args[1])?;
    atom_from_binary(ctx, args[0], encoding, false)
}

/// `binary_to_existing_atom(Binary)` — UTF-8, never creates.
fn binary_to_existing_atom_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    atom_from_binary(ctx, args[0], TextEncoding::Utf8, true)
}

/// `binary_to_existing_atom(Binary, Encoding)` — never creates.
fn binary_to_existing_atom_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let encoding = encoding_arg(ctx.atoms(), args[1])?;
    atom_from_binary(ctx, args[0], encoding, true)
}

/// The shared `binary_to_atom/2` core (`must_exist` is the
/// `binary_to_existing_atom/2` split in `erl_unicode.c`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary, a bad
/// encoding of the bytes, or (when `must_exist`) a name that is not interned
/// (which includes a name longer than 255 characters: it cannot exist);
/// [`BeamError::SystemLimit`] for a *new* name longer than 255 characters;
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the name
/// is new and the runtime installed no interner.
fn atom_from_binary(
    ctx: &mut BifContext<'_>,
    input: Eterm,
    encoding: TextEncoding,
    must_exist: bool,
) -> Result<Eterm> {
    let data = bytes_arg(input)?;
    let name = decode_text(&data, encoding)?;
    if must_exist {
        return ctx.atom(&name).map_err(|_| badarg());
    }
    if name.chars().count() > MAX_ATOM_CHARS {
        return Err(system_limit());
    }
    ctx.intern_atom(&name)
}

// ---------------------------------------------------------------------------
// term_to_binary/1,2 and binary_to_term/1,2
// ---------------------------------------------------------------------------

/// `term_to_binary(Term)` — the `beam_codec` external term image.
///
/// The default `{minor_version, 2}` flavour with no compression, so the image is
/// exactly [`T2bOptions::default`]'s; an internal pid/port id is encoded through the
/// context's local node (see [`encode_body`]).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed option list
/// or an unknown/out-of-range option; the codec's errors otherwise (e.g.
/// `unsupported_external_term` for an internal id with no resolvable local node, or a
/// tag the codec cannot write).
fn term_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = encode_image(ctx, args[0], &T2bOptions::default())?;
    make_bin(ctx, &bytes)
}

/// `term_to_binary(Term, Options)` — the `compressed` family and `{minor_version, N}`.
///
/// The option list is validated exactly like the oracle's `parse_t2b_opts` and the
/// image is built for the selected tag set / compression (see the module docs).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed option list
/// or an unknown/out-of-range option; the codec's and zlib's errors otherwise (e.g.
/// `unsupported_external_term` for a tag the codec cannot write).
fn term_to_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let options = parse_term_to_binary_options(ctx.atoms(), args[1])?;
    let image = encode_image(ctx, args[0], &options)?;
    make_bin(ctx, &image)
}

/// The parsed `term_to_binary/2` options (`parse_t2b_opts`, `external.c:1481-1520`).
///
/// Crate-visible so that `erlang:external_size/2` (in [`crate::term_conv`]) runs the
/// **same** parser and the **same** body encoder as `term_to_binary/2`; the oracle's
/// own `external_size_2` reuses `parse_t2b_opts` and the encoder's size probe
/// (`external.c:2221-2246`, `external.c:736-743`), so the two BIFs must not drift
/// on which option lists are valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct T2bOptions {
    /// The selected `{minor_version, N}` (`0..=2`).
    minor_version: u8,
    /// The zlib level of `compressed`/`{compressed, Level}`, `None` when neither was
    /// given (the oracle's `level` default).
    compression: Option<u8>,
    /// The bare `deterministic` atom (`DFLAG_DETERMINISTIC`, `external.c:1552`).
    deterministic: bool,
    /// The bare `local` atom (`DFLAG_LOCAL_EXT` — the `LOCAL_EXT` wrapper,
    /// `external.c:1539-1542`, `:3305-3317`).
    local: bool,
}

impl Default for T2bOptions {
    /// `{minor_version, 2}` (the current default flag set) and no compression —
    /// exactly the oracle's `TERM_TO_BINARY_DFLAGS` with `level = 0`.
    fn default() -> Self {
        T2bOptions {
            minor_version: 2,
            compression: None,
            deterministic: false,
            local: false,
        }
    }
}

/// The zlib level the bare `compressed` atom selects.
///
/// The C sets `Z_DEFAULT_COMPRESSION` (`-1`), which zlib maps to level 6, so the
/// explicit [`flate2`](beam_codec::compress) level is 6.
const DEFAULT_COMPRESSED_LEVEL: u8 = 6;

/// The largest `{compressed, Level}` level — the C requires `0 =< Level < 10`.
const MAX_COMPRESSED_LEVEL: i64 = 9;

/// The zlib wrapper overhead the C's `real_size - 5` compression budget accounts
/// for: the `COMPRESSED` tag, the 4-byte size field — the encoded body already
/// carries neither, so a compressed image only pays off when the deflate stream is
/// at least these 5 bytes shorter than the body.
const COMPRESSED_OVERHEAD: usize = 5;

/// Parses and validates a `term_to_binary/2` option list.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list (including an
/// improper list), an unknown atom/tuple option, a `{minor_version, N}` outside
/// `0..=2`, or a `{compressed, Level}` whose level is not a small integer in
/// `0..=9` — the C `parse_t2b_opts` rejections.
pub(crate) fn parse_term_to_binary_options(atoms: &AtomTable, opts: Eterm) -> Result<T2bOptions> {
    let mut parsed = T2bOptions::default();
    for item in proper_list(opts).ok_or_else(badarg)? {
        if access::is_atom(item) {
            match atoms.name(atom_val(item)) {
                Some("compressed") => parsed.compression = Some(DEFAULT_COMPRESSED_LEVEL),
                Some("deterministic") => parsed.deterministic = true,
                Some("local") => parsed.local = true,
                // `iovec` is accepted by `parse_t2b_opts` only when the caller can
                // receive an iovec (`iovecp != NULL`) — `term_to_binary/2` passes
                // `NULL` (`external.c:1565-1590`), so the oracle answers `badarg`, as
                // it does for `fragment` (a `fsizep == NULL` caller, `:1592-1617`) and
                // for every other atom.
                _ => return Err(badarg()),
            }
            continue;
        }
        if !access::is_tuple(item) || access::tuple_size(item) != 2 {
            return Err(badarg());
        }
        let name = access::tuple_element(item, 0);
        let value = access::tuple_element(item, 1);
        if !access::is_atom(name) {
            return Err(badarg());
        }
        match atoms.name(atom_val(name)) {
            Some("minor_version") => {
                let version = small_arg(value)?;
                if !(0..=MAX_MINOR_VERSION).contains(&version) {
                    return Err(badarg());
                }
                parsed.minor_version = version as u8;
            }
            Some("compressed") => {
                let level = small_arg(value)?;
                if !(0..=MAX_COMPRESSED_LEVEL).contains(&level) {
                    return Err(badarg());
                }
                parsed.compression = Some(level as u8);
            }
            _ => return Err(badarg()),
        }
    }
    // `parse_t2b_opts` rejects the pair *after* the whole list is scanned
    // (`external.c:1543-1545`), so `[local, deterministic]` is `badarg` in either
    // order — the two flag sets are mutually exclusive.
    if parsed.local && parsed.deterministic {
        return Err(badarg());
    }
    Ok(parsed)
}

/// Encodes `term` as the `term_to_binary/2` image `options` select.
///
/// Takes `ctx` mutably because the local-node identity, the local-node hash and the
/// local-fun identity are read through the context (see [`term_encoder`]).
///
/// # Errors
///
/// The encoder's and zlib's errors (see [`encode_body`]), plus
/// `error:unsupported_bif` for the `local` option when the runtime installed no node
/// hash ([`BifContext::local_node_hash`]).
fn encode_image(ctx: &mut BifContext<'_>, term: Eterm, options: &T2bOptions) -> Result<Vec<u8>> {
    // `local` selects the `LOCAL_EXT` wrapper: `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE]`
    // (`external.c:3305-3317`) followed by the plain body, whose internal pid/port/ref
    // node atom is replaced by `NIL_EXT` (`:5589`). The *shape* is therefore
    // byte-identical to the oracle's — the codec owns the wrapper
    // ([`Encoder::finish_local`]) and the internal-node marker
    // ([`Encoder::with_local_ext`]) — **except** the four hash bytes: they are the
    // node's `local_node_hash`, a Bob-Jenkins block hash seeded from the node's
    // hostname/pid/time-offset and the runtime's monotonic clock
    // (`external.c:136-200`), i.e. node-local by design, so two oracle runs of
    // `term_to_binary(self(), [local])` differ in exactly those bytes
    // (`<<131,121,1,194,117,38,…>>` vs `<<131,121,6,240,188,32,…>>`) — see
    // `tests/term_to_binary_options.rs`. The value therefore comes from the runtime
    // through the [`BifContext::local_node_hash`] seam; with no seam installed
    // `[local]` stays the typed `unsupported_bif` gap, and the hash is **never**
    // fabricated (four invented bytes would produce an image the oracle and every other
    // node reject).
    if options.local {
        let hash = ctx.local_node_hash().ok_or_else(unsupported_bif)?;
        // `finish_local` writes the uncompressed `[local]` image
        // `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]`, body included.
        let image = term_encoder(ctx, term, options)?.finish_local(hash);
        return match options.compression {
            None => Ok(image),
            // `[local, compressed]`: the C deflates the **local body** — the wrapper
            // plus the term body, i.e. the image minus its version byte — because it
            // writes the `LOCAL_EXT` prologue into the buffer it compresses
            // (`erts_term_to_binary_simple`, `external.c:2265-2298`): the image is
            // `[VERSION_MAGIC][COMPRESSED_EXT][local_body_len:4 BE][zlib local body]`,
            // reverting to the plain `[local]` image when compression does not help
            // (handed back as the very buffer it was given, not a copy).
            Some(level) => compressed_image(image, level),
        };
    }
    // `deterministic` is threaded into the encoder itself (`DFLAG_DETERMINISTIC`
    // re-sorts every map's pairs into full Erlang term order, `external.c:3757-3764`
    // and `:3783-3807`), so an image that needs a re-sort is produced faithfully
    // rather than refused (see [`encode_body`]).
    match options.compression {
        None => plain_image_of(ctx, term, options),
        Some(level) => compressed_image(encode_body(ctx, term, options)?, level),
    }
}

/// The **uncompressed** external image of `term` under `options`: [`encode_body`]'s
/// bytes — which already begin with the version byte — or, for the `local` option, the
/// `LOCAL_EXT`-wrapped image `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]` (see
/// [`encode_image`]; the hash comes from [`BifContext::local_node_hash`], and with no
/// seam installed `[local]` is the typed `unsupported_bif` gap).
///
/// This is the image `erlang:term_to_binary/1` produces, and the one
/// `erlang:external_size/1,2` ([`crate::term_conv`]) sizes — `external_size` only reads
/// the length, which costs nothing extra because the image is the encoder's own buffer
/// (see [`encode_body`]) rather than a copy of it.
///
/// The oracle's `external_size_2` (`external.c:2221-2246`) validates the whole option
/// list — `compressed`/`{compressed, Level}` included — but passes only the *flags*
/// (the `{minor_version, N}` tag set, and `local`) to its size probe
/// (`erts_encode_ext_size_2`, `external.c:736-743`, which never deflates). The
/// documented contract is an upper bound — `byte_size(term_to_binary(T, Opts)) =<
/// erlang:external_size(T, Opts)` (`../../erts/preloaded/src/erlang.erl:2687-2710`) —
/// so a compressed image is deliberately **not** what this returns. Oracle-pinned:
/// `erlang:external_size(lists:duplicate(500,hello),[compressed])` is `3507` while
/// `byte_size(term_to_binary(lists:duplicate(500,hello),[compressed]))` is `50`; with
/// `[local]` the probe sees the `LOCAL_EXT` wrapper (and the `NIL_EXT` substitution),
/// so `external_size([1,2],[local])` is `11` and `external_size(self(),[local])` is
/// `20` (the plain sizes are `6` and `29`).
///
/// # Errors
///
/// The encoder's errors (see [`encode_body`]), plus `error:unsupported_bif` for the
/// `local` option when the runtime installed no node hash.
pub(crate) fn plain_image_of(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: &T2bOptions,
) -> Result<Vec<u8>> {
    if options.local {
        let hash = ctx.local_node_hash().ok_or_else(unsupported_bif)?;
        return Ok(term_encoder(ctx, term, options)?.finish_local(hash));
    }
    encode_body(ctx, term, options)
}
/// Encodes `term` as the whole `term_to_binary` image: the version byte `131`
/// followed by the term's body, under the tag set `options.minor_version` selects.
///
/// Shared by `term_to_binary/1,2` (via [`encode_image`]) and by
/// `erlang:external_size/1,2` (via [`plain_image_of`]), so both BIFs agree on the tag
/// set and on what is encodable. It is [`term_encoder`] plus [`Encoder::finish`]; the
/// `local` option's `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE]` prologue is added by
/// [`Encoder::finish_local`] in [`encode_image`]/[`plain_image_of`], not here.
///
/// **The image is the encoder's own buffer, not a copy.** [`term_encoder`] builds an
/// [`Encoder::new`] (which writes `VERSION_MAGIC` into the writer *before* the body)
/// and [`Encoder::finish`] moves that buffer out, so one encode allocates the writer
/// and nothing else — there is no `[push(VERSION_MAGIC)] + extend(body)` second
/// `Vec` and no `Vec::insert(0, …)` memmove of the body
/// (`tests/binconv_alloc.rs` pins it).
///
/// The three `{minor_version, N}` flavours are [`MinorVersion`]'s (`external.c:1499-1514`:
/// `DFLAG_UTF8_ATOMS` and `DFLAG_NEW_FLOATS`); the codec owns the tag selection, so an
/// atom goes on the wire as `ATOM_EXT` (`100`) under `0`/`1` (the `term_to_binary`
/// Latin-1 form — *not* the distribution `SMALL_ATOM_EXT` flavour) and a float as the
/// legacy 31-byte `FLOAT_EXT` (`99`) under `0`.
///
/// Because the flavour is threaded through the *encoder*, `compressed` (which deflates
/// exactly these bytes) combines with any minor version.
///
/// # Errors
///
/// The encoder's errors: `unsupported_external_term` for a tag the codec cannot write
/// (an internal pid/port id with no resolvable local node, an off-heap `BinRef`),
/// `SystemLimit` for a value beyond a wire bound, and `error:unsupported_bif` for the
/// local (`make_fun3`) fun that has no faithful wire image.
pub(crate) fn encode_body(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: &T2bOptions,
) -> Result<Vec<u8>> {
    Ok(term_encoder(ctx, term, options)?.finish())
}
/// Builds the term encoder for `term` under `options`, over `ctx`'s atom table.
///
/// Factored out of [`encode_body`] so the `local` path can consume the encoder through
/// [`Encoder::finish_local`] (the `LOCAL_EXT` wrapper) instead of [`Encoder::finish`]
/// (the plain image).
///
/// It is an [`Encoder::new`], not an [`Encoder::new_body`]: the codec writes
/// `VERSION_MAGIC` into its writer *before* the term body, so [`Encoder::finish`]
/// already **is** the whole `[131][body]` image. Adding the version byte afterwards
/// would mean a second image-sized `Vec` (or an `O(n)` memmove), which is the one
/// allocation this shape exists to avoid — the bytes are identical either way.
///
/// The context's local node identity, its `local` node hash and the local-fun identity
/// all borrow `ctx` mutably, while the encoder holds an immutable borrow of `ctx`'s
/// atom table for its whole lifetime — so the mutably-read values are taken **first**
/// and installed on the encoder afterwards.
///
/// * `options.deterministic` installs [`Encoder::with_deterministic`] (a map's pairs
///   are emitted in full Erlang term order on the key — the `DFLAG_DETERMINISTIC`
///   behaviour for **both** a ≤ 32-pair flat map and a > 32-pair HAMT
///   (`external.c:3757-3764`, `:3783-3807`); the flag changes neither the tag set nor
///   the encoded length, so the `erlang:external_size/2` probe stays correct).
/// * `options.local` installs [`Encoder::with_local_ext`], so an internal pid/port/ref
///   writes an `NIL_EXT` (`106`) node marker instead of a sysname atom
///   (`external.c:2978`/`:5589`) — it needs no node identity. Otherwise the context's
///   [`BifContext::local_node`] identity (when resolvable) is installed, so an internal
///   id becomes the oracle's `NEW_PID_EXT`/`V4_PORT_EXT` (and an internal reference its
///   `NEWER_REFERENCE_EXT`).
/// * The context's [`BifContext::local_fun_identity`] (when resolvable) is installed so
///   a local (`make_fun3`) fun is written as its `NEW_FUN_EXT` image.
///
/// # Errors
///
/// Propagates the local-fun-identity hook's failure.
fn term_encoder<'c>(
    ctx: &'c mut BifContext<'_>,
    term: Eterm,
    options: &T2bOptions,
) -> Result<Encoder<'c>> {
    let local = ctx.local_node();
    let identity = ctx.local_fun_identity(term)?;
    // `Encoder::new` writes the version byte into the writer first, so finishing the
    // encoder yields the whole image (see this function's docs); the selected minor
    // version reproduces `term_to_binary`'s tag set exactly.
    let minor_version = match options.minor_version {
        0 => MinorVersion::V0,
        1 => MinorVersion::V1,
        _ => MinorVersion::V2,
    };
    let mut encoder = Encoder::new(ctx.atoms()).with_minor_version(minor_version);
    if options.deterministic {
        encoder = encoder.with_deterministic();
    }
    if options.local {
        // The `[local]` marker wins over any local-node identity (an internal id's node
        // field is then the `NIL_EXT` marker), so it needs no sysname/creation.
        encoder = encoder.with_local_ext();
    } else if let Some((node, creation)) = local {
        encoder = encoder.with_local_node(node, creation);
    }
    if let Some(identity) = identity {
        encoder = encoder.with_fun_identity(identity);
    }
    encoder.encode(term)?;
    Ok(encoder)
}
/// The C's compressed-or-revert rule (`external.c:2250-2301`).
///
/// Takes the **uncompressed image** `[VERSION_MAGIC][body]` by value (the encoder's own
/// buffer — see [`encode_body`]) and deflates its body at `level`: the
/// `[131][80][body_len:4 BE][zlib]` image is emitted only when the stream fits the
/// `body_len - 5` budget, otherwise the image it was given is handed straight back —
/// the same bytes, with **no** copy and no second image-sized allocation (which is what
/// happens for every small body, zlib's own framing costing more than the body saves).
///
/// The `local` caller passes its `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]` image:
/// the C deflates that whole wrapper (`external.c:2265-2298`), i.e. exactly "the image
/// minus its version byte", which is what the slice below is.
///
/// # Errors
///
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) for a body that
/// does not fit the 4-byte size field (unreachable in practice: the term would have
/// to be larger than 4 GiB).
fn compressed_image(image: Vec<u8>, level: u8) -> Result<Vec<u8>> {
    // Every caller builds the image through `term_encoder`/`finish_local`, both of which
    // begin with `VERSION_MAGIC`; a missing one would silently deflate a body that
    // still carries its version byte.
    debug_assert_eq!(
        image.first().copied(),
        Some(VERSION_MAGIC),
        "a compressed body is the image minus its version byte"
    );
    let body = &image[1..];
    if body.len() >= COMPRESSED_OVERHEAD {
        if let Ok(deflated) = zlib_compress(body, level) {
            if deflated.len() <= body.len() - COMPRESSED_OVERHEAD {
                let size = u32::try_from(body.len()).map_err(|_| system_limit())?;
                let mut out = Vec::with_capacity(deflated.len() + 6);
                out.push(VERSION_MAGIC);
                out.push(COMPRESSED_EXT);
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(&deflated);
                return Ok(out);
            }
        }
    }
    Ok(image)
}
/// `binary_to_term(Binary)` — trailing bytes are ignored.
fn binary_to_term_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    decode_term_bif(ctx, args[0], false, false)
}

/// `binary_to_term(Binary, Options)` — the `safe` and `used` options.
fn binary_to_term_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut safe = false;
    let mut used = false;
    for item in proper_list(args[1]).ok_or_else(badarg)? {
        if !access::is_atom(item) {
            return Err(badarg());
        }
        match ctx.atoms().name(atom_val(item)) {
            Some("safe") => safe = true,
            Some("used") => used = true,
            _ => return Err(badarg()),
        }
    }
    decode_term_bif(ctx, args[0], safe, used)
}

// A memoized, per-thread **image** of the caller's shared atom table, used as the
// decoder's mutable scratch (maintained by `with_decode_scratch` below, which also
// carries the correctness argument).
//
// `beam_codec`'s decode entry points take `&mut AtomTable` because they intern every
// atom an image carries. The decode path only ever *interns* — no arm of the decoder
// reads a name back — so a copy of the caller's table is exactly the interface
// required: a name already present in the caller's table is returned at the caller's
// own index, and a genuinely new one at `shared.len() + k`, which is precisely the
// index the `AtomInterner` seam predicts (its `base` is the table's length when the
// call started) and therefore the index it assigns when the novel names are committed
// after the decode (see `decode_term_bif`).
//
// Building that copy per call is `O(table)` **allocations** — ~127 us for the
// ~3000-atom table of a booted runtime, which is the whole cost of
// `binary_to_term/1,2` (the term itself decodes in ~1 us). The copy is therefore
// memoized per thread: it is kept **exact** (the decoder's appends are copied back in)
// and validated against the live table through an O(1) *witness*
// ([`beam_term::AtomSnapshot`] + [`AtomTable::appended_since`]) held beside it, so the
// steady state reads no atom name at all. The index-by-index comparison survives only as
// the fallback for a table the witness cannot vouch for (see [`with_decode_scratch`]).
//
// This is a *cache of the caller's own immutable input*, carrying no cross-call
// semantics; it retains one table image per thread while the process lives.
thread_local! {
    static DECODE_SCRATCH: RefCell<Option<DecodeScratch>> = const { RefCell::new(None) };
}

/// The per-thread decode memo: the scratch image plus the O(1) witness that proves it is
/// still an image of the *shared* table it was taken from.
struct DecodeScratch {
    /// The decoder's mutable scratch. It names the same atom as `witness`'s table at
    /// every index below `witness.len()` (the decoder may append beyond that).
    memo: AtomTable,
    /// A snapshot of the **shared** table — never of `memo`, which is a clone and so
    /// carries a fresh identity that can never satisfy
    /// [`AtomTable::appended_since`] — taken at the last moment `memo` was known to be
    /// index-identical to it over `0..witness.len()`.
    witness: beam_term::AtomSnapshot,
}

// Test-only scratch-machinery metrics for this thread: `(name_comparisons, rebuilds)`.
//
// `name_comparisons` counts the `AtomTable::name` reads the scratch machinery performs
// (one per index compared by [`prefix_matches`], one per index copied by the extend
// loop); `rebuilds` counts the full `AtomTable::clone` rebuilds. Both are read by
// `mod scratch_pins` and are a no-op in a production build (see [`bump_name_comparisons`]
// and [`bump_rebuild`]) — no production behaviour depends on them.
#[cfg(test)]
thread_local! {
    static DECODE_SCRATCH_METRICS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// Adds `n` to this thread's scratch-machinery name-read count (see
/// [`DECODE_SCRATCH_METRICS`]).
#[cfg(test)]
fn bump_name_comparisons(n: usize) {
    DECODE_SCRATCH_METRICS.with(|metrics| {
        let (comparisons, rebuilds) = metrics.get();
        metrics.set((comparisons + n, rebuilds));
    });
}

/// Production build: the name-read count is not instrumented.
#[cfg(not(test))]
#[inline(always)]
fn bump_name_comparisons(_n: usize) {}

/// Counts one scratch rebuild — a full `AtomTable::clone` of the shared table — on this
/// thread (see [`DECODE_SCRATCH_METRICS`]).
#[cfg(test)]
fn bump_rebuild() {
    DECODE_SCRATCH_METRICS.with(|metrics| {
        let (comparisons, rebuilds) = metrics.get();
        metrics.set((comparisons, rebuilds + 1));
    });
}

/// Production build: the rebuild count is not instrumented.
#[cfg(not(test))]
#[inline(always)]
fn bump_rebuild() {}

/// Whether `memo` names the same atom as `shared` at every index `memo` holds.
///
/// `memo.len() > shared.len()` is `false` by definition: such a memo cannot stand in
/// for `shared` (its novel-atom prediction would be off by the difference), which is
/// also why the check never touches a name in that case.
///
/// This is the Θ(table) **fallback** of [`with_decode_scratch`] (allocation-free, but
/// one name read per index); the steady state is proven in O(1) by that function's
/// witness instead.
fn prefix_matches(memo: &AtomTable, shared: &AtomTable) -> bool {
    if memo.len() > shared.len() {
        return false;
    }
    (0..memo.len()).all(|index| {
        bump_name_comparisons(1);
        let atom = Atom::from_raw(index as u32);
        memo.name(atom) == shared.name(atom)
    })
}

/// Runs `f` on an exact image of `shared`, reusing/extending/rebuilding the per-thread
/// memo as needed (see [`DECODE_SCRATCH`]).
///
/// `f` receives a table that is index-identical to `shared`, and may extend it (that is
/// what the decoder does with a novel atom); the memo is *not* required to stay an
/// image of `shared` afterwards, so the next call revalidates. A re-entrant call (which
/// the decode path cannot produce — decoding never dispatches a BIF) falls back to a
/// private copy rather than panicking.
///
/// # The witness protocol
///
/// The memo carries a [`beam_term::AtomSnapshot`] of the **shared** table, taken from
/// `shared` itself — never from the memo, which is a clone and therefore has a fresh
/// identity that can never satisfy [`AtomTable::appended_since`] — at the last moment the
/// memo was known to name the same atom as `shared` at every index below the witness's
/// length. The invariant maintained across calls is exactly that: over
/// `0..min(memo.len(), witness.len())` the memo and the shared table agree index for
/// index (`AtomTable` has no reorder/remove API, so the decoder's own appends cannot
/// break it).
///
/// Under that invariant the **fast path** is O(1) + O(delta) and reads no atom name:
/// `memo.len() == witness.len()` together with `shared.appended_since(witness)` proves
/// that `shared` still names the same atom at every index the memo covers (same instance,
/// and every mutation since the witness was a pure append), so the memo may simply be
/// extended with `shared`'s names at `witness.len()..shared.len()` — normally none, and
/// never more than the table has actually grown.
///
/// Every other shape falls back to the index-by-index [`prefix_matches`]: a different or
/// replaced table, a clone (what a copy-on-write replacement of `shared` presents), a
/// shrunken table, or a memo left **longer** than `shared` by an earlier failed/`safe`
/// decode (whose novel-atom prediction would be off by the difference — such a memo is
/// rejected by a length check alone, without reading a name). A memo that fails that scan
/// is discarded and rebuilt from `shared`.
///
/// Either way the memo ends up exactly `shared`, so the witness is refreshed from
/// `shared` before `f` runs; a decode that appends to the memo merely leaves it longer
/// than the witness, which the next call's fast-path guard refuses.
fn with_decode_scratch<R>(shared: &AtomTable, f: impl FnOnce(&mut AtomTable) -> R) -> R {
    DECODE_SCRATCH.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return f(&mut shared.clone());
        };
        let usable = slot.as_ref().is_some_and(|scratch| {
            // O(1): same instance as the witness, pure appends since, and the memo is
            // exactly as long as the witness vouches for.
            (scratch.memo.len() == scratch.witness.len()
                && shared.appended_since(scratch.witness))
                // Θ(table) fallback: the memo names the same atom as `shared` index for
                // index, and is no longer than it.
                || prefix_matches(&scratch.memo, shared)
        });
        if !usable {
            bump_rebuild();
            *slot = Some(DecodeScratch {
                memo: shared.clone(),
                witness: shared.snapshot(),
            });
        }
        let scratch = slot.as_mut().expect("installed above");
        // Extend to an exact image of `shared`: the memo already agrees with it over
        // `memo.len()`, and the names at `memo.len()..shared.len()` are `shared`'s own.
        for index in scratch.memo.len()..shared.len() {
            bump_name_comparisons(1);
            if let Some(name) = shared.name(Atom::from_raw(index as u32)) {
                scratch.memo.intern(name);
            }
        }
        // The memo now names exactly what `shared` holds, so witness it: that is what
        // lets the next call prove index-identity in O(1) instead of scanning.
        scratch.witness = shared.snapshot();
        f(&mut scratch.memo)
    })
}

/// Deterministic pins on the **O(1)** witness-guarded decode scratch: how many
/// `AtomTable::name` reads (and full rebuilds) the scratch machinery performs per call,
/// counted with the test-only metrics above — never wall-clock time.
///
/// The counters are thread-local and every `#[test]` runs on its own thread, so each case
/// starts from a cold memo unless it deliberately warms one. Each case also asserts the
/// atom index the decoded term resolves to, so no pin can be satisfied by a memo that is
/// fast *and wrong*. The shapes a blind reuse would corrupt (a moved name, a shrunken
/// table, a memo left ahead by a failed/`safe` decode) are also pinned by
/// `tests/decode_scratch.rs`, which carries them across calls on the public BIF surface.
#[cfg(test)]
mod scratch_pins {
    use super::*;
    use crate::registry::{default_registry, BifRegistry};
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// `binary_to_term(<<131,119,3,102,111,111>>)` — the atom `foo`.
    const FOO_BLOB: &[u8] = &[131, 119, 3, 102, 111, 111];

    /// `binary_to_term(<<131,116,0,0,0,1,119,1,97,97,1>>)` — the map `#{a => 1}`, whose
    /// key is an already-interned atom (so this image interns nothing).
    const MAP_A_BLOB: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];

    /// The names every pin table is seeded with, in this order, so index 0 is `a` and
    /// index 3 is `foo`.
    const NAMES: &[&str] = &["a", "b", "c", "foo"];

    /// An [`AtomInterner`](crate::context::AtomInterner) over a *clone* of the context's
    /// table — the shape `beam_rt`'s pending interner takes (it predicts the context
    /// table's next indices and commits them in order).
    struct Interner {
        table: Rc<RefCell<AtomTable>>,
    }

    impl crate::context::AtomInterner for Interner {
        fn intern(&mut self, name: &str) -> Atom {
            self.table.borrow_mut().intern(name)
        }
    }

    fn table(names: &[&str]) -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(names.iter().copied());
        atoms
    }

    fn heap() -> Heap {
        Heap::new(1 << 16).expect("test heap")
    }

    /// `(name comparisons, rebuilds)` the scratch machinery performed on this thread.
    fn metrics() -> (usize, usize) {
        DECODE_SCRATCH_METRICS.with(std::cell::Cell::get)
    }

    /// Zeroes this thread's scratch-machinery counters.
    fn reset_metrics() {
        DECODE_SCRATCH_METRICS.with(|metrics| metrics.set((0, 0)));
    }

    /// Decodes `blob` under a context borrowing `atoms` (with an interner over a clone of
    /// it, so a novel name is committed at the index the decoder wrote), returning the
    /// decoded term and the interner's table.
    fn decode(
        reg: &BifRegistry,
        atoms: &AtomTable,
        heap: &mut Heap,
        blob: &[u8],
    ) -> beam_common::Result<(Eterm, Rc<RefCell<AtomTable>>)> {
        let shared = Rc::new(RefCell::new(atoms.clone()));
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx =
            BifContext::new(heap, atoms, ProcessId::new(1, 0)).with_atom_interner(&mut interner);
        let binary = bits::make_binary(ctx.heap(), blob).expect("the blob fits");
        let term = reg.call("erlang", "binary_to_term", 1, &mut ctx, &[binary])?;
        Ok((term, shared))
    }

    /// Decodes `FOO_BLOB` and returns the raw atom index the term resolves to.
    fn decode_foo(reg: &BifRegistry, atoms: &AtomTable, heap: &mut Heap) -> u32 {
        let (term, _interner_table) = decode(reg, atoms, heap, FOO_BLOB).expect("it decodes");
        assert!(beam_term::is_atom(term), "the decode yields an atom");
        atom_val(term).raw()
    }

    #[test]
    fn the_cold_call_witnesses_a_fresh_memo_and_the_steady_state_reads_no_name() {
        let reg = default_registry();
        let atoms = table(NAMES);
        let mut heap = heap();

        // Cold: no memo yet, so it is built by one `AtomTable::clone` — Θ(table)
        // *allocations* — and not one name is read.
        reset_metrics();
        assert_eq!(decode_foo(&reg, &atoms, &mut heap), 3, "`foo` is index 3");
        assert_eq!(
            metrics(),
            (0, 1),
            "the cold call rebuilds (clones) without reading a name"
        );

        // Steady state: the same shared table, so the witness proves index-identity in
        // O(1) — no name read, no rebuild.
        reset_metrics();
        assert_eq!(decode_foo(&reg, &atoms, &mut heap), 3);
        assert_eq!(metrics(), (0, 0), "the steady state must be O(1)");
    }

    #[test]
    fn a_replaced_table_instance_costs_one_theta_table_scan_and_heals() {
        let reg = default_registry();
        let first = table(NAMES);
        let mut heap = heap();
        assert_eq!(decode_foo(&reg, &first, &mut heap), 3);

        // The same names in the same order but a **different instance** — exactly what a
        // copy-on-write replacement of the runtime's shared table presents. A clone has a
        // fresh identity by design, so the witness cannot vouch for it and the Θ(table)
        // fallback runs, comparing every index exactly once (`(0, 1)` or `(0, 0)` would
        // mean the witness had vouched for a table it never saw).
        let second = first.clone();
        reset_metrics();
        assert_eq!(decode_foo(&reg, &second, &mut heap), 3);
        assert_eq!(
            metrics(),
            (second.len(), 0),
            "the fallback compares every index exactly once and needs no rebuild"
        );

        // That fallback re-witnessed the memo for `second`, so the next call is O(1).
        reset_metrics();
        assert_eq!(decode_foo(&reg, &second, &mut heap), 3);
        assert_eq!(metrics(), (0, 0), "the fallback self-heals");
    }

    #[test]
    fn an_appended_table_is_extended_by_reading_only_the_appended_names() {
        let reg = default_registry();
        let mut atoms = table(NAMES);
        let mut heap = heap();
        assert_eq!(decode_foo(&reg, &atoms, &mut heap), 3);

        // The **same** table instance, grown in place by two appends: `appended_since`
        // holds, so the memo stays valid over its whole existing prefix and only the two
        // appended names are copied.
        atoms.seed(["g1", "g2"]);
        reset_metrics();
        assert_eq!(decode_foo(&reg, &atoms, &mut heap), 3, "`foo` stays at 3");
        assert_eq!(
            metrics(),
            (2, 0),
            "exactly the appended names are read — nothing is rebuilt"
        );
    }

    #[test]
    fn a_table_that_moves_a_name_forces_the_fallback_and_a_rebuild() {
        let reg = default_registry();
        let first = table(NAMES); // foo @3
        let mut heap = heap();
        assert_eq!(decode_foo(&reg, &first, &mut heap), 3);

        // A table naming `foo` at index 5 instead (index 3 is `d` there). A blind reuse
        // would resolve the decoded term to `d`; the fallback must reject the memo at the
        // first differing index and rebuild it from the new table.
        let moved = table(&["a", "b", "c", "d", "e", "foo"]);
        reset_metrics();
        assert_eq!(decode_foo(&reg, &moved, &mut heap), 5, "`foo` is index 5 there");
        assert_eq!(
            metrics(),
            (4, 1),
            "the scan stops at the first differing index (3: `foo` vs `d`) and rebuilds"
        );
    }

    #[test]
    fn a_memo_left_longer_than_the_live_table_is_rebuilt_without_a_name_read() {
        let reg = default_registry();
        let atoms = table(&["a", "b", "c"]); // no `foo`
        let mut heap = heap();

        // `foo` is absent, so the decoder appends it to the scratch and the interner
        // commits it to the interner's table at exactly `atoms.len()`; the context's own
        // table is untouched, which leaves the memo one name longer than `shared`.
        let (term, interner_table) = decode(&reg, &atoms, &mut heap, FOO_BLOB).expect("it decodes");
        assert_eq!(atom_val(term).raw(), 3, "the fresh atom lands at atoms.len()");
        assert_eq!(interner_table.borrow().len(), 4, "committed at index 3");
        assert_eq!(atoms.len(), 3, "the context's table is untouched");

        // A memo that is *longer* than `shared` cannot stand in for it (its novel-atom
        // prediction would be off by the difference), so it is rejected by the length
        // check alone — no name read — and rebuilt.
        reset_metrics();
        assert_eq!(decode_foo(&reg, &atoms, &mut heap), 3, "still `atoms.len()`");
        assert_eq!(
            metrics(),
            (0, 1),
            "the too-long memo is rebuilt, with no name read to reject it"
        );
    }

    #[test]
    fn an_unknown_then_known_atom_sequence_keeps_the_written_indices() {
        let reg = default_registry();
        let atoms = table(&["a", "b", "c"]); // no `foo`, `a` present
        let mut heap = heap();

        // Unknown: `foo` is appended at `atoms.len()` and committed there.
        let (fresh, table) = decode(&reg, &atoms, &mut heap, FOO_BLOB).expect("it decodes");
        assert_eq!(atom_val(fresh).raw(), 3);
        assert_eq!(table.borrow().name(Atom::from_raw(3)), Some("foo"));

        // Known: the very next decode interns nothing, and the memo it rebuilds from the
        // (unchanged) shared table still resolves `a` at index 0 — the fresh name did not
        // shift or alias any existing index.
        reset_metrics();
        let (map, _table) = decode(&reg, &atoms, &mut heap, MAP_A_BLOB).expect("it decodes");
        let pairs = beam_term::map::map_pairs(map, &atoms).expect("a flat map");
        assert_eq!(pairs.len(), 1);
        assert_eq!(atom_val(pairs[0].0).raw(), 0, "`a` is index 0");
        assert_eq!(pairs[0].1, beam_term::make_small(1));
        assert_eq!(
            metrics(),
            (0, 1),
            "the too-long memo was rebuilt, reading no name"
        );
    }
}

/// Decodes an external term image (see the module docs for the atom-index
/// coupling this relies on).
///
/// The `[used]` count comes from the codec's own `[used]` entry points, which handle
/// the top-level `COMPRESSED` (`80`) wrapper — the form `term_to_binary/2` with a
/// `compressed` option produces — internally (`external.c:1754-1816`): a compressed
/// image reports its own length, so bytes after the zlib stream are ignored *and*
/// uncounted (`external.c:1797-1801`), while a plain image reports the term's own end
/// and tolerates trailing bytes, like the oracle.
///
/// The `[safe]` flag is fed into the codec through its [`DecodeOptions`] seam: with it
/// set a wire atom **name** that is not already interned in the context's table is
/// `BadArg` **from the codec**, raised **before** anything is interned (C
/// `erts_atom_get` instead of `erts_atom_put`, `external.c:4530-4539`), so the shared
/// atom table cannot grow. The flag-free path is unchanged: the names the decoder
/// appends to the scratch are still committed to the shared table afterwards.
///
/// The context's local node identity ([`BifContext::local_node`]) is installed on the
/// decoder, so a wire pid/port/reference whose node atom and creation name this node
/// materializes as this runtime's **internal** form — the C's `dec_is_this_node`
/// branches (`external.c:3175-3182` pid, `:4721` port, `:4821-4862` ref) — and
/// `binary_to_term(term_to_binary(self())) =:= self()` holds. With no resolvable
/// identity the decode is byte-for-byte the previous behaviour (the wire form, an
/// external id). The 4-byte `LOCAL_EXT` node hash is runtime state with no seam here,
/// so it is not installed (`None` consumes it unchecked).
///
/// The body is decoded into [`DECODE_SCRATCH`], an exact, memoized image of the
/// context's table — never a fresh `O(table)` clone per call.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary, a
/// malformed/truncated image, an unknown option, or `safe` with a wire atom name that
/// is not already interned; the codec's `unsupported_external_term` raise for a tag
/// it cannot build (funs, exports, records, `LOCAL`). A local reference id that
/// does not fit the M1 internal-ref block's 60-bit slot is not an error: the
/// decoder keeps the faithful external reference form for it.
fn decode_term_bif(
    ctx: &mut BifContext<'_>,
    input: Eterm,
    safe: bool,
    used: bool,
) -> Result<Eterm> {
    let data = bytes_arg(input)?;
    // Read the local node identity *before* `ctx.heap()` — the accessor takes
    // `&mut self`, so it cannot be called while the heap is borrowed. It is installed
    // on the decoder through the codec's option seam so an incoming identifier that
    // names this node demotes to the internal form (see the docs above); `None` leaves
    // the decode unchanged. `local_hash` stays `None`: the 4-byte `LOCAL_EXT` node hash
    // is runtime state with no seam in this crate (see the docs above).
    let identity = ctx.local_node();
    // Feed `[safe]` (and the resolved identity) into the codec: under `safe` a wire atom
    // **name** that is not already interned is `BadArg` *from the codec* and **nothing is
    // interned** (C `erts_atom_get`), so the fresh-name commit below is unreachable under
    // `safe` — it stays as the flag-free path's commit.
    let options = DecodeOptions {
        safe,
        local_node: identity,
        local_hash: None,
    };
    // Decode into the scratch image and collect the names it appended, before the
    // scratch is released — they are exactly the names that must reach the shared
    // table, at exactly the indices the decoder wrote into the term.
    let (decoded, fresh) = {
        let (shared, heap) = ctx.atom_and_heap();
        with_decode_scratch(shared, |scratch| {
            let known = scratch.len();
            // The codec's option-aware `[used]` entry point owns the `COMPRESSED` wrapper
            // handling, the consumed-source-byte count (the zlib stream end for a compressed
            // image, `Decoder::position` otherwise), the local-node identity install and the
            // `[safe]` refusal — and, unlike the `finish()`-ing `decode_term` pair, it
            // tolerates trailing bytes, matching the oracle's `binary_to_term/2`. With
            // `safe` false its decode is byte-for-byte the flag-free/identity-aware twins'.
            let decoded = decode_term_with_options_used(&data, heap, scratch, &options);
            // A failed decode leaves the scratch possibly ahead of the shared table (the
            // next call's `prefix_matches` rebuilds it); nothing is committed.
            let fresh: Vec<String> = if decoded.is_ok() {
                (known..scratch.len())
                    .filter_map(|index| {
                        scratch
                            .name(Atom::from_raw(index as u32))
                            .map(str::to_string)
                    })
                    .collect()
            } else {
                Vec::new()
            };
            (decoded, fresh)
        })
    };
    let (term, position) = decoded?;
    if !fresh.is_empty() {
        // Unreachable under `[safe]` (the codec refuses an unknown name and interns
        // nothing, so `fresh` is empty), kept as a guard should a future codec change
        // ever append a name under `safe`.
        if safe {
            return Err(badarg());
        }
        for name in &fresh {
            ctx.intern_atom(name)?;
        }
    }
    if used {
        let count = ctx.int(i64::try_from(position).map_err(|_| system_limit())?)?;
        ctx.tuple(&[term, count])
    } else {
        Ok(term)
    }
}
/// Registers the `erlang` binary/atom conversion and term (de)serialization BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "list_to_binary", 1, list_to_binary_1);
    reg.register("erlang", "binary_to_list", 1, binary_to_list_1);
    reg.register("erlang", "binary_to_list", 3, binary_to_list_3);
    reg.register("erlang", "byte_size", 1, byte_size_1);
    reg.register("erlang", "bit_size", 1, bit_size_1);
    reg.register("erlang", "binary_part", 2, binary_part_2);
    reg.register("erlang", "binary_part", 3, binary_part_3);
    reg.register("erlang", "integer_to_binary", 1, integer_to_binary_1);
    reg.register("erlang", "integer_to_binary", 2, integer_to_binary_2);
    reg.register("erlang", "binary_to_integer", 1, binary_to_integer_1);
    reg.register("erlang", "binary_to_integer", 2, binary_to_integer_2);
    reg.register("erlang", "float_to_binary", 1, float_to_binary_1);
    reg.register("erlang", "float_to_binary", 2, float_to_binary_2);
    reg.register("erlang", "binary_to_float", 1, binary_to_float_1);
    reg.register("erlang", "atom_to_binary", 1, atom_to_binary_1);
    reg.register("erlang", "atom_to_binary", 2, atom_to_binary_2);
    reg.register("erlang", "binary_to_atom", 1, binary_to_atom_1);
    reg.register("erlang", "binary_to_atom", 2, binary_to_atom_2);
    reg.register("erlang", "binary_to_existing_atom", 1, binary_to_existing_atom_1);
    reg.register("erlang", "binary_to_existing_atom", 2, binary_to_existing_atom_2);
    reg.register("erlang", "term_to_binary", 1, term_to_binary_1);
    reg.register("erlang", "term_to_binary", 2, term_to_binary_2);
    reg.register("erlang", "binary_to_term", 1, binary_to_term_1);
    reg.register("erlang", "binary_to_term", 2, binary_to_term_2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_codec::error::is_unsupported;
    use beam_codec::term::external_pid_parts;
    use beam_process::{PortId, ProcessId};
    use beam_term::AtomTable;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The names every test table is seeded with, **in this order** — a clone then
    /// has identical indices, which is what [`decode_term_bif`] relies on, and the
    /// option atoms (`latin1`, `minor_version`, …) must be present for the BIFs to
    /// recognise them.
    const NAMES: &[&str] = &[
        "true",
        "false",
        "ok",
        "a",
        "b",
        "hello",
        "abc",
        "\u{c8}",
        "nonode@nohost",
        "latin1",
        "utf8",
        "unicode",
        "compressed",
        "minor_version",
        "safe",
        "used",
        "decimals",
        "scientific",
        "compact",
        "short",
        "",
        "\u{3c0}",
        // The `term_to_binary/2` option atoms the previous group deliberately left
        // out (`local`/`deterministic` select a flag, `iovec`/`fragment` are the
        // oracle's `badarg` for a caller that can receive neither). Appending keeps
        // every earlier index — and therefore every clone-coupled decode — stable.
        "deterministic",
        "local",
        "iovec",
        "fragment",
        // Two atoms whose **atom-index** order (`zz` before `aa`, the order they are
        // seeded here) is the reverse of their **name** order — the only way to make a
        // flat map's stored `erts_cmp_flatmap_keys` order differ from the
        // `[deterministic]` (`cmp`) order, i.e. to exercise the re-sort.
        "zz",
        "aa",
    ];

    /// `self()` — `NEW_PID_EXT` on `nonode@nohost`, id 10, serial/creation 0 (the
    /// vector `beam_codec/tests/ground_truth.rs` pins).
    const PID_SELF: &[u8] = &[
        131, 88, 119, 13, 110, 111, 110, 111, 100, 101, 64, 110, 111, 104, 111, 115, 116, 0, 0, 0,
        10, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    /// `<<"hi">>` — its `BINARY_EXT` image, i.e. `term_to_binary(<<"hi">>)`.
    const BINARY_HI: &[u8] = &[131, 109, 0, 0, 0, 2, 104, 105];

    fn table() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(NAMES.iter().copied());
        atoms
    }

    fn heap() -> Heap {
        Heap::new(8192).expect("test heap")
    }

    /// Runs `f` over a fresh context (no seams installed).
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = heap();
        let atoms = table();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// An [`AtomInterner`](crate::context::AtomInterner) over a *clone* of the
    /// context table, so a name it interns lands at exactly the index the context's
    /// own table would have assigned it.
    struct Interner {
        table: Rc<RefCell<AtomTable>>,
    }

    impl crate::context::AtomInterner for Interner {
        fn intern(&mut self, name: &str) -> Atom {
            self.table.borrow_mut().intern(name)
        }
    }

    /// Runs `f` over a fresh context with the atom interner installed, handing back
    /// the table the interner owns (used to read back a freshly interned name).
    fn with_interner<R>(f: impl FnOnce(&mut BifContext<'_>, &Rc<RefCell<AtomTable>>) -> R) -> R {
        with_interner_over(table(), f)
    }

    /// Runs `f` over a fresh context whose atom table is *exactly* `atoms` — so the
    /// local-node fallback ([`BifContext::local_node`]) stays unresolved when `atoms`
    /// lacks `nonode@nohost` — with an atom interner installed over a clone of it (the
    /// shape [`decode_term_bif`]'s fresh-name handling relies on).
    fn with_interner_over<R>(
        atoms: AtomTable,
        f: impl FnOnce(&mut BifContext<'_>, &Rc<RefCell<AtomTable>>) -> R,
    ) -> R {
        let mut heap = heap();
        let shared = Rc::new(RefCell::new(atoms.clone()));
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        f(&mut ctx, &shared)
    }

    // A per-thread scratch heap used *only* to build argument terms. Keeping it
    // separate from the context's own heap lets `bin(..)`/`bitstring(..)` be called
    // inside an expression that already borrows the context (`ctx.list(&[bin(..)])`)
    // — a single `&mut BifContext` would forbid that double borrow. Each `#[test]`
    // runs on its own thread, so the heap is per-test and never shared across tests.
    thread_local! {
        static SCRATCH: RefCell<Heap> = RefCell::new(Heap::new(1 << 20).expect("scratch heap"));
    }

    /// A binary term of `bytes`, allocated on the per-thread scratch heap.
    fn bin(bytes: &[u8]) -> Eterm {
        SCRATCH.with(|h| bits::make_binary(&mut h.borrow_mut(), bytes).expect("the binary fits"))
    }

    /// A (possibly non-byte-aligned) bitstring term on the per-thread scratch heap.
    fn bitstring(data: &[u8], bits: usize) -> Eterm {
        SCRATCH.with(|h| bits::make_bitstring(&mut h.borrow_mut(), data, bits).expect("fits"))
    }

    /// The bytes of a binary term.
    fn text(term: Eterm) -> Vec<u8> {
        bits::bytes(term).expect("a byte-aligned binary")
    }

    /// The small integers of a proper list.
    fn values(term: Eterm) -> Vec<i64> {
        proper_list(term)
            .expect("a proper list")
            .into_iter()
            .map(signed_value)
            .collect()
    }

    /// The small integer a BIF returned.
    fn int_of(term: Eterm) -> i64 {
        assert!(access::is_small(term), "not a small integer");
        signed_value(term)
    }

    /// The float value a BIF returned.
    fn float_of(term: Eterm) -> f64 {
        assert!(access::is_float(term), "not a float");
        access::float_value(term)
    }

    /// The option list `[{Name, Value}]` for a `term_to_binary/2` call.
    fn option_tuple(ctx: &mut BifContext<'_>, name: &str, value: Eterm) -> Eterm {
        let pair = ctx
            .tuple(&[ctx.atom(name).expect("seeded"), value])
            .expect("fits");
        ctx.list(&[pair]).expect("fits")
    }

    /// The option list `[compressed]`.
    fn compressed_option(ctx: &mut BifContext<'_>) -> Eterm {
        ctx.list(&[ctx.atom("compressed").expect("seeded")])
            .expect("fits")
    }

    /// The option list `[used]`.
    fn used_option(ctx: &mut BifContext<'_>) -> Eterm {
        ctx.list(&[ctx.atom("used").expect("seeded")])
            .expect("fits")
    }

    /// The option list `[Name]` for a bare-atom option.
    fn atom_option(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
        ctx.list(&[ctx.atom(name).expect("seeded")]).expect("fits")
    }

    /// The option list `[Name1, Name2]` for two bare-atom options.
    fn atom_options(ctx: &mut BifContext<'_>, first: &str, second: &str) -> Eterm {
        let first = ctx.atom(first).expect("seeded");
        let second = ctx.atom(second).expect("seeded");
        ctx.list(&[first, second]).expect("fits")
    }

    /// Asserts `result` is `badarg`.
    fn assert_badarg(result: Result<Eterm>) {
        assert!(
            matches!(result, Err(beam_common::BeamError::BadArg)),
            "expected badarg, got {result:?}"
        );
    }

    /// Whether `err` is this crate's typed `unsupported_bif` capability gap.
    fn is_unsupported_bif(err: &beam_common::BeamError) -> bool {
        matches!(
            err,
            beam_common::BeamError::Raise(raise)
                if raise.reason.as_atom() == Some("unsupported_bif")
        )
    }

    // -- live oracle ------------------------------------------------------

    /// `$ERTS_ORACLE` if it names an existing file, else `erl` on `$PATH` — the same
    /// resolution `src/numeric.rs`, `src/ets/specs.rs` and `tests/checksum.rs` use.
    fn resolve_oracle() -> Option<std::path::PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = std::path::PathBuf::from(&value);
            if program.is_file() {
                return Some(program);
            }
            eprintln!(
                "[beam_bifs binconv] $ERTS_ORACLE='{}' is not an existing file — \
                 treating it as no oracle.",
                program.display()
            );
            return None;
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| candidate.is_file())
    }

    /// Runs `eval` under the oracle (`erl -noshell -eval <eval> -s init stop`) and
    /// returns its stdout as lines.
    ///
    /// `None` means there is **no usable oracle** (the same convention
    /// [`resolve_oracle`] uses): a loud notice is printed and the caller skips — never a
    /// silent pass. A **spawned** oracle that fails (non-zero exit) is a test failure,
    /// not a skip.
    fn oracle_stdout(eval: &str) -> Option<Vec<String>> {
        let oracle = resolve_oracle()?;
        let output = std::process::Command::new(&oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(eval)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert!(
            output.status.success(),
            "the oracle '{}' exited with {:?}\n--- stderr ---\n{}",
            oracle.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8(output.stdout).expect("the oracle printed UTF-8");
        Some(stdout.lines().map(str::to_string).collect())
    }

    // -- list_to_binary / binary_to_list ----------------------------------

    #[test]
    fn list_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let digits = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            assert_eq!(text(list_to_binary_1(ctx, &[digits]).unwrap()), vec![1, 2, 3]);

            // An iolist: <<"ab">>, <<"cd">>, [101,[102]], 1 and the improper tail <<2>>.
            let nested = ctx.list(&[ctx.int(102).expect("fits")]).expect("fits");
            let inner = ctx
                .list(&[ctx.int(101).expect("fits"), nested])
                .expect("fits");
            let iolist = ctx
                .list(&[
                    bin(b"ab"),
                    bin(b"cd"),
                    inner,
                    ctx.int(1).expect("fits"),
                ])
                .expect("fits");
            let iolist = ctx.cons(iolist, bin(&[2])).expect("fits");
            assert_eq!(
                text(list_to_binary_1(ctx, &[iolist]).unwrap()),
                vec![97, 98, 99, 100, 101, 102, 1, 2]
            );

            // `[]` is the empty binary.
            let empty = ctx.list(&[]).expect("nil");
            assert!(text(list_to_binary_1(ctx, &[empty]).unwrap()).is_empty());

            // A bare binary is *not* an iolist for `list_to_binary/1`.
            for (i, bad) in [
                bin(&[1, 2]),
                ctx.int(1).expect("fits"),
                ctx.atom("a").expect("seeded"),
                ctx.list(&[ctx.int(256).expect("fits")]).expect("fits"),
                ctx.list(&[ctx.int(-1).expect("fits")]).expect("fits"),
                ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits"),
                // An improper integer tail.
                ctx.cons(ctx.int(1).expect("fits"), ctx.int(2).expect("fits"))
                    .expect("fits"),
                // A non-byte-aligned bitstring element.
                ctx.list(&[bitstring(&[0x80], 1)])
                    .expect("fits"),
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    matches!(
                        list_to_binary_1(ctx, &[bad]),
                        Err(beam_common::BeamError::BadArg)
                    ),
                    "case {i} should be badarg"
                );
            }
        });
    }

    #[test]
    fn binary_to_list_matches_the_oracle() {
        with_ctx(|ctx| {
            let three = bin(&[1, 2, 3]);
            assert_eq!(values(binary_to_list_1(ctx, &[three]).unwrap()), vec![1, 2, 3]);
            // The empty binary is `[]` (`NIL`).
            let empty = bin(&[]);
            assert_eq!(binary_to_list_1(ctx, &[empty]).unwrap(), beam_term::NIL);

            // `binary_to_list/3` is 1-based and inclusive.
            for (start, stop, expected) in [
                (2, 3, vec![2, 3]),
                (1, 1, vec![1]),
                (3, 3, vec![3]),
                (2, 2, vec![2]),
                (1, 3, vec![1, 2, 3]),
            ] {
                let args = [
                    three,
                    ctx.int(start).expect("fits"),
                    ctx.int(stop).expect("fits"),
                ];
                assert_eq!(
                    values(binary_to_list_3(ctx, &args).unwrap()),
                    expected,
                    "binary_to_list(<<1,2,3>>, {start}, {stop})"
                );
            }
            // Out of range, inverted, non-integer and non-binary are `badarg`.
            for (start, stop) in [(4i64, 3i64), (3, 2), (0, 1), (1, 4)] {
                let args = [
                    three,
                    ctx.int(start).expect("fits"),
                    ctx.int(stop).expect("fits"),
                ];
                assert_badarg(binary_to_list_3(ctx, &args));
            }
            let float_start = [
                three,
                ctx.make_float(1.0).expect("fits"),
                ctx.int(3).expect("fits"),
            ];
            assert_badarg(binary_to_list_3(ctx, &float_start));
            // The empty binary accepts no range at all.
            let empty_args = [
                empty,
                ctx.int(1).expect("fits"),
                ctx.int(0).expect("fits"),
            ];
            assert_badarg(binary_to_list_3(ctx, &empty_args));

            // A non-byte-aligned bitstring and a non-bitstring are `badarg`.
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            for bad in [odd, ctx.int(1).expect("fits"), ctx.list(&[]).expect("nil")] {
                assert_badarg(binary_to_list_1(ctx, &[bad]));
            }
        });
    }

    // -- byte_size / bit_size ---------------------------------------------

    #[test]
    fn byte_and_bit_size_match_the_oracle() {
        with_ctx(|ctx| {
            for (bytes, expected) in [
                (&[1u8, 2, 3][..], 3i64),
                (&[][..], 0),
            ] {
                let term = bin(bytes);
                assert_eq!(int_of(bit_size_1(ctx, &[term]).unwrap()), expected * 8);
                assert_eq!(int_of(byte_size_1(ctx, &[term]).unwrap()), expected);
            }
            // `byte_size/1` rounds *up*: 1, 9, 16 and 17 bits are 1, 2, 2 and 3 bytes.
            for (bit_len, expected) in [(1usize, 1i64), (9, 2), (16, 2), (17, 3), (0, 0)] {
                let data = vec![0xFFu8; bit_len.div_ceil(8).max(1)];
                let term = bits::make_bitstring(ctx.heap(), &data, bit_len).expect("fits");
                assert_eq!(
                    int_of(bit_size_1(ctx, &[term]).unwrap()),
                    bit_len as i64,
                    "bit_size(<<..:{bit_len}>>)"
                );
                assert_eq!(
                    int_of(byte_size_1(ctx, &[term]).unwrap()),
                    expected,
                    "byte_size(<<..:{bit_len}>>)"
                );
            }
            // A non-bitstring is `badarg`.
            for bad in [
                ctx.int(1).expect("fits"),
                ctx.atom("a").expect("seeded"),
                ctx.list(&[ctx.int(1).expect("fits")]).expect("fits"),
            ] {
                assert_badarg(bit_size_1(ctx, &[bad]));
                assert_badarg(byte_size_1(ctx, &[bad]));
            }
        });
    }

    // -- binary_part ------------------------------------------------------

    #[test]
    fn binary_part_matches_the_oracle() {
        with_ctx(|ctx| {
            let five = bin(&[1, 2, 3, 4, 5]);
            // `binary_part/3`: `Start` non-negative, `Len` signed.
            for (start, len, expected) in [
                (1i64, 2i64, vec![2, 3]),
                (5, 0, vec![]),
                (4, -2, vec![3, 4]),
                (2, -1, vec![2]),
                (0, 0, vec![]),
                (0, 5, vec![1, 2, 3, 4, 5]),
                (1, -1, vec![1]),
            ] {
                let args = [
                    five,
                    ctx.int(start).expect("fits"),
                    ctx.int(len).expect("fits"),
                ];
                assert_eq!(
                    text(binary_part_3(ctx, &args).unwrap()),
                    expected,
                    "binary_part(<<1,2,3,4,5>>, {start}, {len})"
                );
            }
            // `binary_part/2` destructures a 2-tuple the same way.
            let pair = ctx
                .tuple(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            assert_eq!(text(binary_part_2(ctx, &[five, pair]).unwrap()), vec![2, 3]);

            // Out of range, inverted-length and non-integer are `badarg`.
            for (start, len) in [
                (6i64, 0i64),
                (5, 1),
                (-1, 1),
                (0, -1),
                (1, -2),
                (-1, -1),
                (0, 6),
            ] {
                let args = [
                    five,
                    ctx.int(start).expect("fits"),
                    ctx.int(len).expect("fits"),
                ];
                assert_badarg(binary_part_3(ctx, &args));
            }
            let float_len = [
                five,
                ctx.int(1).expect("fits"),
                ctx.make_float(2.0).expect("fits"),
            ];
            assert_badarg(binary_part_3(ctx, &float_len));

            // The 2-tuple form requires exactly two elements.
            let triple = ctx
                .tuple(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            for bad in [triple, ctx.int(1).expect("fits")] {
                assert_badarg(binary_part_2(ctx, &[five, bad]));
            }

            // The subject must be a byte-aligned binary.
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            let nil = ctx.list(&[]).expect("nil");
            let zero = ctx.int(0).expect("fits");
            assert_badarg(binary_part_3(ctx, &[odd, zero, zero]));
            assert_badarg(binary_part_3(ctx, &[nil, zero, zero]));
        });
    }

    // -- integer conversions ----------------------------------------------

    #[test]
    fn integer_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            for (value, expected) in [
                (255i64, "255"),
                (-17, "-17"),
                (0, "0"),
                (17, "17"),
            ] {
                let term = integer_to_binary_1(ctx, &[ctx.int(value).expect("fits")]).unwrap();
                assert_eq!(text(term), expected.as_bytes());
            }
            for (value, base, expected) in [
                (255i64, 16i64, "FF"),
                (255, 2, "11111111"),
                (255, 8, "377"),
                (-255, 36, "-73"),
                (0, 10, "0"),
                (35, 36, "Z"),
            ] {
                let args = [ctx.int(value).expect("fits"), ctx.int(base).expect("fits")];
                let term = integer_to_binary_2(ctx, &args).unwrap();
                assert_eq!(text(term), expected.as_bytes(), "base {base}");
            }
            for base in [1i64, 37, 0, -2] {
                let args = [ctx.int(5).expect("fits"), ctx.int(base).expect("fits")];
                assert_badarg(integer_to_binary_2(ctx, &args));
            }
            let float_base = [ctx.int(5).expect("fits"), ctx.make_float(16.0).expect("fits")];
            assert_badarg(integer_to_binary_2(ctx, &float_base));
            let float = ctx.make_float(5.0).expect("fits");
            assert_badarg(integer_to_binary_1(ctx, &[float]));
        });
    }

    #[test]
    fn binary_to_integer_matches_the_oracle() {
        with_ctx(|ctx| {
            for (input, expected) in [
                (&b"123"[..], 123i64),
                (b"-99", -99),
                (b"+33", 33),
                (b"00123", 123),
                (b"0", 0),
            ] {
                let term = binary_to_integer_1(ctx, &[bin(input)]).unwrap();
                assert_eq!(int_of(term), expected, "binary_to_integer({input:?})");
            }
            for (input, base, expected) in [
                (&b"3FF"[..], 16i64, 1023i64),
                (b"ff", 16, 255),
                (b"Zz", 36, 1295),
                (b"zz", 36, 1295),
                (b"101", 2, 5),
            ] {
                let args = [bin(input), ctx.int(base).expect("fits")];
                let term = binary_to_integer_2(ctx, &args).unwrap();
                assert_eq!(int_of(term), expected, "binary_to_integer({input:?}, {base})");
            }
            for (input, base) in [
                (&b""[..], 10i64),
                (b" ", 10),
                (b"-", 10),
                (b"12.0", 10),
                (b"2", 2),
                (b"1:1", 10),
                (b"\x01\x02", 10),
            ] {
                let args = [bin(input), ctx.int(base).expect("fits")];
                assert_badarg(binary_to_integer_2(ctx, &args));
                if base == 10 {
                    assert_badarg(binary_to_integer_1(ctx, &[bin(input)]));
                }
            }
            for base in [1i64, 37, 0] {
                let args = [bin(b"10"), ctx.int(base).expect("fits")];
                assert_badarg(binary_to_integer_2(ctx, &args));
            }
            let atom_base = [bin(b"10"), ctx.atom("a").expect("seeded")];
            assert_badarg(binary_to_integer_2(ctx, &atom_base));
            // A non-binary is `badarg`.
            let nil = ctx.list(&[]).expect("nil");
            assert_badarg(binary_to_integer_1(ctx, &[nil]));
            // Beyond the 60-bit small range the value is the exact bignum the
            // oracle builds.
            let term = binary_to_integer_1(ctx, &[bin(b"12345678901234567890")]).expect("promotes");
            assert!(access::is_big(term), "expected a bignum");
            let (negative, digits) = beam_codec::term::big_parts(term).expect("a bignum");
            assert!(!negative);
            assert_eq!(
                crate::numeric::mag_to_digits(negative, digits, 10),
                "12345678901234567890"
            );
        });
    }

    // -- float conversions ------------------------------------------------

    #[test]
    fn float_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let cases: &[(f64, &[u8])] = &[
                (1.5, b"1.50000000000000000000e+00"),
                (1.0, b"1.00000000000000000000e+00"),
                (1.0e10, b"1.00000000000000000000e+10"),
                (-0.0, b"-0.00000000000000000000e+00"),
            ];
            for (value, expected) in cases {
                let number = ctx.make_float(*value).expect("fits");
                let term = float_to_binary_1(ctx, &[number]).unwrap();
                assert_eq!(&text(term), expected, "float_to_binary({value})");
            }
            // `/2` renderings, pinned against the oracle.
            for (value, opts, expected) in [
                (7.12, "[{decimals,4}]", &b"7.1200"[..]),
                (7.12, "[{decimals,4},compact]", b"7.12"),
                (7.12, "[{scientific,3}]", b"7.120e+00"),
                (7.12, "[]", b"7.12000000000000010658e+00"),
                (1.5, "[{decimals,2}]", b"1.50"),
                (1.5, "[{scientific,0}]", b"2e+00"),
                (1.5, "[{decimals,25}]", b"1.5000000000000000000000000"),
                (1.0, "[{decimals,2},compact]", b"1.0"),
                (1.0e10, "[{decimals,2}]", b"10000000000.00"),
                (2.5, "[{decimals,0}]", b"3"),
            ] {
                let options = match opts {
                    "[{decimals,4}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(4).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{decimals,4},compact]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(4).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t, ctx.atom("compact").expect("seeded")])
                            .expect("fits")
                    }
                    "[{scientific,3}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("scientific").expect("seeded"),
                                ctx.int(3).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[]" => ctx.list(&[]).expect("nil"),
                    "[{decimals,2}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(2).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{scientific,0}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("scientific").expect("seeded"),
                                ctx.int(0).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{decimals,25}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(25).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    "[{decimals,2},compact]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(2).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t, ctx.atom("compact").expect("seeded")])
                            .expect("fits")
                    }
                    "[{decimals,0}]" => {
                        let t = ctx
                            .tuple(&[
                                ctx.atom("decimals").expect("seeded"),
                                ctx.int(0).expect("fits"),
                            ])
                            .expect("fits");
                        ctx.list(&[t]).expect("fits")
                    }
                    other => panic!("unhandled option literal {other}"),
                };
                let args = [ctx.make_float(value).expect("fits"), options];
                let term = float_to_binary_2(ctx, &args).unwrap();
                assert_eq!(&text(term), expected, "float_to_binary({value}, {opts})");
            }

            // Bad arguments and options.
            let one = ctx.int(1).expect("fits");
            assert_badarg(float_to_binary_1(ctx, &[one]));
            let float = ctx.make_float(1.5).expect("fits");
            let negative = ctx
                .tuple(&[
                    ctx.atom("decimals").expect("seeded"),
                    ctx.int(-1).expect("fits"),
                ])
                .expect("fits");
            let negative = ctx.list(&[negative]).expect("fits");
            assert_badarg(float_to_binary_2(ctx, &[float, negative]));
            let unknown = ctx
                .tuple(&[
                    ctx.atom("a").expect("seeded"),
                    ctx.int(2).expect("fits"),
                ])
                .expect("fits");
            let both = ctx
                .list(&[ctx.atom("a").expect("seeded"), unknown])
                .expect("fits");
            assert_badarg(float_to_binary_2(ctx, &[float, both]));
            assert_badarg(float_to_binary_2(ctx, &[float, ctx.atom("a").expect("seeded")]));
            // `short` is the shortest round-trip rendering, byte-identical to
            // `float_to_list/2` with the same option.
            let short = ctx.list(&[ctx.atom("short").expect("seeded")]).expect("fits");
            for (value, expected) in [
                (1.5f64, &b"1.5"[..]),
                (1.0e10, b"1.0e10"),
                (1.0e-4, b"0.0001"),
                (123456789.0, b"123456789.0"),
                (9_007_199_254_740_992.0, b"9.007199254740992e15"),
                (f64::from_bits(0x3e70_0000_0000_0000), b"5.960464477539063e-8"),
                (-0.0, b"-0.0"),
            ] {
                let number = ctx.make_float(value).expect("fits");
                let term = float_to_binary_2(ctx, &[number, short]).expect("renders");
                assert_eq!(&text(term), expected, "float_to_binary({value}, [short])");
            }        });
    }

    #[test]
    fn binary_to_float_matches_the_oracle() {
        with_ctx(|ctx| {
            for (input, expected) in [
                (&b"1.5"[..], 1.5f64),
                (b"1.5e3", 1500.0),
                (b"1,5", 1.5),
                (b"-1.5", -1.5),
            ] {
                let term = binary_to_float_1(ctx, &[bin(input)]).unwrap();
                assert_eq!(float_of(term), expected, "binary_to_float({input:?})");
            }
            for bad in [
                &b"1"[..],
                b"1.",
                b".5",
                b"",
                b"1.5 ",
                b"1.0e309",
                b" 1.5",
                b"\xff\xff",
            ] {
                assert_badarg(binary_to_float_1(ctx, &[bin(bad)]));
            }
            let nil = ctx.list(&[]).expect("nil");
            assert_badarg(binary_to_float_1(ctx, &[nil]));
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            assert_badarg(binary_to_float_1(ctx, &[odd]));
        });
    }

    // -- atom conversions -------------------------------------------------

    #[test]
    fn atom_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let cases: &[(&str, &str, &[u8])] = &[
                ("a", "utf8", b"a"),
                ("abc", "latin1", b"abc"),
                ("\u{c8}", "latin1", &[200]),
                ("", "utf8", b""),
                ("\u{3c0}", "utf8", &[207, 128]),
                ("\u{3c0}", "unicode", &[207, 128]),
            ];
            for (name, encoding, expected) in cases {
                let atom = ctx.atom(name).expect("interned");
                let enc = ctx.atom(encoding).expect("seeded");
                let term = atom_to_binary_2(ctx, &[atom, enc]).unwrap();
                assert_eq!(text(term), expected.to_vec(), "atom_to_binary({name}, {encoding})");
            }
            // `/1` is UTF-8.
            let term = atom_to_binary_1(ctx, &[ctx.atom("a").expect("seeded")]).unwrap();
            assert_eq!(text(term), b"a".to_vec());

            // `latin1` cannot represent a code point above `U+00FF`.
            let enc = ctx.atom("latin1").expect("seeded");
            let pi = ctx.atom("\u{3c0}").expect("interned");
            assert_badarg(atom_to_binary_2(ctx, &[pi, enc]));
            // A non-atom first argument or an unknown encoding is `badarg`.
            let latin1 = ctx.atom("latin1").expect("seeded");
            assert_badarg(atom_to_binary_1(ctx, &[ctx.int(1).expect("fits")]));
            assert_badarg(atom_to_binary_2(ctx, &[ctx.int(1).expect("fits"), latin1]));
            let a = ctx.atom("a").expect("seeded");
            assert_badarg(atom_to_binary_2(ctx, &[a, ctx.atom("hello").expect("seeded")]));
            assert_badarg(atom_to_binary_2(ctx, &[a, ctx.int(1).expect("fits")]));
        });
    }

    #[test]
    fn binary_to_atom_matches_the_oracle() {
        with_interner(|ctx, interned| {
            // An existing name resolves through the table.
            let term = binary_to_atom_1(ctx, &[bin(b"abc")]).unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("abc"));
            let term = binary_to_atom_2(
                ctx,
                &[bin(b"abc"), ctx.atom("latin1").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("abc"));

            // `latin1` maps each byte to the matching code point; `utf8` requires a
            // valid sequence.
            let term = binary_to_atom_2(
                ctx,
                &[bin(&[200]), ctx.atom("latin1").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("\u{c8}"));
            assert_badarg(binary_to_atom_2(
                ctx,
                &[bin(&[200]), ctx.atom("utf8").expect("seeded")],
            ));
            assert_badarg(binary_to_atom_2(
                ctx,
                &[bin(&[255, 255]), ctx.atom("utf8").expect("seeded")],
            ));

            // The empty binary is the empty atom, and `unicode` aliases `utf8`.
            let term = binary_to_atom_2(
                ctx,
                &[bin(&[]), ctx.atom("unicode").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some(""));
            let term = binary_to_atom_2(
                ctx,
                &[bin(b"abc"), ctx.atom("unicode").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(term)), Some("abc"));

            // A new name is created through the interner, at the same index the
            // cloned table assigned it (the coupling `decode_term_bif` relies on).
            let term = binary_to_atom_1(ctx, &[bin(b"brand_new_atom")]).unwrap();
            assert_eq!(
                interned.borrow().name(atom_val(term)),
                Some("brand_new_atom")
            );
            // A 256-character name is `system_limit`; 255 fit.
            let long = vec![b'a'; 255];
            assert!(binary_to_atom_1(ctx, &[bin(&long)]).is_ok());
            let too_long = vec![b'a'; 256];
            assert!(matches!(
                binary_to_atom_1(ctx, &[bin(&too_long)]),
                Err(beam_common::BeamError::SystemLimit)
            ));
            // `binary_to_existing_atom` never creates: `badarg` for a new name (and
            // for an over-long one, which cannot exist either).
            assert_badarg(binary_to_existing_atom_1(ctx, &[bin(b"brand_new_atom2")]));
            assert_badarg(binary_to_existing_atom_1(ctx, &[bin(&too_long)]));
            let hit = binary_to_existing_atom_1(ctx, &[bin(b"abc")]).unwrap();
            assert_eq!(ctx.atoms().name(atom_val(hit)), Some("abc"));
            let hit = binary_to_existing_atom_2(
                ctx,
                &[bin(b"abc"), ctx.atom("unicode").expect("seeded")],
            )
            .unwrap();
            assert_eq!(ctx.atoms().name(atom_val(hit)), Some("abc"));

            // A non-binary or an unknown encoding is `badarg`.
            let utf8 = ctx.atom("utf8").expect("seeded");
            assert_badarg(binary_to_atom_1(ctx, &[ctx.int(1).expect("fits")]));
            assert_badarg(binary_to_atom_2(ctx, &[bin(b"abc"), ctx.atom("a").expect("seeded")]));
            assert_badarg(binary_to_atom_2(ctx, &[ctx.int(1).expect("fits"), utf8]));
        });
        // Without an interner a *new* atom cannot be created (documented gap).
        with_ctx(|ctx| {
            assert!(matches!(
                binary_to_atom_1(ctx, &[bin(b"no_interner_here")]),
                Err(beam_common::BeamError::TermNotFound)
            ));
        });
    }

    // -- term_to_binary ---------------------------------------------------

    #[test]
    fn term_to_binary_matches_the_oracle() {
        with_ctx(|ctx| {
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let tuple = ctx.tuple(&[ctx.atom("a").expect("seeded"), one]).expect("fits");
            let list = ctx.list(&[one, two, ctx.int(3).expect("fits")]).expect("fits");
            let map = access::make_flatmap(ctx.heap(), &[(one, two)]).expect("fits");

            let cases: &[(Eterm, &[u8])] = &[
                (ctx.int(17).expect("fits"), &[131, 97, 17]),
                (
                    ctx.atom("hello").expect("seeded"),
                    &[131, 119, 5, 104, 101, 108, 108, 111],
                ),
                (
                    ctx.make_float(1.5).expect("fits"),
                    &[131, 70, 63, 248, 0, 0, 0, 0, 0, 0],
                ),
                (tuple, &[131, 104, 2, 119, 1, 97, 97, 1]),
                (list, &[131, 107, 0, 3, 1, 2, 3]),
                (map, &[131, 116, 0, 0, 0, 1, 97, 1, 97, 2]),
                (beam_term::NIL, &[131, 106]),
            ];
            for (term, expected) in cases {
                assert_eq!(&text(term_to_binary_1(ctx, &[*term]).unwrap()), expected);
            }

            // Every accepted `{minor_version, N}` leaves an integer term unchanged; a
            // version outside `0..=2` and an unknown option are `badarg`.
            let seventeen = ctx.int(17).expect("fits");
            for version in [0i64, 1, 2] {
                let options = option_tuple(ctx, "minor_version", ctx.int(version).expect("fits"));
                assert_eq!(
                    text(term_to_binary_2(ctx, &[seventeen, options]).unwrap()),
                    vec![131, 97, 17],
                    "{{minor_version, {version}}}"
                );
            }
            for version in [-1i64, 3] {
                let options = option_tuple(ctx, "minor_version", ctx.int(version).expect("fits"));
                assert_badarg(term_to_binary_2(ctx, &[seventeen, options]));
            }
            // The version must be a *small* integer (`is_small`, `external.c:1502`).
            let one_point_zero = ctx.make_float(1.0).expect("fits");
            let float_version = option_tuple(ctx, "minor_version", one_point_zero);
            assert_badarg(term_to_binary_2(ctx, &[seventeen, float_version]));

            let unknown = ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits");
            assert_badarg(term_to_binary_2(ctx, &[seventeen, unknown]));
            // A non-list (including a bare atom) or an improper list is `badarg`.
            let not_a_list = ctx.atom("a").expect("seeded");
            assert_badarg(term_to_binary_2(ctx, &[seventeen, not_a_list]));
            let improper = ctx
                .cons(ctx.atom("compressed").expect("seeded"), seventeen)
                .expect("fits");
            assert_badarg(term_to_binary_2(ctx, &[seventeen, improper]));

            // `{compressed, Level}` requires `0 =< Level =< 9` (`Level < 10`).
            for level in [-1i64, 10] {
                let options = option_tuple(ctx, "compressed", ctx.int(level).expect("fits"));
                assert_badarg(term_to_binary_2(ctx, &[seventeen, options]));
            }
            let six_point_zero = ctx.make_float(6.0).expect("fits");
            let float_level = option_tuple(ctx, "compressed", six_point_zero);
            assert_badarg(term_to_binary_2(ctx, &[seventeen, float_level]));
        });
    }

    #[test]
    fn term_to_binary_minor_version_selects_the_atom_encoding() {
        with_ctx(|ctx| {
            let one = ctx.int(1).expect("fits");
            // `#{a => 1}` — the map the differential `wire/minor_version_2_map` case pins.
            let key = ctx.atom("a").expect("seeded");
            let map = access::make_flatmap(ctx.heap(), &[(key, one)]).expect("fits");
            let hello = ctx.atom("hello").expect("seeded");

            // The default (minor version 2) and `{minor_version, 2}`: UTF-8 atom tags.
            let utf8_map: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];
            let utf8_hello: &[u8] = &[131, 119, 5, 104, 101, 108, 108, 111];
            assert_eq!(text(term_to_binary_1(ctx, &[map]).unwrap()), utf8_map);
            let mv2 = option_tuple(ctx, "minor_version", ctx.int(2).expect("fits"));
            assert_eq!(
                text(term_to_binary_2(ctx, &[map, mv2]).unwrap()),
                utf8_map
            );
            assert_eq!(
                text(term_to_binary_2(ctx, &[hello, mv2]).unwrap()),
                utf8_hello
            );

            // Minor versions 1 and 0 clear `DFLAG_UTF8_ATOMS`: the oracle writes the
            // Latin-1 `ATOM_EXT` (100) — `term_to_binary(#{a => 1}, [{minor_version, N}])`
            // is `<<131,116,0,0,0,1,100,0,1,97,97,1>>` and
            // `term_to_binary(hello, [{minor_version, N}])` is
            // `<<131,100,0,5,"hello">>`. That is *not* the distribution
            // `AtomEncoding::Latin1` flavour (`SMALL_ATOM_EXT`, 115): `term_to_binary`'s
            // `TERM_TO_BINARY_DFLAGS` carry no `DFLAG_SMALL_ATOM_TAGS`
            // (`external.c:2906-2932`), which is exactly what `MinorVersion::V0`/`V1`
            // encode.
            let latin1_map: &[u8] = &[131, 116, 0, 0, 0, 1, 100, 0, 1, 97, 97, 1];
            let latin1_hello: &[u8] = &[131, 100, 0, 5, 104, 101, 108, 108, 111];
            for version in [0i64, 1] {
                let options = option_tuple(ctx, "minor_version", ctx.int(version).expect("fits"));
                assert_eq!(
                    text(term_to_binary_2(ctx, &[map, options]).unwrap()),
                    latin1_map,
                    "{{minor_version, {version}}} on the map"
                );
                assert_eq!(
                    text(term_to_binary_2(ctx, &[hello, options]).unwrap()),
                    latin1_hello,
                    "{{minor_version, {version}}} on the atom"
                );
            }

            // Minor version 1 keeps `NEW_FLOAT_EXT` and carries no atom, so a bare float
            // is byte-identical to the oracle (`term_to_binary(1.5, [{minor_version,1}])`).
            let mv1 = option_tuple(ctx, "minor_version", ctx.int(1).expect("fits"));
            let float = ctx.make_float(1.5).expect("fits");
            assert_eq!(
                text(term_to_binary_2(ctx, &[float, mv1]).unwrap()),
                vec![131, 70, 63, 248, 0, 0, 0, 0, 0, 0]
            );
        });
    }

    /// The `{minor_version, 0}` legacy float: `FLOAT_EXT` (99) with its fixed 31-byte
    /// text payload — `term_to_binary(1.5, [{minor_version,0}])`.
    #[test]
    fn term_to_binary_minor_version_0_writes_the_legacy_float_ext() {
        with_ctx(|ctx| {
            let mv0 = option_tuple(ctx, "minor_version", ctx.int(0).expect("fits"));
            let float = ctx.make_float(1.5).expect("fits");
            // `external.c:3825-3849`: the tag, the 24-char text and NUL padding to the
            // 31-byte payload.
            let mut expected = vec![131u8, 99];
            expected.extend_from_slice(b"1.50000000000000000000e+00");
            expected.extend_from_slice(&[0u8; 5]);
            assert_eq!(expected.len(), 33, "the oracle's 1.5 image");
            assert_eq!(text(term_to_binary_2(ctx, &[float, mv0]).unwrap()), expected);

            // A float nested anywhere takes the legacy form too.
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[one, float]).expect("fits");
            let mut expected = vec![131u8, 104, 2, 97, 1, 99];
            expected.extend_from_slice(b"1.50000000000000000000e+00");
            expected.extend_from_slice(&[0u8; 5]);
            assert_eq!(
                text(term_to_binary_2(ctx, &[tuple, mv0]).unwrap()),
                expected
            );
            let list = ctx.list(&[float]).expect("fits");
            let mut expected = vec![131u8, 108, 0, 0, 0, 1, 99];
            expected.extend_from_slice(b"1.50000000000000000000e+00");
            expected.extend_from_slice(&[0u8; 5]);
            expected.push(106);
            assert_eq!(text(term_to_binary_2(ctx, &[list, mv0]).unwrap()), expected);
            let map = access::make_flatmap(ctx.heap(), &[(one, float)]).expect("representable");
            let mut expected = vec![131u8, 116, 0, 0, 0, 1, 97, 1, 99];
            expected.extend_from_slice(b"1.50000000000000000000e+00");
            expected.extend_from_slice(&[0u8; 5]);
            assert_eq!(text(term_to_binary_2(ctx, &[map, mv0]).unwrap()), expected);

            // A fun's image carries atoms, so it takes the Latin-1 `ATOM_EXT` form.
            let module = atom_val(ctx.atom("hello").expect("seeded"));
            let function = atom_val(ctx.atom("a").expect("seeded"));
            let fun = beam_codec::term::make_fun_external(ctx.heap(), 1, module, function)
                .expect("fits");
            assert_eq!(
                text(term_to_binary_2(ctx, &[fun, mv0]).unwrap()),
                vec![131, 113, 100, 0, 5, 104, 101, 108, 108, 111, 100, 0, 1, 97, 97, 1]
            );
        });
        // A term with no float at all is accepted at minor version 0 (byte-identical
        // to the oracle — see `term_to_binary_minor_version_selects_the_atom_encoding`).
        with_ctx(|ctx| {
            let mv0 = option_tuple(ctx, "minor_version", ctx.int(0).expect("fits"));
            let list = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            assert_eq!(
                text(term_to_binary_2(ctx, &[list, mv0]).unwrap()),
                vec![131, 107, 0, 3, 1, 2, 3]
            );
        });
    }

    /// A [`LocalFunIdentity`](crate::context::LocalFunIdentity) test double answering
    /// one fixed identity, so the `term_to_binary` local-fun encode path can be
    /// exercised end-to-end.
    struct FixedFunIdentity {
        identity: beam_codec::FunIdentity,
    }

    impl crate::context::LocalFunIdentity for FixedFunIdentity {
        fn local_fun_identity(&mut self, _term: Eterm) -> Result<Option<beam_codec::FunIdentity>> {
            Ok(Some(self.identity))
        }
    }

    /// A local (`make_fun3`) fun term of `arity` with a zeroed 2-word descriptor
    /// (`[uniq][index]`) — the frozen `[thing_word, uniq, index, env…]` layout
    /// `beam_term::fun` writes. Returns the boxed storage (which must outlive the
    /// term) alongside the term; `beam_codec`'s `make_fun_local` is test-gated and
    /// this crate denies `unsafe_code`, so the words are built directly.
    fn local_fun(arity: u32) -> (Box<[Eterm]>, Eterm) {
        let words: Box<[Eterm]> = vec![
            Eterm::from_raw(beam_term::FUN_SUBTAG | ((arity as usize) << 8)),
            Eterm::from_raw(0), // uniq  — the engine-internal descriptor
            Eterm::from_raw(0), // index
        ]
        .into_boxed_slice();
        let term = beam_term::make_boxed(words.as_ptr());
        (words, term)
    }

    #[test]
    fn term_to_binary_of_a_local_fun_is_unsupported_without_an_identity_hook() {
        with_ctx(|ctx| {
            let (_storage, fun) = local_fun(1);
            let err = term_to_binary_1(ctx, &[fun]).expect_err("no identity => unsupported");
            assert!(
                beam_codec::error::is_unsupported_local_fun(&err),
                "a local fun without an identity must report the local-fun reason, got {err:?}"
            );
        });
    }

    #[test]
    fn term_to_binary_of_a_local_fun_uses_the_identity_hook() {
        let mut heap = heap();
        let atoms = table();
        let module = atoms.lookup("hello").expect("seeded");
        let creator = beam_codec::FunPid { node: module, number: 10, serial: 0, creation: 0 };
        let identity = beam_codec::FunIdentity::new(module, [7u8; 16], 3, 9, creator);
        let mut hook = FixedFunIdentity { identity };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_local_fun_identity(&mut hook);
        let (_storage, fun) = local_fun(1);

        let image =
            text(term_to_binary_1(&mut ctx, &[fun]).expect("the identity makes it encodable"));
        assert_eq!(image[0], 131, "VERSION_MAGIC");
        assert_eq!(image[1], 112, "NEW_FUN_EXT");
    }

    #[test]
    fn term_to_binary_compressed_follows_the_oracles_revert_rule() {
        with_ctx(|ctx| {
            let list = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            let plain = vec![131u8, 107, 0, 3, 1, 2, 3];
            // A small body cannot pay for zlib's framing (`real_size - 5` budget), so the
            // image stays uncompressed — the oracle-pinned
            // `term_to_binary([1,2,3], [compressed])`.
            let compressed = compressed_option(ctx);
            assert_eq!(
                text(term_to_binary_2(ctx, &[list, compressed]).unwrap()),
                plain
            );
            for level in [0i64, 1, 6, 9] {
                let options = option_tuple(ctx, "compressed", ctx.int(level).expect("fits"));
                assert_eq!(
                    text(term_to_binary_2(ctx, &[list, options]).unwrap()),
                    plain,
                    "{{compressed, {level}}}"
                );
            }

            // A large, highly repetitive body does compress: `[131][80][len:4 BE][zlib]`.
            let hello = ctx.atom("hello").expect("seeded");
            let mut big = beam_term::NIL;
            for _ in 0..500 {
                big = ctx.cons(hello, big).expect("fits");
            }
            let plain = text(term_to_binary_1(ctx, &[big]).unwrap());
            assert_eq!(plain.len(), 3507, "the oracle's plain image for 500 `hello`s");
            let compressed = compressed_option(ctx);
            let image = text(term_to_binary_2(ctx, &[big, compressed]).unwrap());
            assert_eq!(image[0], 131, "VERSION_MAGIC");
            assert_eq!(image[1], 80, "COMPRESSED_EXT");
            assert_eq!(
                u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize,
                plain.len() - 1,
                "the size field is the body length (without the version byte)"
            );
            assert!(
                image.len() < plain.len(),
                "compression helped: {} vs {}",
                image.len(),
                plain.len()
            );

            // The compressed image round-trips to the very same term.
            let back = binary_to_term_1(ctx, &[bin(&image)]).unwrap();
            assert_eq!(text(term_to_binary_1(ctx, &[back]).unwrap()), plain);

            // `{compressed, 9}` wraps a large body as well.
            let level9 = option_tuple(ctx, "compressed", ctx.int(9).expect("fits"));
            let image9 = text(term_to_binary_2(ctx, &[big, level9]).unwrap());
            assert_eq!(image9[0], 131);
            assert_eq!(image9[1], 80);
            let back = binary_to_term_1(ctx, &[bin(&image9)]).unwrap();
            assert_eq!(text(term_to_binary_1(ctx, &[back]).unwrap()), plain);

            // Level 0 stores without compressing, so the wrapper does not pay off
            // (`body.len() + overhead > body.len()`): the image reverts to plain.
            let store = option_tuple(ctx, "compressed", ctx.int(0).expect("fits"));
            assert_eq!(
                text(term_to_binary_2(ctx, &[big, store]).unwrap()),
                plain
            );
        });
    }

    // -- the `local` / `deterministic` options and the C's rejections ----

    #[test]
    fn term_to_binary_accepts_the_oracles_option_set() {
        with_ctx(|ctx| {
            let list = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                ])
                .expect("fits");
            let plain = vec![131u8, 107, 0, 3, 1, 2, 3];

            // erl -noshell -eval 'io:format("~p~n",[term_to_binary([1,2,3],
            //   [deterministic])]),halt().'  % <<131,107,0,3,1,2,3>>
            // `deterministic` on a map-free term is byte-identical to the default (there
            // is nothing to re-sort), so the option is accepted *and* honoured.
            let det = atom_option(ctx, "deterministic");
            assert_eq!(text(term_to_binary_2(ctx, &[list, det]).unwrap()), plain);

            // ...and repeating one option is fine:
            // erl ... 'term_to_binary([1,2,3],[deterministic,deterministic])' succeeds.
            let det2 = atom_options(ctx, "deterministic", "deterministic");
            assert_eq!(text(term_to_binary_2(ctx, &[list, det2]).unwrap()), plain);

            // `iovec` and `fragment` are the C's `badarg` (`parse_t2b_opts` accepts them
            // only for a caller that can receive an iovec / passed a fragment size,
            // `external.c:1565-1617`) — erl ... '[term_to_binary([1,2,3],[O]) ||
            //   O <- [iovec, fragment]]'  % both raise `badarg` — and so are their
            // tuples (`[{iovec,1}]`/`[{fragment,10}]` ⇒ `badarg`).
            for name in ["iovec", "fragment"] {
                let option = atom_option(ctx, name);
                assert_badarg(term_to_binary_2(ctx, &[list, option]));
                let ten = ctx.int(10).expect("fits");
                let tuple = option_tuple(ctx, name, ten);
                assert_badarg(term_to_binary_2(ctx, &[list, tuple]));
            }

            // `local` + `deterministic` is `badarg` in either order — the two flag sets
            // are mutually exclusive (`external.c:1543-1545`):
            // erl ... 'term_to_binary([1,2,3],[local,deterministic])' % badarg
            for (first, second) in [("local", "deterministic"), ("deterministic", "local")] {
                let both = atom_options(ctx, first, second);
                assert_badarg(term_to_binary_2(ctx, &[list, both]));
            }
        });
    }

    #[test]
    fn term_to_binary_local_is_a_typed_gap() {
        with_ctx(|ctx| {
            let list = ctx
                .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            // The oracle returns `<<131,121,Hash:4/binary,Body/binary>>`: a `LOCAL_EXT`
            // (121) wrapper whose 4-byte payload is `local_node_hash` — a Bob-Jenkins
            // block hash seeded from the node's hostname/pid/time-offset and the VM's
            // monotonic clock (`external.c:136-200`). It is runtime state the runtime
            // supplies through the `LocalNode::local_node_hash` seam; with none installed
            // the option is the typed `unsupported_bif`, never a fabricated hash:
            //   erl ... 'term_to_binary([1,2],[local])'          % <<131,121,_,_,_,_,107,0,2,1,2>>
            //   erl ... 'term_to_binary(self(),[local])'         % 3rd..6th byte differ per run
            // (see `tests/term_to_binary_options.rs`, which pins the two-run difference
            // and the hash-seam image).
            let local = atom_option(ctx, "local");
            let err = term_to_binary_2(ctx, &[list, local]).unwrap_err();
            assert!(
                is_unsupported_bif(&err),
                "expected the typed unsupported_bif gap, got {err:?}"
            );

            // `local` combines with `compressed` in the oracle; with no hash seam the gap
            // is reached before compression matters — the option is refused whatever else
            // is set.
            let both = atom_options(ctx, "local", "compressed");
            let err = term_to_binary_2(ctx, &[list, both]).unwrap_err();
            assert!(is_unsupported_bif(&err), "local + compressed, got {err:?}");

            // `local` is refused for a term holding an internal pid too — with no seam the
            // wrapper (and nothing else) is the gap.
            let pid = ctx.self_pid().to_eterm();
            let local = atom_option(ctx, "local");
            let err = term_to_binary_2(ctx, &[pid, local]).unwrap_err();
            assert!(is_unsupported_bif(&err), "local + pid, got {err:?}");
        });
    }
    /// The oracle's `[deterministic]` image of `#{zz=>1,aa=>2}` — atoms sorted by
    /// **name** (`erl -noshell -eval 'io:format("~p~n",[term_to_binary(#{zz=>1,aa=>2},
    /// [deterministic])]),halt().'`), *not* the plain image, whose flat-map storage
    /// order is atom-index order (`aa` before `zz` here).
    const DETERMINISTIC_MAP_ZZ_AA: &[u8] = &[
        131, 116, 0, 0, 0, 2, 119, 2, 97, 97, 97, 2, 119, 2, 122, 122, 97, 1,
    ];

    /// The oracle's plain (index-ordered) image of the same map — the control that
    /// proves `[deterministic]` really re-ordered it.
    const PLAIN_MAP_ZZ_AA: &[u8] = &[
        131, 116, 0, 0, 0, 2, 119, 2, 122, 122, 97, 1, 119, 2, 97, 97, 97, 2,
    ];

    #[test]
    fn term_to_binary_deterministic_sorts_a_flat_map_by_full_term_order() {
        with_ctx(|ctx| {
            let zz = ctx.atom("zz").expect("seeded");
            let aa = ctx.atom("aa").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");

            // `zz` is seeded before `aa`, so the flat map's `erts_cmp_flatmap_keys`
            // (atom-index) storage order is `[zz, aa]` — the reverse of the
            // `[deterministic]` (`cmp`, atom-**name**) order `[aa, zz]`.
            let map = access::make_map(ctx.heap(), &[(zz, one), (aa, two)]).expect("fits");
            assert!(access::is_flatmap(map), "2 distinct keys ⇒ a flat map");
            assert_eq!(
                text(term_to_binary_1(ctx, &[map]).unwrap()),
                PLAIN_MAP_ZZ_AA.to_vec(),
                "the plain image keeps the index (storage) order"
            );

            let det = atom_option(ctx, "deterministic");
            assert_eq!(
                text(term_to_binary_2(ctx, &[map, det]).unwrap()),
                DETERMINISTIC_MAP_ZZ_AA.to_vec(),
                "[deterministic] re-sorts the flat map into atom-name order"
            );

            // A map nested inside a container is re-sorted too (the flag lives on the
            // encoder, so every map it reaches is affected):
            // erl ... 'term_to_binary({1,#{zz=>1,aa=>2}},[deterministic])'
            let map = access::make_map(ctx.heap(), &[(zz, one), (aa, two)]).expect("fits");
            let nested = ctx.tuple(&[one, map]).expect("fits");
            let det = atom_option(ctx, "deterministic");
            let mut expected = vec![131u8, 104, 2, 97, 1];
            expected.extend_from_slice(&DETERMINISTIC_MAP_ZZ_AA[1..]);
            assert_eq!(text(term_to_binary_2(ctx, &[nested, det]).unwrap()), expected);

            // `[deterministic,deterministic]` is the same flag twice, so it is the same
            // image (`parse_t2b_opts` sets it, it does not toggle).
            let map = access::make_map(ctx.heap(), &[(zz, one), (aa, two)]).expect("fits");
            let det2 = atom_options(ctx, "deterministic", "deterministic");
            assert_eq!(
                text(term_to_binary_2(ctx, &[map, det2]).unwrap()),
                DETERMINISTIC_MAP_ZZ_AA.to_vec()
            );
        });
    }

    /// The oracle's `[deterministic]` image of the 40-pair integer-keyed HAMT
    /// `maps:from_list([{N, N*10} || N <- lists:seq(1,40)])` — every pair in ascending
    /// key order (`erl -noshell -eval 'M = maps:from_list([{N, N*10} || N <-
    /// lists:seq(1,40)]), io:format("~p~n",[term_to_binary(M,[deterministic])]),
    /// halt().'`). The values' small/`INTEGER_EXT` split (`10`..`250` one byte, `260`…
    /// four) is the oracle's own integer tagging.
    const DETERMINISTIC_HAMT_40: &[u8] = &[
        131, 116, 0, 0, 0, 40, //
        97, 1, 97, 10, 97, 2, 97, 20, 97, 3, 97, 30, 97, 4, 97, 40, 97, 5, 97, 50, 97, 6, 97, 60,
        97, 7, 97, 70, 97, 8, 97, 80, 97, 9, 97, 90, 97, 10, 97, 100, 97, 11, 97, 110, 97, 12, 97,
        120, 97, 13, 97, 130, 97, 14, 97, 140, 97, 15, 97, 150, 97, 16, 97, 160, 97, 17, 97, 170,
        97, 18, 97, 180, 97, 19, 97, 190, 97, 20, 97, 200, 97, 21, 97, 210, 97, 22, 97, 220, 97, 23,
        97, 230, 97, 24, 97, 240, 97, 25, 97, 250, //
        97, 26, 98, 0, 0, 1, 4, 97, 27, 98, 0, 0, 1, 14, 97, 28, 98, 0, 0, 1, 24, 97, 29, 98, 0, 0,
        1, 34, 97, 30, 98, 0, 0, 1, 44, 97, 31, 98, 0, 0, 1, 54, 97, 32, 98, 0, 0, 1, 64, 97, 33, 98,
        0, 0, 1, 74, 97, 34, 98, 0, 0, 1, 84, 97, 35, 98, 0, 0, 1, 94, 97, 36, 98, 0, 0, 1, 104, 97,
        37, 98, 0, 0, 1, 114, 97, 38, 98, 0, 0, 1, 124, 97, 39, 98, 0, 0, 1, 134, 97, 40, 98, 0, 0, 1,
        144,
    ];

    #[test]
    fn term_to_binary_deterministic_sorts_a_hash_map_by_full_term_order() {
        with_ctx(|ctx| {
            // 40 distinct keys is past `MAP_SMALL_MAP_LIMIT`, so `write_map` builds a
            // HAMT whose stored order is the hash-slot walk — a different order from the
            // ascending integer key order `[deterministic]` requires.
            let pairs: Vec<(Eterm, Eterm)> = (1i64..=40)
                .map(|n| {
                    (
                        ctx.int(n).expect("fits"),
                        ctx.int(n * 10).expect("fits"),
                    )
                })
                .collect();
            let map = access::make_map(ctx.heap(), &pairs).expect("fits");
            assert!(!access::is_flatmap(map), "40 distinct keys ⇒ a HAMT");

            let plain = text(term_to_binary_1(ctx, &[map]).unwrap());
            let det = atom_option(ctx, "deterministic");
            let sorted = text(term_to_binary_2(ctx, &[map, det]).unwrap());

            assert_eq!(sorted, DETERMINISTIC_HAMT_40.to_vec(), "the sorted HAMT image");
            assert_ne!(
                plain, sorted,
                "the HAMT's hash-slot order differs from the ascending key order"
            );
            assert_eq!(sorted.len(), plain.len(), "the re-sort changes order, not length");
        });
    }
    // -- binary_to_term ---------------------------------------------------

    /// `binary_to_term/2`'s `[used]` count for a top-level `COMPRESSED` image: the
    /// image's own length, so bytes after the zlib stream are ignored *and* uncounted.
    /// A plain image, by contrast, counts its own end (its trailing bytes are uncounted
    /// too — see the tail of the test).
    #[test]
    fn binary_to_term_used_counts_only_a_compressed_image() {
        with_ctx(|ctx| {
            // A compressed image built by hand from a tiny body, so the count is exact
            // whatever `term_to_binary/2`'s revert rule would have done with the body:
            // `[131][80][len:4 BE][zlib]`, then four trailing bytes.
            let body = [107u8, 0, 3, 1, 2, 3]; // LIST_EXT of [1,2,3], no version byte
            let stream = zlib_compress(&body, 6).expect("deflate");
            let mut hand = vec![131u8, 80];
            hand.extend_from_slice(&(body.len() as u32).to_be_bytes());
            hand.extend_from_slice(&stream);
            let image_len = hand.len();
            hand.extend_from_slice(&[9, 8, 7, 6]);

            // The premise of the count: a prefix that stops short of the zlib stream's
            // end does **not** inflate to the declared size (`flate2`'s reader reports an
            // unexpected EOF), while the stream's own end does.
            let declared = body.len();
            assert!(
                beam_codec::compress::decompress_bounded(&stream[..stream.len() - 1], declared)
                    .is_err()
            );
            assert!(beam_codec::compress::decompress_bounded(&stream, declared).is_ok());
            assert!(beam_codec::compress::decompress_bounded(&hand[6..], declared).is_ok());

            // The codec's public `[used]` entry point — the one `decode_term_bif` uses —
            // reports the compressed image's own length for the padded input and for the
            // exact image alike: the count stops at the zlib stream's end.
            let mut h = heap();
            let mut atoms = table();
            let (_, consumed) = beam_codec::decode_term_used(&hand, &mut h, &mut atoms)
                .expect("the padded compressed image decodes");
            assert_eq!(consumed, image_len);
            let (_, consumed) =
                beam_codec::decode_term_used(&hand[..image_len], &mut h, &mut atoms)
                    .expect("the exact compressed image decodes");
            assert_eq!(consumed, image_len);

            // The oracle reports the compressed image's length for both, not the padded
            // input's: erl ... 'binary_to_term(<<C/binary,9,8,7,6>>,[used])' ⇒
            // `{[1,2,3], byte_size(C)}`.
            let blob = bin(&hand);
            let used = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[blob, used]).unwrap();
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                image_len as i64
            );
            assert_eq!(values(access::tuple_element(wrapped, 0)), vec![1, 2, 3]);

            // The plain path is the contrast: its count is the *term's* own end, so the
            // same trailing bytes are not counted — `binary_to_term(<<131,107,0,3,1,2,3,
            // 9,8,7,6>>, [used])` is `{[1,2,3], 7}`, the oracle's rule.
            let plain = [131u8, 107, 0, 3, 1, 2, 3]; // `term_to_binary([1,2,3])`
            let mut padded_plain = plain.to_vec();
            padded_plain.extend_from_slice(&[9, 8, 7, 6]);
            let blob = bin(&padded_plain);
            let used = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[blob, used]).unwrap();
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                plain.len() as i64
            );
            assert_eq!(values(access::tuple_element(wrapped, 0)), vec![1, 2, 3]);
        });
    }

    /// The same count for an image `term_to_binary/2` itself produced (the real path a
    /// caller hits), including the many trailing bytes an appended payload would add.
    #[test]
    fn binary_to_term_used_counts_a_produced_compressed_image() {
        with_ctx(|ctx| {
            let hello = ctx.atom("hello").expect("seeded");
            let mut big = beam_term::NIL;
            for _ in 0..500 {
                big = ctx.cons(hello, big).expect("fits");
            }
            let plain = text(term_to_binary_1(ctx, &[big]).unwrap());
            let compressing = compressed_option(ctx);
            let image = text(term_to_binary_2(ctx, &[big, compressing]).unwrap());
            assert_eq!(image[1], 80, "COMPRESSED_EXT");

            // Without trailing bytes the count is the whole input (the stream ends at its
            // last byte — the single-probe fast path).
            let blob = bin(&image);
            let used = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[blob, used]).unwrap();
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                image.len() as i64
            );

            // With trailing bytes it is still the image's length — and the decoded term
            // is unchanged (trailing bytes are ignored by `/1` and uncounted by `[used]`).
            let mut padded = image.clone();
            padded.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
            let blob = bin(&padded);
            let used = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[blob, used]).unwrap();
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                image.len() as i64
            );
            assert_eq!(
                text(term_to_binary_1(ctx, &[access::tuple_element(wrapped, 0)]).unwrap()),
                plain
            );

            // The plain (non-compressed) path keeps the decoder position — trailing bytes
            // are not counted there either, so the two agree on the rule.
            let mut plain_padded = plain.clone();
            plain_padded.extend_from_slice(&[1, 2, 3]);
            let blob = bin(&plain_padded);
            let used = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[blob, used]).unwrap();
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                plain.len() as i64
            );
        });
    }

    /// The live-oracle cross-check for the `[used]` rule on a top-level `COMPRESSED`
    /// image: the count is the compressed image's **own** length, never the padded
    /// input's.
    ///
    /// The oracle produces the image (`term_to_binary(lists:seq(1,1000), [compressed])`),
    /// reports its own `[used]` for `<Image/binary, 1,2,…,10>`, and prints the image's
    /// exact bytes. The **same** bytes are then decoded here, so the two runtimes are
    /// compared on one image (the oracle's zlib payload is its own — miniz_oxide's
    /// deflate bytes differ — which is why comparing on *its* bytes, not two
    /// independently produced ones, is what makes this a cross-check). Oracle-pinned:
    /// `1827 1837 1827` for `lists:seq(1,1000)` (erts-17.1, probed live) — the
    /// 1827-byte image inside a 1837-byte input reports 1827.
    #[test]
    fn binary_to_term_used_on_a_compressed_image_matches_the_live_oracle() {
        // A raw string keeps the Erlang source readable — every `"` and `\` below is
        // the Erlang character, not a Rust escape.
        let eval = r#"C = term_to_binary(lists:seq(1,1000),[compressed]),
P = <<C/binary,1,2,3,4,5,6,7,8,9,10>>,
{_,U} = binary_to_term(P,[used]),
io:format("~p ~p ~p~n",[byte_size(C), byte_size(P), U]),
Bs = [integer_to_list(B) || B <- binary_to_list(C)],
io:format("~s~n", [lists:join(",", Bs)]),
halt()."#;
        let Some(lines) = oracle_stdout(eval) else {
            eprintln!();
            eprintln!("=========== beam_bifs binconv: live oracle SKIPPED ===========");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or");
            eprintln!("not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
            eprintln!("=============================================================");
            eprintln!();
            return;
        };
        assert!(lines.len() >= 2, "expected two oracle lines, got {lines:?}");
        assert_eq!(
            lines[0], "1827 1837 1827",
            "the oracle's own (image, input, used) sizes changed"
        );
        // The byte list is the oracle's exact compressed image.
        let image: Vec<u8> = lines[1]
            .split(',')
            .map(|byte| byte.trim().parse::<u8>().expect("an oracle byte"))
            .collect();
        assert_eq!(image.len(), 1827, "the oracle printed {len} bytes", len = image.len());
        assert_eq!(&image[..2], &[VERSION_MAGIC, COMPRESSED_EXT]);

        // Decode the oracle's very image, with the oracle's own trailing bytes.
        let mut padded = image.clone();
        padded.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        with_ctx(|ctx| {
            let blob = bin(&padded);
            let used_option = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[blob, used_option]).unwrap();
            // The rule: the image's length, not the ten bytes longer input's.
            assert_eq!(int_of(access::tuple_element(wrapped, 1)), image.len() as i64);
            // And the decoded term really is `lists:seq(1,1000)`.
            assert_eq!(
                values(access::tuple_element(wrapped, 0)),
                (1..=1000).collect::<Vec<i64>>()
            );
        });
    }

    #[test]
    fn binary_to_term_matches_the_oracle() {
        with_ctx(|ctx| {
            // Every supported tag decodes, and the `used` count is the decoder's
            // position (including the `131` version byte).
            let images: &[(&[u8], u64)] = &[
                (&[131, 97, 17], 3),
                (&[131, 119, 5, 104, 101, 108, 108, 111], 8),
                (&[131, 70, 63, 240, 0, 0, 0, 0, 0, 0], 10),
                (&[131, 106], 2),
                (&[131, 116, 0, 0, 0, 0], 6),
                (&[131, 104, 2, 119, 1, 97, 97, 1], 8),
            ];
            for (image, used) in images {
                let term = binary_to_term_1(ctx, &[bin(image)]).unwrap();
                // The `used` form wraps the very same term.
                let options = ctx.list(&[ctx.atom("used").expect("seeded")]).expect("fits");
                let wrapped = binary_to_term_2(ctx, &[bin(image), options]).unwrap();
                assert!(access::is_tuple(wrapped));
                assert_eq!(access::tuple_size(wrapped), 2);
                // The two decodes allocate *separate* terms, so compare by
                // re-encoding: `compare::eq` deep-compares every M1 term class but a
                // map (see the module notes), and one of the images is a map.
                assert_eq!(
                    text(term_to_binary_1(ctx, &[access::tuple_element(wrapped, 0)]).unwrap()),
                    text(term_to_binary_1(ctx, &[term]).unwrap()),
                    "image {image:?}"
                );
                assert_eq!(int_of(access::tuple_element(wrapped, 1)), *used as i64);
            }

            // The decoded values are the expected terms.
            let term = binary_to_term_1(ctx, &[bin(&[131, 97, 17])]).unwrap();
            assert_eq!(int_of(term), 17);
            let term = binary_to_term_1(ctx, &[bin(&[131, 104, 2, 119, 1, 97, 97, 1])]).unwrap();
            assert!(access::is_tuple(term));
            assert_eq!(access::tuple_element(term, 0), ctx.atom("a").expect("seeded"));
            assert_eq!(int_of(access::tuple_element(term, 1)), 1);
            let term = binary_to_term_1(ctx, &[bin(&[131, 107, 0, 3, 1, 2, 3])]).unwrap();
            assert_eq!(values(term), vec![1, 2, 3]);
            let term = binary_to_term_1(ctx, &[bin(&[131, 70, 63, 240, 0, 0, 0, 0, 0, 0])]).unwrap();
            assert_eq!(float_of(term), 1.0);

            // Trailing bytes are ignored by `/1` (but counted by `used`).
            let trailing = bin(&[131, 97, 17, 99]);
            assert_eq!(int_of(binary_to_term_1(ctx, &[trailing]).unwrap()), 17);
            let options = ctx.list(&[ctx.atom("used").expect("seeded")]).expect("fits");
            let wrapped = binary_to_term_2(ctx, &[trailing, options]).unwrap();
            assert_eq!(int_of(access::tuple_element(wrapped, 1)), 3);

            // A malformed/truncated/non-binary image is `badarg`.
            for bad in [
                &[][..],
                &[131][..],
                &[131, 110][..],
                &[0][..],
                &[131, 0][..],
                &[130, 97, 17][..],
            ] {
                assert_badarg(binary_to_term_1(ctx, &[bin(bad)]));
            }
            assert_badarg(binary_to_term_1(ctx, &[ctx.int(1).expect("fits")]));
            let nil = ctx.list(&[]).expect("nil");
            assert_badarg(binary_to_term_1(ctx, &[nil]));
            let odd = bits::make_bitstring(ctx.heap(), &[0x80], 1).expect("fits");
            assert_badarg(binary_to_term_1(ctx, &[odd]));

            // An unknown option (or a non-list) is `badarg`; `safe` accepts a term
            // whose atoms already exist.
            let unknown = ctx.list(&[ctx.atom("a").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(&[131, 97, 17]), unknown]));
            assert_badarg(binary_to_term_2(
                ctx,
                &[bin(&[131, 97, 17]), ctx.atom("used").expect("seeded")],
            ));
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            let term = binary_to_term_2(ctx, &[bin(&[131, 106]), safe]).unwrap();
            assert_eq!(term, beam_term::NIL);
        });
    }

    /// `<<131,119,8,"tjenixen">>` — the oracle's `[safe]` example: a **new** atom.
    const NEW_ATOM_IMAGE: &[u8] = &[131, 119, 8, 116, 106, 101, 110, 105, 120, 101, 110];

    /// `ATOM_EXT` (`100`) for `hello`, an atom every seeded test table holds.
    const KNOWN_ATOM_IMAGE: &[u8] = &[131, 100, 0, 5, 104, 101, 108, 108, 111];

    #[test]
    fn binary_to_term_safe_rejects_new_atoms_and_interns_them_otherwise() {
        // Without `safe` the atom is created through the interner, at the index the
        // cloned table assigned it.
        with_interner(|ctx, interned| {
            let term = binary_to_term_1(ctx, &[bin(NEW_ATOM_IMAGE)]).unwrap();
            assert!(access::is_atom(term));
            assert_eq!(interned.borrow().name(atom_val(term)), Some("tjenixen"));
            // The `[safe]` refusal consults `ctx.atoms()` (the decode scratch is an image
            // of it), which this harness keeps distinct from the interner's clone — so the
            // name the interner just created is still unknown here and the codec refuses.
            // In the runtime the interner mutates that very table, so the atom then exists
            // and `safe` accepts it (the oracle's behaviour).
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM_IMAGE), safe]));
            assert!(interned.borrow().lookup("tjenixen").is_some());
        });
        // With `safe` and an unknown atom the decode is `badarg` — raised **by the codec**
        // before any interning, so nothing reaches either table.
        with_interner(|ctx, interned| {
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM_IMAGE), safe]));
            // Nothing was interned anywhere: the receiving node's table is untouched…
            assert_eq!(ctx.atoms().len(), NAMES.len(), "the shared table grew");
            assert!(ctx.atoms().lookup("tjenixen").is_none());
            // …and neither is the interner's clone.
            assert_eq!(interned.borrow().len(), NAMES.len(), "the interner's table grew");
            assert!(interned.borrow().lookup("tjenixen").is_none());
        });
    }

    #[test]
    fn binary_to_term_safe_accepts_an_atom_already_interned() {
        with_ctx(|ctx| {
            let before = ctx.atoms().len();
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            let term = binary_to_term_2(ctx, &[bin(KNOWN_ATOM_IMAGE), safe])
                .expect("an already-interned atom is accepted under [safe]");
            assert_eq!(term, ctx.atom("hello").expect("seeded"));
            assert_eq!(ctx.atoms().len(), before, "a known atom was interned anew");
        });
    }

    #[test]
    fn binary_to_term_safe_with_used_reports_the_consumed_count() {
        with_ctx(|ctx| {
            // `[safe, used]` on an already-interned atom reports the same count `[used]`
            // alone does — the codec's option-aware `_used` entry point owns the rule.
            let safe_used = atom_options(ctx, "safe", "used");
            let wrapped = binary_to_term_2(ctx, &[bin(KNOWN_ATOM_IMAGE), safe_used]).unwrap();
            assert!(access::is_tuple(wrapped));
            assert_eq!(
                access::tuple_element(wrapped, 0),
                ctx.atom("hello").expect("seeded")
            );
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                KNOWN_ATOM_IMAGE.len() as i64
            );
            let used = used_option(ctx);
            let wrapped = binary_to_term_2(ctx, &[bin(KNOWN_ATOM_IMAGE), used]).unwrap();
            assert_eq!(
                int_of(access::tuple_element(wrapped, 1)),
                KNOWN_ATOM_IMAGE.len() as i64
            );
            // `[safe, used]` on an unknown atom is still `badarg`, not a `{Term,N}` tuple.
            let safe_used = atom_options(ctx, "safe", "used");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM_IMAGE), safe_used]));
            assert_eq!(ctx.atoms().len(), NAMES.len(), "the shared table grew");
        });
    }

    /// The live-oracle cross-check for `[safe]`: the oracle raises `badarg` for a safe
    /// decode of an unknown atom and accepts the same image without `safe`, creating the
    /// atom — the split this crate reproduces.
    #[test]
    fn binary_to_term_safe_matches_the_live_oracle() {
        // A raw string keeps the Erlang source readable — every `"` below is the Erlang
        // character, not a Rust escape. A `try`/`catch` (not the `catch` operator, which
        // the `-eval` route wraps differently) reports the exception class and reason.
        let eval = r#"A = <<131,119,8,116,106,101,110,105,120,101,110>>,
io:format("~p~n", [try binary_to_term(A,[safe]) catch C:R -> {C,R} end]),
io:format("~p~n", [binary_to_term(A)]),
halt()."#;
        let Some(lines) = oracle_stdout(eval) else {
            eprintln!();
            eprintln!("=========== beam_bifs binconv: live oracle SKIPPED ===========");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or");
            eprintln!("not executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
            eprintln!("=============================================================");
            eprintln!();
            return;
        };
        assert_eq!(
            lines.first().map(String::as_str),
            Some("{error,badarg}"),
            "the oracle's `[safe]` verdict changed: {lines:?}"
        );
        assert_eq!(
            lines.get(1).map(String::as_str),
            Some("tjenixen"),
            "the oracle's flag-free decode changed: {lines:?}"
        );
        // The Rust side agrees on both arms.
        with_interner(|ctx, interned| {
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM_IMAGE), safe]));
            assert_eq!(ctx.atoms().len(), NAMES.len(), "the shared table grew");
            assert!(ctx.atoms().lookup("tjenixen").is_none());
            let term = binary_to_term_1(ctx, &[bin(NEW_ATOM_IMAGE)]).unwrap();
            assert!(access::is_atom(term));
            assert_eq!(interned.borrow().name(atom_val(term)), Some("tjenixen"));
        });
    }

    #[test]
    fn binary_to_term_round_trips_pids_and_maps() {
        with_ctx(|ctx| {
            // `PID_SELF` *is* the oracle's own `term_to_binary(self())` image
            // (`erl -noshell -eval 'io:format("~w~n",[binary_to_list(
            // term_to_binary(self()))]),halt(0).'` ⇒ exactly these 29 bytes) for the
            // pid `(number = 10, serial = 0)` on `nonode@nohost`, so this pins
            // `term_to_binary(binary_to_term(Bin)) == Bin` for it.
            //
            // It names this node (`nonode@nohost`, the harness identity), so the decode
            // demotes it to the *internal* pid — the oracle's `dec_is_this_node` branch
            // — and it re-encodes to the very same wire image.
            let term = binary_to_term_1(ctx, &[bin(PID_SELF)]).unwrap();
            assert!(access::is_pid(term));
            assert!(access::is_internal_pid(term), "expected the internal pid");
            assert_eq!(
                ProcessId::from_eterm(term).expect("an internal pid"),
                ProcessId::new(10, 0)
            );
            assert_eq!(text(term_to_binary_1(ctx, &[term]).unwrap()), PID_SELF.to_vec());

            // A map round-trips (structurally).
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let map = access::make_flatmap(ctx.heap(), &[(one, two)]).expect("fits");
            let image = term_to_binary_1(ctx, &[map]).unwrap();
            let back = binary_to_term_1(ctx, &[image]).unwrap();
            assert!(access::is_map(back));
            // A structural `compare::eq` on maps is not available in M1, so the
            // round-trip is pinned byte-exactly instead.
            assert_eq!(text(term_to_binary_1(ctx, &[back]).unwrap()), text(image.clone()));
        });
    }

    #[test]
    fn term_to_binary_and_binary_to_term_round_trip_terms() {
        with_ctx(|ctx| {
            // `binary_to_term(term_to_binary(T))` re-encodes to the very same image
            // for every term class the codec's M1 set covers.
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            // `2^64 + 1`: a bignum, beyond the 60-bit small range.
            let big = beam_codec::term::make_big(ctx.heap(), false, &[1, 1]).expect("fits");
            assert!(access::is_big(big), "expected a bignum");
            let atom = ctx.atom("hello").expect("seeded");
            let tuple = ctx.tuple(&[ctx.atom("a").expect("seeded"), one]).expect("fits");
            let list = ctx.list(&[one, two, ctx.int(3).expect("fits")]).expect("fits");
            let binary = bin(b"hi");
            let float = ctx.make_float(1.5).expect("fits");
            let map = access::make_flatmap(ctx.heap(), &[(one, two)]).expect("fits");

            for term in [one, big, atom, tuple, list, binary, float, map] {
                let image = text(term_to_binary_1(ctx, &[term]).unwrap());
                let back = binary_to_term_1(ctx, &[bin(&image)]).unwrap();
                assert_eq!(
                    text(term_to_binary_1(ctx, &[back]).unwrap()),
                    image,
                    "round trip of {term:?}"
                );
            }

            // The `<<"hi">>` `BINARY_EXT` fixture decodes to that binary, whose
            // re-encoding is the fixture again.
            let decoded = binary_to_term_1(ctx, &[bin(BINARY_HI)]).unwrap();
            assert!(access::is_binary(decoded));
            assert_eq!(text(decoded), b"hi".to_vec());
            assert_eq!(
                text(term_to_binary_1(ctx, &[decoded]).unwrap()),
                BINARY_HI.to_vec()
            );
        });
    }

    /// The oracle's `NEW_PID_EXT` (`88`) image for a pid on `nonode@nohost`: the node
    /// as `SMALL_ATOM_UTF8_EXT` (`119`) then the 4-byte id, serial and creation —
    /// byte-identical to
    /// `erl -noshell -eval 'io:format("~w~n",[term_to_binary(list_to_pid("<0.1.0>"))])'`
    /// for `(1, 0, 0)`.
    fn pid_image(number: u32, serial: u32, creation: u32) -> Vec<u8> {
        let mut image = vec![VERSION_MAGIC, 88, 119, 13];
        image.extend_from_slice(b"nonode@nohost");
        image.extend_from_slice(&number.to_be_bytes());
        image.extend_from_slice(&serial.to_be_bytes());
        image.extend_from_slice(&creation.to_be_bytes());
        image
    }

    /// The oracle's `V4_PORT_EXT` (`120`) image for a port on `nonode@nohost`: the node
    /// then the 8-byte id and 4-byte creation.
    fn port_image(number: u64, creation: u32) -> Vec<u8> {
        let mut image = vec![VERSION_MAGIC, 120, 119, 13];
        image.extend_from_slice(b"nonode@nohost");
        image.extend_from_slice(&number.to_be_bytes());
        image.extend_from_slice(&creation.to_be_bytes());
        image
    }

    #[test]
    fn internal_pids_encode_via_the_local_node() {
        with_ctx(|ctx| {
            // `self()` is an *internal* pid (this harness's `ProcessId::new(1, 0)`);
            // the default `nonode@nohost`/creation-0 fallback lets it encode as the
            // oracle's `NEW_PID_EXT` rather than `unsupported_external_term`.
            let self_pid = ctx.self_pid().to_eterm();
            let image = text(term_to_binary_1(ctx, &[self_pid]).unwrap());
            assert_eq!(image, pid_image(1, 0, 0));

            // A pid nested inside a tuple encodes the same way, inside the tuple's
            // `SMALL_TUPLE_EXT` (`104`) frame.
            let nested = ctx.tuple(&[self_pid]).expect("fits");
            let nested_image = text(term_to_binary_1(ctx, &[nested]).unwrap());
            let mut expected = vec![VERSION_MAGIC, 104, 1];
            expected.extend_from_slice(&pid_image(1, 0, 0)[1..]);
            assert_eq!(nested_image, expected);

            // The `term_to_binary/2` option path threads the local node too (the tiny
            // body makes `compressed` revert to the plain image, `external.c:2250-2301`).
            let options = compressed_option(ctx);
            let optioned = text(term_to_binary_2(ctx, &[self_pid, options]).unwrap());
            assert_eq!(optioned, pid_image(1, 0, 0));

            // The round trip decodes back to the *internal* pid — word-identical to
            // `self()` (the harness's `ProcessId::new(1, 0)`), i.e. the oracle's
            // `P =:= self()` — because the image names this node.
            let back = binary_to_term_1(ctx, &[bin(&image)]).unwrap();
            assert!(access::is_internal_pid(back), "expected the internal pid");
            assert_eq!(back, self_pid, "binary_to_term ∘ term_to_binary must be id");
        });
    }

    // -----------------------------------------------------------------------
    // binary_to_term — the local-node decode demotion
    // -----------------------------------------------------------------------

    /// The local pid **decode** demotion (the oracle's `dec_is_this_node` branch,
    /// `external.c:3175-3182`): an image that names this node materializes the internal
    /// pid immediate, so `binary_to_term(term_to_binary(self())) =:= self()`.
    #[test]
    fn binary_to_term_demotes_a_local_pid_to_the_internal_form() {
        with_ctx(|ctx| {
            let self_pid = ctx.self_pid().to_eterm();
            let image = text(term_to_binary_1(ctx, &[self_pid]).unwrap());
            assert_eq!(
                image,
                pid_image(1, 0, 0),
                "the encoder's internal-pid image"
            );

            let back = binary_to_term_1(ctx, &[bin(&image)]).unwrap();
            assert!(access::is_internal_pid(back), "expected the internal pid");
            assert_eq!(back, self_pid, "P =:= self() after the round trip");

            // A nested identifier demotes too (the demotion is per-arm, not top-level).
            let nested = ctx.tuple(&[self_pid]).expect("fits");
            let nested_image = text(term_to_binary_1(ctx, &[nested]).unwrap());
            let back = binary_to_term_1(ctx, &[bin(&nested_image)]).unwrap();
            assert!(access::is_tuple(back), "expected the tuple back");
            assert_eq!(access::tuple_element(back, 0), self_pid);

            // …and so does the `[used]` arm of `binary_to_term/2` (the `[used]` count
            // still includes the `131` version byte — see `etf_options.rs`).
            let used = used_option(ctx);
            let pair = binary_to_term_2(ctx, &[bin(&image), used]).unwrap();
            assert_eq!(access::tuple_element(pair, 0), self_pid);
            assert_eq!(int_of(access::tuple_element(pair, 1)), image.len() as i64);
        });
    }

    /// The COMPRESSED-body path installs the identity as well, so a local pid inside a
    /// deflated image demotes exactly like its uncompressed twin.
    #[test]
    fn binary_to_term_demotes_a_local_pid_inside_a_compressed_image() {
        with_ctx(|ctx| {
            let self_pid = ctx.self_pid().to_eterm();
            // A body big enough that the deflate stream wins (the C reverts to the plain
            // image when it does not, `external.c:2250-2301`).
            let elements = vec![self_pid; 300];
            let list = ctx.list(&elements).expect("the list fits");
            let options = compressed_option(ctx);
            let image = text(term_to_binary_2(ctx, &[list, options]).unwrap());
            assert_eq!(image[1], COMPRESSED_EXT, "expected a COMPRESSED image");

            let back = binary_to_term_1(ctx, &[bin(&image)]).unwrap();
            let items = proper_list(back).expect("a proper list");
            assert_eq!(items.len(), 300);
            assert!(
                items.iter().all(|&item| item == self_pid),
                "every element must be the internal pid"
            );
        });
    }

    /// An internal reference round-trips under the local node identity: the encoder
    /// writes `NEWER_REFERENCE_EXT` (`90` — the form this runtime emits for an internal
    /// ref), the decoder demotes it back to the frozen 4-word M1 block
    /// (`external.c:4821-4862`), so the two terms compare exactly equal (they are two
    /// distinct allocations, so the raw `Eterm` words differ — `compare::eq` is the
    /// structural test) and re-encode to the same bytes.
    #[test]
    fn binary_to_term_demotes_a_local_reference_to_the_internal_form() {
        with_ctx(|ctx| {
            for id in [0u64, 1, 42, 1 << 18, 1 << 40, (1u64 << 50) | 7] {
                let reference = access::make_ref_thing(ctx.heap(), id).expect("the ref fits");
                let image = text(term_to_binary_1(ctx, &[reference]).unwrap());
                assert_eq!(image[0], VERSION_MAGIC, "id = {id}");
                assert_eq!(image[1], 90, "NEWER_REFERENCE_EXT, id = {id}");

                let back = binary_to_term_1(ctx, &[bin(&image)]).unwrap();
                assert!(access::is_internal_ref(back), "id = {id}");
                assert_eq!(access::ref_thing_id(back).unwrap(), id, "id = {id}");
                assert!(
                    beam_term::compare::eq(back, reference, ctx.atoms()),
                    "binary_to_term ∘ term_to_binary, id = {id}"
                );
                // The round trip is idempotent: the re-encode is the very same image.
                assert_eq!(text(term_to_binary_1(ctx, &[back]).unwrap()), image);
            }
        });
    }
    /// The oracle's own `term_to_binary(make_ref())` image.
    ///
    /// ORACLE (OTP 29.1.1 / erts-17.1):
    /// `erl -noshell -eval 'io:format("~w~n",[binary_to_list(term_to_binary(make_ref()))]),halt(0).'`
    /// ⇒ `[131,90,0,3,119,13,"nonode@nohost",0,0,0,0, 0,0,2,97,219, 29,184,0,2, 67,170,76,26]`
    /// — a `NEWER_REFERENCE_EXT` (`90`) with 3 numbers `0x000261DB`, `0x1DB80002`,
    /// `0x43AA4C1A`. The third is `>= 2^28`, so the packed M1 id
    /// (`(n0 & 0x3FFFF) | (n1 & 0xFFFC0000) | (n2 << 32)`) reaches past the 60-bit small
    /// the internal-ref block stores, so the local-node demotion does not apply: the decode
    /// keeps the faithful **external** reference form (which re-encodes to these very
    /// bytes), never a truncated/substituted id (`beam_codec::ref_wire::representable_ref_id`).
    #[test]
    fn the_oracle_reference_image_decodes_to_the_faithful_external_form() {
        let mut oracle = vec![VERSION_MAGIC, 90, 0, 3, 119, 13];
        oracle.extend_from_slice(b"nonode@nohost");
        oracle.extend_from_slice(&0u32.to_be_bytes()); // creation
        oracle.extend_from_slice(&0x0002_61DBu32.to_be_bytes()); // n0
        oracle.extend_from_slice(&0x1DB8_0002u32.to_be_bytes()); // n1
        oracle.extend_from_slice(&0x43AA_4C1Au32.to_be_bytes()); // n2

        with_ctx(|ctx| {
            let back = binary_to_term_1(ctx, &[bin(&oracle)]).expect("the image decodes");
            assert!(
                access::is_reference(back),
                "expected a reference, got {back:?}"
            );
            assert!(
                access::is_external_ref(back),
                "a packed id past the 60-bit small keeps the external form"
            );
            assert_eq!(
                text(term_to_binary_1(ctx, &[back]).unwrap()),
                oracle,
                "the external reference re-encodes byte-exactly"
            );
        });
    }
    /// A minimal [`LocalNode`](crate::context::LocalNode) test double answering a fixed
    /// `(node, creation)`.
    struct StubNode {
        node: Atom,
        creation: u32,
    }

    impl crate::context::LocalNode for StubNode {
        fn local_node(&mut self) -> Option<(Atom, u32)> {
            Some((self.node, self.creation))
        }
    }

    #[test]
    fn local_node_hook_supplies_the_creation_and_the_port_image() {
        let mut heap = heap();
        let atoms = table();
        let node = atoms.lookup("nonode@nohost").expect("seeded");
        let mut hook = StubNode {
            node,
            creation: 7,
        };
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(5, 3)).with_local_node(&mut hook);

        // The hook's creation (7) is threaded into the pid image, not the fallback's 0;
        // the id and serial come from the pid immediate (`ProcessId::new(5, 3)`).
        let pid = ctx.self_pid().to_eterm();
        let image = text(term_to_binary_1(&mut ctx, &[pid]).unwrap());
        assert_eq!(image, pid_image(5, 3, 7));

        // An internal port encodes as the oracle's `V4_PORT_EXT` under the same node.
        let port = PortId::new(3).to_eterm();
        let image = text(term_to_binary_1(&mut ctx, &[port]).unwrap());
        assert_eq!(image, port_image(3, 7));
    }

    #[test]
    fn internal_pids_stay_unsupported_without_a_local_node() {
        // A table that lacks `nonode@nohost` (and no `LocalNode` hook) leaves the
        // internal id with no wire image — a typed `unsupported_external_term`, never a
        // wrong image.
        let mut heap = heap();
        let mut atoms = AtomTable::new();
        atoms.seed(["a", "b"].iter().copied());
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let self_pid = ctx.self_pid().to_eterm();
        let err = term_to_binary_1(&mut ctx, &[self_pid]).unwrap_err();
        assert!(is_unsupported(&err), "expected unsupported, got {err:?}");

        // A term that merely *contains* an internal pid fails the same way.
        let nested = ctx.tuple(&[self_pid]).expect("fits");
        let err = term_to_binary_1(&mut ctx, &[nested]).unwrap_err();
        assert!(is_unsupported(&err), "expected unsupported, got {err:?}");
    }

    /// The **additive** guarantee of the decode identity: with no resolvable local node
    /// the very same pid image still decodes to the *external* wire pid, byte-for-byte
    /// as before the seam.
    #[test]
    fn a_local_pid_image_stays_external_without_a_local_node() {
        // A table that lacks `nonode@nohost` (and no `LocalNode` hook) leaves
        // `BifContext::local_node` unresolved, so the decoder installs no identity. The
        // interner is needed only because the image's own node atom is *new* to this
        // table (the decoder still interns it, exactly as any other incoming atom).
        let mut no_node = AtomTable::new();
        no_node.seed(["a", "b", "true", "false"].iter().copied());
        with_interner_over(no_node, |ctx, interned| {
            assert!(ctx.local_node().is_none(), "no identity in this context");

            let back = binary_to_term_1(ctx, &[bin(PID_SELF)]).unwrap();
            assert!(access::is_pid(back));
            assert!(
                !access::is_internal_pid(back),
                "expected the external wire pid, got the internal form"
            );
            let (node, number, serial, creation) =
                external_pid_parts(back).expect("an external pid");
            assert_eq!(
                interned.borrow().name(atom_val(node)),
                Some("nonode@nohost")
            );
            assert_eq!((number, serial, creation), (10, 0, 0));
        });
    }
    // -- registration -----------------------------------------------------

    #[test]
    fn register_installs_every_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("list_to_binary", 1),
            ("binary_to_list", 1),
            ("binary_to_list", 3),
            ("byte_size", 1),
            ("bit_size", 1),
            ("binary_part", 2),
            ("binary_part", 3),
            ("integer_to_binary", 1),
            ("integer_to_binary", 2),
            ("binary_to_integer", 1),
            ("binary_to_integer", 2),
            ("float_to_binary", 1),
            ("float_to_binary", 2),
            ("binary_to_float", 1),
            ("atom_to_binary", 1),
            ("atom_to_binary", 2),
            ("binary_to_atom", 1),
            ("binary_to_atom", 2),
            ("binary_to_existing_atom", 1),
            ("binary_to_existing_atom", 2),
            ("term_to_binary", 1),
            ("term_to_binary", 2),
            ("binary_to_term", 1),
            ("binary_to_term", 2),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 24);
        // `float_to_binary/3` is deliberately absent: erts-17.1 has no such BIF.
        assert!(!reg.contains("erlang", "float_to_binary", 3));
    }
}
