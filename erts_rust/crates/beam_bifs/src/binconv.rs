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
//!   option section below — both are accepted, but each is only partly honoured).
//!   `binary_to_term/1` ignores trailing bytes; `binary_to_term/2` takes `safe` and
//!   `used` (`binary_to_term(<<131,97,17>>, [used])` is `{17, 3}` — the count
//!   includes the `131` version byte; for a top-level `COMPRESSED` image it stops at
//!   the zlib stream's end, so bytes after the image are *not* counted — see
//!   [`consumed_compressed_len`]). `safe` refuses a term that would create an atom.
//!
//! # The `term_to_binary/2` options
//!
//! * **`compressed`/`{compressed, Level}`.** The term is encoded as a *body* (no
//!   version byte) and deflated; the compressed wrapper
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
//!   `:3433-3470`/`:3757-3804`). `beam_codec`'s encoder has no deterministic mode (its
//!   map arms emit the stored order), so the option is honoured only when no
//!   re-sorting is needed: a term whose maps are already in that order (and which
//!   holds no fun/native record, whose interior this crate cannot inspect — see
//!   [`deterministic_order_is_unchanged`]) encodes byte-exactly, and anything else
//!   reports the typed `unsupported_bif` — never a wrongly ordered image. Oracle-
//!   pinned: `term_to_binary(#{a=>1,b=>2},[deterministic])` is
//!   `<<131,116,0,0,0,2,119,1,97,97,1,119,1,98,97,2>>` (name order) while the plain
//!   image of the same map is index-ordered. Closing the gap needs
//!   `beam_codec::Encoder::with_deterministic()` (sibling crate — see the code
//!   comment in [`encode_image`]).
//! * **`local`.** Selects `DFLAG_LOCAL_EXT`, which wraps the image in
//!   `[131][LOCAL_EXT:121][4-byte node hash]` (`external.c:3305-3317`) and replaces
//!   an internal pid/port/ref's node atom with `NIL_EXT`. `beam_codec` can write the
//!   wrapper (`Encoder::finish_local(hash)`), but the 4 bytes are
//!   `local_node_hash(...)`: a Bob-Jenkins block hash of the node's
//!   hostname/pid/time-offset and the VM's monotonic clock (`external.c:136-200`),
//!   i.e. `beam_rt`/`beam_dist` runtime state that changes on every VM start and has
//!   no seam into this crate. A typed `unsupported_bif` is the honest answer —
//!   fabricating four bytes would produce an image the oracle (and every other node)
//!   rejects.
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
//! * **`local` is always, and `deterministic` is partly, a typed gap.** `[local]`
//!   reports `unsupported_bif` because the `LOCAL_EXT` payload is the node-local hash
//!   (`beam_rt`/`beam_dist` runtime state with no seam — see the option section
//!   above); `[deterministic]` is byte-exact when the term's maps are already in
//!   atom-name order and reports `unsupported_bif` for any term whose maps would need
//!   re-sorting. Neither ever returns a wrong image.
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

use beam_codec::compress::compress as zlib_compress;
use beam_codec::{
    decode_term, decode_term_with_local_node, Decoder, Encoder, MinorVersion, COMPRESSED_EXT,
    VERSION_MAGIC,
};
use beam_common::Result;
use beam_term::{atom_val, signed_value, Atom, AtomTable, Eterm};

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

/// The bytes of an iolist: an integer in `0..=255`, a binary, or a (possibly
/// improper) list of iolists whose tail is a binary or `[]`.
///
/// The walk is iterative (an explicit work stack) so a deeply nested iolist
/// cannot overflow the Rust stack — C's `erts_iolist_to_buf` is iterative for the
/// same reason.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an element that is
/// none of the three shapes (including a non-byte-aligned bitstring, an integer
/// outside `0..=255` and any improper tail that is not a binary).
fn flatten_iolist(root: Eterm, out: &mut Vec<u8>) -> Result<()> {
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            let byte = u8::try_from(signed_value(term)).map_err(|_| badarg())?;
            out.push(byte);
            continue;
        }
        if bits::is_binary(term) {
            out.extend_from_slice(&bits::bytes(term)?);
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
    let mut out = Vec::new();
    flatten_iolist(args[0], &mut out)?;
    make_bin(ctx, &out)
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
/// Takes `ctx` mutably because the local-node identity is read through
/// [`BifContext::local_node`] (see [`encode_body`]).
///
/// # Errors
///
/// The encoder's and zlib's errors (see [`encode_body`]).
fn encode_image(ctx: &mut BifContext<'_>, term: Eterm, options: &T2bOptions) -> Result<Vec<u8>> {
    // `local` selects the `LOCAL_EXT` wrapper, whose 4-byte payload is the *node's*
    // local hash: `local_node_hash` seeds a Bob-Jenkins block hash from the node's
    // hostname/pid/time-offset and the runtime's monotonic clock
    // (`external.c:136-200`), patches the `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE]`
    // prologue (`:3305-3317`, `:4046-4109`) and — for an internal pid/port/ref —
    // replaces the node atom with `NIL_EXT` (`:5589`). `beam_codec` can write that
    // wrapper (`Encoder::finish_local(hash)`), so the **only** missing piece is the
    // hash value itself: it is runtime/node state that changes on every VM start, so
    // it belongs to `beam_rt`/`beam_dist`, not to this crate, and there is no seam to
    // obtain it. A typed gap is therefore the honest answer — fabricating four bytes
    // would produce an image the oracle (and every other node) rejects.
    if options.local {
        return Err(unsupported_bif());
    }
    // `deterministic` (`DFLAG_DETERMINISTIC`) re-sorts every map's keys into full
    // Erlang term order (`CMP_TERM` = `erts_cmp(_, _, 1, 0)`, `map_key_compare`,
    // `external.c:3217-3227`, applied at `:3433-3470`/`:3757-3804`) — in particular
    // atoms by **name**, where ERTS stores a flat map in `erts_cmp_flatmap_keys`
    // (index) order. The codec's encoder has no deterministic mode: its map arms emit
    // the flat map in stored order and the HAMT in reversed slot order
    // (`encode_map`/`encode_hashmap_pairs`). When the stored order already *is* the
    // deterministic order the plain encoder is byte-exact and the option is a no-op —
    // verified here by [`deterministic_order_is_unchanged`], which walks the term
    // conservatively and answers `false` (⇒ typed gap) for any map that would need
    // re-sorting and for any subtree whose maps it cannot inspect.
    //
    // Closing the remaining gap fully is a `beam_codec` change (reported, not made
    // here): the encoder needs a `with_deterministic()` mode that re-sorts each map's
    // pairs by `beam_term::compare::cmp` before emitting them.
    if options.deterministic && !deterministic_order_is_unchanged(term, ctx.atoms()) {
        return Err(unsupported_bif());
    }
    match options.compression {
        None => plain_image_of(ctx, term, options),
        Some(level) => compressed_image(&encode_body(ctx, term, options)?, level),
    }
}

/// Whether `term`'s map keys are already in the order `[deterministic]` requires, so
/// the plain encoder reproduces the option's image byte-for-byte.
///
/// `[deterministic]` sorts each map's pairs by full Erlang term order (`CMP_TERM`,
/// `erts_cmp(a, b, 1, 0)`) — atoms by **name**, not by index — whereas ERTS stores a
/// flat map in `erts_cmp_flatmap_keys` order (atoms by index) and a HAMT in hash order.
/// A map therefore needs re-sorting whenever its stored pair order differs from the
/// `cmp` order ([`beam_term::map::map_pairs`] returns exactly that order).
///
/// The walk is deliberately **conservative**: it descends tuples, lists (proper and
/// improper), and both map kinds, but a native record or a fun — whose fields/*
/// closure environment can hold a map this module cannot read without `unsafe` — makes
/// it answer `false` (a typed gap) rather than risk `true` for a subtree that does
/// contain a map. A leaf term is trivially unchanged.
fn deterministic_order_is_unchanged(term: Eterm, atoms: &AtomTable) -> bool {
    if access::is_map(term) {
        // The stored order (flat map: `erts_cmp_flatmap_keys`; HAMT: slot walk) vs the
        // deterministic (`cmp`) order. `map_pairs` returns the latter.
        let Ok(stored) = access::map_pairs_storage_order(term) else {
            return false;
        };
        let Ok(ordered) = beam_term::map::map_pairs(term, atoms) else {
            return false;
        };
        if stored.len() != ordered.len() {
            return false;
        }
        let same_order = stored.iter().zip(ordered.iter()).all(|(s, o)| {
            beam_term::compare::cmp(s.0, o.0, atoms).is_eq()
                && beam_term::compare::cmp(s.1, o.1, atoms).is_eq()
        });
        if !same_order {
            return false;
        }
        return stored
            .iter()
            .all(|(k, v)| deterministic_order_is_unchanged(*k, atoms) && deterministic_order_is_unchanged(*v, atoms));
    }
    if access::is_tuple(term) {
        return (0..access::tuple_size(term))
            .all(|index| deterministic_order_is_unchanged(access::tuple_element(term, index), atoms));
    }
    if access::is_list(term) {
        // A cons (`is_list` covers only the non-NIL cons); follow the chain and inspect
        // the tail too, so an improper list's final element is walked as well.
        let mut cursor = term;
        while access::is_list(cursor) {
            if !deterministic_order_is_unchanged(access::car(cursor), atoms) {
                return false;
            }
            cursor = access::cdr(cursor);
        }
        return deterministic_order_is_unchanged(cursor, atoms);
    }
    // A fun's environment and a native record's fields are opaque here — see above.
    !(access::is_function(term) || access::is_record(term))
}

/// The **uncompressed** external image of `term` under `options`: the version byte
/// followed by [`encode_body`]'s bytes.
///
/// This is the image `erlang:term_to_binary/1` produces, and the one
/// `erlang:external_size/1,2` ([`crate::term_conv`]) sizes.
///
/// The oracle's `external_size_2` (`external.c:2221-2246`) validates the whole option
/// list — `compressed`/`{compressed, Level}` included — but passes only the *flags*
/// (the `{minor_version, N}` tag set), never the compression level, to its size probe
/// (`erts_encode_ext_size_2`, `external.c:736-743`, which never deflates). The
/// documented contract is an upper bound — `byte_size(term_to_binary(T, Opts)) =<
/// erlang:external_size(T, Opts)` (`../../erts/preloaded/src/erlang.erl:2687-2710`) —
/// so a compressed image is deliberately **not** what this returns. Oracle-pinned:
/// `erlang:external_size(lists:duplicate(500,hello),[compressed])` is `3507` while
/// `byte_size(term_to_binary(lists:duplicate(500,hello),[compressed]))` is `50`.
///
/// # Errors
///
/// The encoder's errors (see [`encode_body`]).
pub(crate) fn plain_image_of(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: &T2bOptions,
) -> Result<Vec<u8>> {
    Ok(plain_image(&encode_body(ctx, term, options)?))
}

/// Encodes `term` as a **body** — the term with no version byte — under the tag set
/// `options.minor_version` selects.
///
/// Shared by `term_to_binary/1,2` (via [`encode_image`]) and by
/// `erlang:external_size/1,2` (via [`plain_image_of`]), so both BIFs agree on the tag
/// set and on what is encodable.
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
/// The context's local node identity and local-fun identity are read **first** (they
/// borrow `ctx` mutably) and installed on the encoder, so an internal pid/port id
/// encodes as the oracle's `NEW_PID_EXT`/`V4_PORT_EXT` and a local (`make_fun3`) fun
/// as its `NEW_FUN_EXT` image; with no resolvable local node / fun identity the encoder
/// leaves the term unencodable (see [`BifContext::local_node`] and
/// [`BifContext::local_fun_identity`]).
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
    // The local node identity and the local-fun identity both borrow `ctx` mutably,
    // and the encoder holds an immutable borrow of `ctx`'s atom table for its whole
    // lifetime — so read both first and install them afterwards.
    let local = ctx.local_node();
    let identity = ctx.local_fun_identity(term)?;
    // The body carries no version byte; `Encoder::new_body` plus the selected
    // minor version reproduces `term_to_binary`'s tag set exactly.
    let minor_version = match options.minor_version {
        0 => MinorVersion::V0,
        1 => MinorVersion::V1,
        _ => MinorVersion::V2,
    };
    let mut encoder = Encoder::new_body(ctx.atoms()).with_minor_version(minor_version);
    if let Some((node, creation)) = local {
        encoder = encoder.with_local_node(node, creation);
    }
    if let Some(identity) = identity {
        encoder = encoder.with_fun_identity(identity);
    }
    encoder.encode(term)?;
    Ok(encoder.finish())
}
/// The plain image: the version byte followed by the body.
fn plain_image(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 1);
    out.push(VERSION_MAGIC);
    out.extend_from_slice(body);
    out
}

/// The C's compressed-or-revert rule (`external.c:2250-2301`).
///
/// The body is deflated at `level`; the `[131][80][body_len:4 BE][zlib]` image is
/// emitted only when the stream fits the `body_len - 5` budget, otherwise the plain
/// image is returned (which is what happens for every small body — zlib's own
/// framing costs more than the body saves).
///
/// # Errors
///
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) for a body that
/// does not fit the 4-byte size field (unreachable in practice: the term would have
/// to be larger than 4 GiB).
fn compressed_image(body: &[u8], level: u8) -> Result<Vec<u8>> {
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
    Ok(plain_image(body))
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

/// Decodes an external term image (see the module docs for the atom-index
/// coupling this relies on).
///
/// A top-level `COMPRESSED` (`80`) image — the form `term_to_binary/2` with a
/// `compressed` option produces — is unwrapped first, exactly as the oracle does
/// (`external.c:1754-1816`); its `used` count is the image's own length, so bytes
/// after the zlib stream are ignored *and* uncounted, like the plain case's trailing
/// bytes (see [`consumed_compressed_len`]).
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
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-binary, a
/// malformed/truncated image, an unknown option, or `safe` with an image that
/// would create an atom; the codec's `unsupported_external_term` raise for a tag
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
    // on the decoder below so an incoming identifier that names this node demotes to
    // the internal form (see the docs above); `None` leaves the decode unchanged.
    let identity = ctx.local_node();
    // Decode into a *clone* of the context's table: the decoder needs `&mut`, and
    // the clone has the context's indices (the names are interned in order).
    let mut local = ctx.atoms().clone();
    let known = local.len();
    let (term, position) = if is_compressed_image(&data) {
        // `beam_codec::decode_term[_with_local_node]` owns the wrapper handling (the
        // 4-byte size check and the inner body decode) — its `decompress_wrapper` is
        // private, so the wrapper is recognised here with the same two-byte test it
        // uses. The identity-aware entry point installs the identity on the inflated
        // body, so a compressed image demotes exactly like its uncompressed twin.
        let heap = ctx.heap();
        let decoded = match identity {
            Some((node, creation)) => {
                decode_term_with_local_node(&data, heap, &mut local, node, creation, None)?
            }
            None => decode_term(&data, heap, &mut local)?,
        };
        // The wrapper's `used` count stops at the zlib stream's end (the C subtracts
        // what the inflater left unread, `external.c:1797-1801`/`:2011-2014`), so it
        // is the compressed image's length — *not* the whole input when trailing bytes
        // follow.
        let position = consumed_compressed_len(&data).unwrap_or(data.len());
        (decoded, position)
    } else {
        let heap = ctx.heap();
        // The explicit `Decoder` (rather than the free function) keeps `position()` for
        // the `[used]` count *and* — unlike the free function — does not require the
        // whole stream to be consumed, matching the oracle's trailing-byte tolerance.
        let mut decoder = Decoder::new(&data)?;
        if let Some((node, creation)) = identity {
            decoder = decoder.with_local_node(node, creation);
        }
        let decoded = decoder.decode(heap, &mut local)?;
        (decoded, decoder.position())
    };
    if local.len() > known {
        if safe {
            // `safe` forbids creating atoms; nothing has been created yet.
            return Err(badarg());
        }
        let fresh: Vec<String> = (known..local.len())
            .map(|index| {
                local
                    .name(Atom::from_raw(index as u32))
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
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

/// Whether `bytes` is a top-level `COMPRESSED` image:
/// `[131][COMPRESSED_EXT][size:4 BE][zlib stream]`.
///
/// Mirrors `beam_codec::decode`'s private `is_compressed_image` (its 6-byte minimum is
/// the C check at `external.c:1754-1759`); `beam_codec::decode_term` validates the size
/// field and the stream, so this test only selects the entry point.
fn is_compressed_image(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && bytes[0] == VERSION_MAGIC && bytes[1] == COMPRESSED_EXT
}

/// The number of input bytes a top-level `COMPRESSED` image actually occupies — the
/// value the oracle's `binary_to_term/2` `[used]` reports for one.
///
/// `binary2term_prepare` starts the count at the whole input length and, once the zlib
/// stream has ended, subtracts whatever is left unread in the inflater's input buffer
/// (`external.c:1797-1801`, `:2011-2014`) — the count therefore stops at the stream's
/// end, so bytes *after* a compressed image are neither read nor counted (unlike the
/// plain path, whose count is the term's own end). Oracle-pinned: an input of
/// `<<Compressed/binary, 1,2,…,10>>` reports the compressed image's length, not the ten
/// bytes longer input.
///
/// `beam_codec`'s wrapper handling returns only the *term*, so the consumed length is
/// recovered here from the two facts the wrapper guarantees. The 4-byte size field
/// declares the inner body's length, and a **prefix** of the input inflates to exactly
/// that body **iff** it reaches the end of the zlib stream: a truncated prefix fails
/// (`flate2`'s reader reports an unexpected EOF — `zio::read`'s `Err(UnexpectedEof)`
/// against slice input — and the C's `inflate` never reaches `Z_STREAM_END`), while
/// bytes after the stream's end never change the result (inflate stops there and the
/// reader leaves the rest unread). The predicate is therefore monotone in the prefix
/// length, so a binary search for the **shortest** prefix that inflates to the declared
/// size yields the exact consumed length in `O(log n)` inflates. The common case — a
/// stream ending at the input's last byte — costs a single probe.
///
/// Returns `None` when the search cannot converge, i.e. when not even the whole input
/// inflates to the declared size. [`decode_term_bif`] only calls this after
/// `beam_codec::decode_term` accepted the very same image, which already inflates the
/// whole input to the declared size (`decompress_wrapper`), so the fallback is
/// unreachable in practice; it exists so a caller never gets a fabricated count.
fn consumed_compressed_len(data: &[u8]) -> Option<usize> {
    // Every caller checked `is_compressed_image`, so `data[2..6]` (the size field) is
    // present.
    let declared = u32::from_be_bytes([data[2], data[3], data[4], data[5]]) as usize;
    let stream = &data[6..];
    if stream.is_empty() {
        return None;
    }
    let inflates_to_declared = |len: usize| -> bool {
        matches!(
            beam_codec::compress::decompress_bounded(&stream[..len], declared),
            Ok(ref body) if body.len() == declared
        )
    };
    // The whole input must be a valid stream — it is, after a successful decode.
    if !inflates_to_declared(stream.len()) {
        return None;
    }
    // The stream ends at the input's last byte (the common case): one probe rules the
    // binary search out. `inflates_to_declared(0)` cannot hold (an empty input has no
    // zlib header), so the search's lower range below is non-empty.
    if stream.len() == 1 || !inflates_to_declared(stream.len() - 1) {
        return Some(data.len());
    }
    // The shortest prefix that reaches the stream's end: `lo` converges on the first
    // length that inflates to the declared size, `hi` is one that already does.
    let (mut lo, mut hi) = (1usize, stream.len() - 1);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if inflates_to_declared(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Some(6 + lo)
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
            // The oracle returns `<<131,121,Hash:4/binary,Body/binary>>` (LOCAL_EXT,
            // tag 121); the 4-byte payload is `local_node_hash` — runtime/node state this
            // crate has no seam for — so the option is the typed `unsupported_bif`, never
            // a fabricated hash:
            // erl ... 'term_to_binary([1,2],[local])'  % <<131,121,_,_,_,_,107,0,2,1,2>>
            let local = atom_option(ctx, "local");
            let err = term_to_binary_2(ctx, &[list, local]).unwrap_err();
            assert!(
                is_unsupported_bif(&err),
                "expected the typed unsupported_bif gap, got {err:?}"
            );

            // `local` combines with `compressed` in the oracle, but the gap is reached
            // before compression matters — the option is refused whatever else is set.
            let both = atom_options(ctx, "local", "compressed");
            let err = term_to_binary_2(ctx, &[list, both]).unwrap_err();
            assert!(is_unsupported_bif(&err), "local + compressed, got {err:?}");
        });
    }

    /// The oracle's `[deterministic]` image of `#{a=>1,b=>2}` — atoms sorted by **name**
    /// (`erl -noshell -eval 'io:format("~p~n",[term_to_binary(#{a=>1,b=>2},
    /// [deterministic])]),halt().'`), *not* the plain image, which is index-ordered.
    const DETERMINISTIC_MAP_AB: &[u8] = &[
        131, 116, 0, 0, 0, 2, 119, 1, 97, 97, 1, 119, 1, 98, 97, 2,
    ];

    #[test]
    fn term_to_binary_deterministic_is_byte_exact_when_no_resort_is_needed() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");

            // `NAMES` seeds `a` before `b`, so the stored order of this flat map already
            // *is* the deterministic (atom-name) order — the plain encoder emits it and
            // the image is byte-exact. The premise: `map_pairs`' `cmp` order agrees with
            // the storage order.
            assert!(deterministic_order_is_unchanged(
                access::make_flatmap(ctx.heap(), &[(a, one), (b, two)]).expect("fits"),
                ctx.atoms()
            ));

            let map = access::make_flatmap(ctx.heap(), &[(a, one), (b, two)]).expect("fits");
            let det = atom_option(ctx, "deterministic");
            assert_eq!(
                text(term_to_binary_2(ctx, &[map, det]).unwrap()),
                DETERMINISTIC_MAP_AB.to_vec()
            );

            // The same map with no option is the very same image (its stored order is
            // already the name order), and a *nested* map is walked too:
            // erl ... 'term_to_binary({1,#{a=>1,b=>2}},[deterministic])'
            //   % <<131,104,2,97,1,116,0,0,0,2,119,1,97,97,1,119,1,98,97,2>>
            let map = access::make_flatmap(ctx.heap(), &[(a, one), (b, two)]).expect("fits");
            assert_eq!(
                text(term_to_binary_1(ctx, &[map]).unwrap()),
                DETERMINISTIC_MAP_AB.to_vec()
            );
            let nested = ctx.tuple(&[one, map]).expect("fits");
            let det = atom_option(ctx, "deterministic");
            let image = text(term_to_binary_2(ctx, &[nested, det]).unwrap());
            let mut expected = vec![131u8, 104, 2, 97, 1];
            expected.extend_from_slice(&DETERMINISTIC_MAP_AB[1..]);
            assert_eq!(image, expected);
        });
    }

    #[test]
    fn term_to_binary_deterministic_reports_a_typed_gap_when_a_map_must_be_resorted() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");

            // Storage order `[b, a]` vs the deterministic (`cmp`) order `[a, b]`: this map
            // needs the re-sort `beam_codec`'s encoder cannot do, so the option reports
            // the typed gap — never the wrongly ordered (plain) image.
            let reversed = access::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
            assert!(!deterministic_order_is_unchanged(reversed, ctx.atoms()));
            let det = atom_option(ctx, "deterministic");
            let err = term_to_binary_2(ctx, &[reversed, det]).unwrap_err();
            assert!(
                is_unsupported_bif(&err),
                "expected the typed unsupported_bif gap, got {err:?}"
            );

            // A map nested inside a container is found by the walk as well.
            let reversed = access::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
            let nested = ctx.tuple(&[reversed]).expect("fits");
            let det = atom_option(ctx, "deterministic");
            let err = term_to_binary_2(ctx, &[nested, det]).unwrap_err();
            assert!(is_unsupported_bif(&err), "a nested map, got {err:?}");

            // Both reads are still *encodable* without the option (the plain image keeps
            // the stored order), so the gap is the re-sort and nothing else.
            let reversed = access::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
            assert!(term_to_binary_1(ctx, &[reversed]).is_ok());
        });
    }

    // -- binary_to_term ---------------------------------------------------

    /// `binary_to_term/2`'s `[used]` count for a top-level `COMPRESSED` image: the
    /// image's own length, so bytes after the zlib stream are ignored *and* uncounted.
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

            // The premise of the search: a prefix that stops short of the zlib stream's
            // end does **not** inflate to the declared size (`flate2`'s reader reports an
            // unexpected EOF), while the stream's own end does.
            let declared = body.len();
            assert!(
                beam_codec::compress::decompress_bounded(&stream[..stream.len() - 1], declared)
                    .is_err()
            );
            assert!(beam_codec::compress::decompress_bounded(&stream, declared).is_ok());
            assert!(beam_codec::compress::decompress_bounded(&hand[6..], declared).is_ok());

            assert_eq!(consumed_compressed_len(&hand), Some(image_len));
            assert_eq!(consumed_compressed_len(&hand[..image_len]), Some(image_len));

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

    #[test]
    fn binary_to_term_safe_rejects_new_atoms_and_interns_them_otherwise() {
        // `<<131,119,8,"tjenixen">>` — the oracle's `[safe]` example: a *new* atom.
        const NEW_ATOM: &[u8] = &[131, 119, 8, 116, 106, 101, 110, 105, 120, 101, 110];
        // Without `safe` the atom is created through the interner, at the index the
        // cloned table assigned it.
        with_interner(|ctx, interned| {
            let term = binary_to_term_1(ctx, &[bin(NEW_ATOM)]).unwrap();
            assert!(access::is_atom(term));
            assert_eq!(interned.borrow().name(atom_val(term)), Some("tjenixen"));
            // The `safe` form consults the runtime table (`ctx.atoms()`), which this
            // harness keeps distinct from the interner's clone — so the name the
            // interner just created is still unknown here and the decode is refused.
            // In the runtime the interner mutates that very table, so the atom then
            // exists and `safe` accepts it (the oracle's behaviour).
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM), safe]));
            assert!(interned.borrow().lookup("tjenixen").is_some());
        });
        // With `safe` and an unknown atom the decode is `badarg` — and *creates*
        // nothing.
        with_interner(|ctx, interned| {
            let safe = ctx.list(&[ctx.atom("safe").expect("seeded")]).expect("fits");
            assert_badarg(binary_to_term_2(ctx, &[bin(NEW_ATOM), safe]));
            assert!(interned.borrow().lookup("tjenixen").is_none());
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
    fn the_oracle_reference_image_is_a_typed_id_width_gap() {
        let mut oracle = vec![VERSION_MAGIC, 90, 0, 3, 119, 13];
        oracle.extend_from_slice(b"nonode@nohost");
        oracle.extend_from_slice(&0u32.to_be_bytes()); // creation
        oracle.extend_from_slice(&0x0002_61DBu32.to_be_bytes()); // n0
        oracle.extend_from_slice(&0x1DB8_0002u32.to_be_bytes()); // n1
        oracle.extend_from_slice(&0x43AA_4C1Au32.to_be_bytes()); // n2

        with_ctx(|ctx| {
            let err = binary_to_term_1(ctx, &[bin(&oracle)]).unwrap_err();
            assert!(
                matches!(err, beam_common::BeamError::SystemLimit),
                "expected the typed id-width gap, got {err:?}"
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
