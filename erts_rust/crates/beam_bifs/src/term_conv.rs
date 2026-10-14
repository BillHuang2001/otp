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

//! Term-conversion and inspection BIFs for `erlang` and `binary`:
//! `bitstring_to_list/1`, `delete_element/2`, `insert_element/3`,
//! `iolist_to_iovec/1`, `is_integer/3`, `display/1`, `display_string/2`,
//! `external_size/1,2`, `binary:referenced_byte_size/1`, and the textual
//! pid/ref/port conversions
//! `pid_to_list/1`, `list_to_pid/1`, `ref_to_list/1`, `list_to_ref/1`,
//! `pid_to_binary/1`, `ref_to_binary/1` and `port_to_binary/1`.
//!
//! Ground truth (read-only): the entries in `../../erts/emulator/beam/bif.tab`
//! (`bif erlang:bitstring_to_list/1` :493, `ubif erlang:is_integer/3` :845,
//! `bif erlang:insert_element/3` :612, `bif erlang:delete_element/2` :613,
//! `bif erlang:iolist_to_iovec/1` :686, `bif erlang:display/1` :60,
//! `bif erlang:display_string/2` :61, `bif erlang:external_size/1` :68,
//! `bif erlang:external_size/2` :69, `bif binary:referenced_byte_size/1` :561,
//! `bif erlang:list_to_pid/1` :92, `bif erlang:list_to_ref/1` :94,
//! `bif erlang:pid_to_list/1` :126, `bif erlang:ref_to_list/1` :273,
//! `bif erlang:pid_to_binary/1` :866, `bif erlang:port_to_binary/1` :867,
//! `bif erlang:ref_to_binary/1` :868) and their implementations — `binary.c`'s
//! `bitstring_to_list_1`,
//! `bif.c`'s `insert_element_3`/`delete_element_2`/`display_1`/`display_string_2`
//! and the textual conversions (`pid_to_list_1`:4512, `list_to_pid_1`:4532,
//! `ref_to_list_1`:4466, `list_to_ref_1`:4712, `pid_to_binary_1`:4879,
//! `port_to_binary_1`:4886, `ref_to_binary_1`:4893, `term2list_dsprintf`:4450,
//! `term2binary_dsprintf`:4863), the `%T` renderer in
//! `erl_printf_term.c` (`REF_DEF`:493, `PID_DEF`:508, `PORT_DEF`:521) and the
//! string helper `intlist_to_buf` in `utils.c:2828`;
//! `erl_bif_guard.c`'s `is_integer_3`, `erl_iolist.c`'s `iolist_to_iovec_1` and
//! `erl_bif_binary.c`'s `binary_referenced_byte_size_1`.
//!
//! # Roster
//!
//! Every BIF above is registered; each one is pinned against the OTP 29 /
//! erts-17.1 oracle (the probe is quoted at the implementation and in
//! `tests/term_conv.rs`). `pid_to_binary/1`, `ref_to_binary/1` and
//! `port_to_binary/1` are OTP-30 additions the 29.1.1 oracle does **not** export
//! (`erlang:function_exported/3` is `false` for all three); their bytes are pinned
//! to the C instead, which renders them through the same `"%T"` format as the
//! `*_to_list/1` forms (`term2binary_dsprintf` / `term2list_dsprintf`).
//!
//! Two behaviours the oracle has are **not** reproduced, because this runtime has
//! no seam for them; both are a typed `error:unsupported_bif`, never a wrong
//! result (see [`display_string_2`]):
//!
//! * `display_string/2`'s `stderr`/`stdin` streams — the only output seam is the
//!   string-valued [`OutputSink`](crate::context::OutputSink) `io:format` uses, and
//!   the oracle writes those two streams to their own file descriptors;
//! * `display_string/2` of a binary payload that is not valid UTF-8 — the sink
//!   takes a `&str`, so the raw bytes cannot be handed through it.
//!
//! # The textual conversions and the Milestone-1 identity substitution
//!
//! The `*_to_list/1` forms return the `%T` text of a pid/ref/port as a charlist
//! (`buf_to_intlist`, plus the C's `\n`-free `%T`), the `*_to_binary/1` forms the
//! same bytes as a binary. The node (channel) part is `0` — this node's *node
//! number*, which is `0` for the `nonode@nohost` node Milestone-1 always is, the
//! same substitution `src/port.rs` documents for `#Port<0.Num>` and
//! `src/print.rs`'s `format_term` for `<0.Num.Ser>`/`#Ref<0.0.0.N>`.
//!
//! An **external** pid/port/ref is refused (`badarg`) — deliberately, not because
//! the argument is malformed: its channel number is the index the node-name atom
//! happens to hold in the *running* VM's atom table, which the frozen external term
//! does not carry and no function of the term can recover (the verbatim oracle
//! record, the "local node prints `0`" branch and the seam a faithful rendering
//! would need are all on [`external_channel_gap`]). `list_to_ref/1` accepts only
//! the exact `#Ref<0.0.0.N>` shape this runtime's own ref thing can hold (see
//! [`parse_ref_text`]).
//!
//! # `erlang:external_size/1,2`
//!
//! `external_size(T)` / `external_size(T, Options)` size the external term image
//! without ever building it. Both delegate to [`crate::binconv`]'s **uncompressed**
//! image (`plain_image_of`) under the option list `term_to_binary/2` accepts, so the
//! two BIFs share one option parser and one body encoder and cannot drift.
//!
//! The oracle's `external_size_2` (`external.c:2221-2246`) validates
//! `compressed`/`{compressed, Level}` but passes only the `{minor_version, N}` *flags*
//! to its size probe (`erts_encode_ext_size_2`, `external.c:736-743`), which never
//! deflates — the documented contract is the upper bound `byte_size(term_to_binary(T,
//! Opts)) =< external_size(T, Opts)`
//! (`../../erts/preloaded/src/erlang.erl:2687-2710`). So `compressed` selects the
//! *uncompressed* size: oracle-pinned,
//! `erlang:external_size(lists:duplicate(500,hello),[compressed])` is `3507` (the
//! plain image) while the compressed image is `50` bytes. A term the codec cannot
//! encode — an internal pid with no resolvable local node, a local fun with no
//! identity — stays the codec's typed `unsupported_external_term`, never a number.

use beam_common::{BeamError, Result};
use beam_codec::term as codec_term;
use beam_process::{PortId, ProcessId, MAX_PROCESS_NUMBER, MAX_PROCESS_SERIAL};
use beam_term::prelude::signed_value;
use beam_term::{atom_val, Eterm, ERL_ONHEAP_BITS_LIMIT, NIL};

use crate::access;
use crate::arith;
use crate::binconv::{parse_term_to_binary_options, plain_image_of, T2bOptions};
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, system_limit, unsupported_bif};
use crate::print::{flatten, proper_list, term_chunk, UNLIMITED_DEPTH};
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// bitstring_to_list/1
// ---------------------------------------------------------------------------

/// `bitstring_to_list(Bitstring)` (`binary.c:413`) — the bitstring's whole bytes
/// as small integers, plus its trailing `bit_size rem 8` bits as one final
/// bitstring when the size is not a whole number of bytes.
///
/// Pinned to the oracle:
/// `bitstring_to_list(<<1,2,3>>)` ⇒ `[1,2,3]`,
/// `bitstring_to_list(<<1,2,3:4>>)` ⇒ `[1,2,<<3:4>>]`,
/// `bitstring_to_list(<<1:9>>)` ⇒ `[0,<<1:1>>]`, `bitstring_to_list(<<>>)` ⇒ `[]`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is not a
/// bitstring (`bitstring_to_list([1,2])`, `bitstring_to_list(5)`).
fn bitstring_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if !bits::is_bitstring(term) {
        return Err(badarg());
    }
    let bit_len = bits::bit_size(term)?;
    if bit_len == 0 {
        // `binary.c:429`: a zero-sized bitstring is the empty list.
        return Ok(NIL);
    }
    // `BYTE_SIZE(size)` whole bytes, then `TAIL_BITS(size)` bits of a sub-bitstring.
    let whole = bit_len / 8;
    let tail_bits = bit_len % 8;
    let packed = access::bitstring_bits(term)?;
    let mut elems: Vec<Eterm> = Vec::with_capacity(whole + 1);
    for byte in &packed[..whole] {
        elems.push(ctx.int(i64::from(*byte))?);
    }
    if tail_bits != 0 {
        // `packed` holds `ceil(bit_len / 8)` bytes, so `packed[whole..]` is exactly
        // the trailing partial byte.
        elems.push(bits::make_bitstring(ctx.heap(), &packed[whole..], tail_bits)?);
    }
    ctx.list(&elems)
}

// ---------------------------------------------------------------------------
// delete_element/2 and insert_element/3
// ---------------------------------------------------------------------------

/// The 0-based slot of the `1`-based element index `index` in a tuple of arity
/// `arity`, where the index may address one **past** the last element
/// (`for_insert`: the C's `arity + 1` bound).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) unless `index` is a small
/// integer in `1..=arity (+ 1)`.
fn element_slot(index: Eterm, arity: usize, for_insert: bool) -> Result<usize> {
    if !access::is_small(index) {
        return Err(badarg());
    }
    let ix = signed_value(index);
    let max = arity as i64 + i64::from(for_insert);
    if ix < 1 || ix > max {
        return Err(badarg());
    }
    Ok(ix as usize - 1)
}

/// The arity of `delete_element/2`'s and `insert_element/3`'s tuple argument, or
/// `badarg` (`bif.c:3118`, `bif.c:3156`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a tuple.
fn element_tuple(term: Eterm) -> Result<usize> {
    if !access::is_tuple(term) {
        return Err(badarg());
    }
    Ok(access::tuple_size(term))
}

/// `delete_element(Index, Tuple)` (`bif.c:3148`) — a new tuple without element
/// `Index`.
///
/// Pinned to the oracle: `delete_element(1, {a,b,c})` ⇒ `{b,c}`,
/// `delete_element(3, {a,b,c})` ⇒ `{a,b}`, `delete_element(1, {a})` ⇒ `{}`. An
/// out-of-range index (`0` or `4` on `{a,b,c}`, any index on `{}`), a non-tuple and
/// a non-small-integer index are `badarg`.
fn delete_element_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (index, tuple) = (args[0], args[1]);
    let arity = element_tuple(tuple)?;
    let slot = element_slot(index, arity, false)?;
    let elems: Vec<Eterm> = (0..arity)
        .filter(|&i| i != slot)
        .map(|i| access::tuple_element(tuple, i))
        .collect();
    ctx.tuple(&elems)
}

/// `insert_element(Index, Tuple, Value)` (`bif.c:3110`) — a new tuple of arity
/// `arity + 1` with `Value` at element `Index` (so `Index` may be `arity + 1`).
///
/// Pinned to the oracle: `insert_element(1, {a,b}, x)` ⇒ `{x,a,b}`,
/// `insert_element(3, {a,b}, x)` ⇒ `{a,b,x}`, `insert_element(1, {}, x)` ⇒ `{x}`.
/// An out-of-range index (`0` or `4` on `{a,b}`), a non-tuple and a
/// non-small-integer index are `badarg`; `Value` is any term (never type-checked).
fn insert_element_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (index, tuple, value) = (args[0], args[1], args[2]);
    let arity = element_tuple(tuple)?;
    let slot = element_slot(index, arity, true)?;
    let mut elems: Vec<Eterm> = Vec::with_capacity(arity + 1);
    for i in 0..arity {
        if i == slot {
            elems.push(value);
        }
        elems.push(access::tuple_element(tuple, i));
    }
    if slot == arity {
        elems.push(value);
    }
    ctx.tuple(&elems)
}

// ---------------------------------------------------------------------------
// binary:referenced_byte_size/1
// ---------------------------------------------------------------------------

/// `binary:referenced_byte_size(Binary)` (`erl_bif_binary.c:2691`) — the size of
/// the whole binary the argument references.
///
/// Every binary this runtime builds is on-heap and whole (`binary:part/3` copies;
/// there is no off-heap `BinRef`/`SubBits` producer), so the size is the
/// argument's own **byte size** — the C's `br == NULL` branch, `BYTE_SIZE(size)`,
/// which truncates a partial trailing byte exactly like `size/1`. Verified against
/// the oracle for a runtime-built on-heap binary
/// (`binary:referenced_byte_size(list_to_binary([1,2,3]))` ⇒ `3`) and for a large
/// one off the 64-byte on-heap limit
/// (`binary:referenced_byte_size(binary:copy(<<1>>, 100000))` ⇒ `100000`).
///
/// Two documented divergences, both properties of terms this runtime cannot
/// produce rather than wrong answers for one it can:
///
/// * a **sub-binary** — the oracle reports the size of the whole binary it points
///   into (`binary:referenced_byte_size(binary:part(binary:copy(<<1>>,100),10,5))`
///   ⇒ `100`), which M1 does not model (`binary:part/3` copies, so that argument is
///   an ordinary 5-byte binary here);
/// * a **literal-area** binary — the `<<1,2,3>>` of a compiled module reports the
///   literal chunk's allocated size in the oracle (`256`); this runtime materializes
///   no such literal.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the argument is not a
/// bitstring (`binary:referenced_byte_size([1,2])`).
fn referenced_byte_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if !bits::is_bitstring(term) {
        return Err(badarg());
    }
    ctx.int((bits::bit_size(term)? / 8) as i64)
}

// ---------------------------------------------------------------------------
// iolist_to_iovec/1
// ---------------------------------------------------------------------------

/// `erl_iolist.c:37`: `IOL2V_COPY_THRESHOLD (ERL_ONHEAP_BITS_LIMIT * 8)` — the
/// largest component (in bits) that is copied into the common output buffer.
const IOL2V_COPY_THRESHOLD_BITS: usize = ERL_ONHEAP_BITS_LIMIT * 8;

/// Turns the pending copied byte run into one binary element
/// (`generic_iolist_copy_vec_enqueue`), leaving nothing pending.
///
/// # Errors
///
/// Propagates a heap-allocation failure.
fn flush_pending(
    ctx: &mut BifContext<'_>,
    elems: &mut Vec<Eterm>,
    pending: &mut Vec<u8>,
) -> Result<()> {
    if !pending.is_empty() {
        elems.push(bits::make_binary(ctx.heap(), pending)?);
        pending.clear();
    }
    Ok(())
}

/// `iolist_to_iovec(IoList)` (`erl_iolist.c:1265`) — the iolist as a list of
/// binaries, merging every run of copied components into one binary and passing a
/// large byte-aligned component through **as-is**.
///
/// The result shape is pinned to the oracle:
///
/// * `iolist_to_iovec([1,2,<<"ab">>,[3,[<<"c">>]],<<"de">>])` ⇒
///   `[<<1,2,97,98,3,99,100,101>>]` (every copied component merges into one);
/// * `iolist_to_iovec([])`, `iolist_to_iovec(<<>>)`, `iolist_to_iovec([<<>>])` and
///   `iolist_to_iovec([<<>>,<<>>])` ⇒ `[]` (an empty result is the empty list, not
///   `[<<>>]`);
/// * `iolist_to_iovec(<<"abc">>)` ⇒ `[<<"abc">>]` and `iolist_to_iovec([1|<<2>>])`
///   ⇒ `[<<1,2>>]` (a bare byte-aligned binary and a bitstring **cons tail** are
///   both accepted);
/// * a component larger than [`IOL2V_COPY_THRESHOLD_BITS`] that is byte aligned is
///   *borrowed*, so it keeps its own slot:
///   `iolist_to_iovec([binary:copy(<<7>>,513), binary:copy(<<7>>,513)])` ⇒ two
///   513-byte binaries while the 512-byte form merges into one 1024-byte binary
///   (the C's `IOL_SHOULD_COPY`), and `iolist_to_iovec([B513,1,2])` ⇒
///   `[B513,<<1,2>>]`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an element that is
/// neither a byte (`0..=255`), a byte-aligned bitstring, nor a nested list — and
/// for a non-list, non-nil, non-bitstring argument (`iolist_to_iovec(5)`). Note
/// that *every* bitstring leaf must be byte aligned
/// (`iolist_to_iovec([<<1:4>>,<<2:4>>])` ⇒ `badarg`, even though the two halves
/// would total a whole byte), unlike `iolist_to_binary/1`'s final-partial-byte
/// rule.
fn iolist_to_iovec_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let root = args[0];
    // The C's fast path: a bare bitstring (`erl_iolist.c:1272`).
    if bits::is_bitstring(root) {
        let size_bits = bits::bit_size(root)?;
        if size_bits % 8 != 0 {
            return Err(badarg());
        }
        return if size_bits == 0 { Ok(NIL) } else { ctx.list(&[root]) };
    }
    if !access::is_nil(root) && !access::is_list(root) {
        return Err(badarg());
    }

    let mut elems: Vec<Eterm> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    // An explicit work stack, so a deeply nested iolist cannot overflow the Rust
    // stack; the loop is bounded by the finite input term.
    let mut stack = vec![root];
    while let Some(term) = stack.pop() {
        if access::is_nil(term) {
            continue;
        }
        if access::is_small(term) {
            let byte = signed_value(term);
            if !(0..=255).contains(&byte) {
                return Err(badarg());
            }
            pending.push(byte as u8);
            continue;
        }
        if bits::is_bitstring(term) {
            let size_bits = bits::bit_size(term)?;
            if size_bits % 8 != 0 {
                return Err(badarg());
            }
            // `IOL_SHOULD_COPY(Threshold, Unit, Offset, Size)` is false — i.e. the
            // component is enqueued *unmerged* — exactly when it is byte aligned and
            // larger than the threshold (`erl_iolist.c:39`).
            let (_, offset, _) = codec_term::bitstring_bytes(term)?;
            if size_bits > IOL2V_COPY_THRESHOLD_BITS && offset % 8 == 0 {
                flush_pending(ctx, &mut elems, &mut pending)?;
                elems.push(term);
            } else {
                // The copied run is byte aligned, so its MSB-first packing is its
                // byte image (the C's `generic_iolist_copy_bitstring` run).
                let packed = access::bitstring_bits(term)?;
                pending.extend_from_slice(&packed[..size_bits / 8]);
            }
            continue;
        }
        if access::is_list(term) {
            let tail = access::cdr(term);
            if !access::is_nil(tail) && !access::is_list(tail) && !bits::is_bitstring(tail) {
                return Err(badarg());
            }
            // Push the tail first so the head is processed next (LIFO).
            stack.push(tail);
            stack.push(access::car(term));
            continue;
        }
        return Err(badarg());
    }
    flush_pending(ctx, &mut elems, &mut pending)?;
    ctx.list(&elems)
}

// ---------------------------------------------------------------------------
// is_integer/3
// ---------------------------------------------------------------------------

/// `is_integer(Term, Lower, Upper)` (`erl_bif_guard.c:337`) — the OTP-29 guard:
/// true iff `Term` is an integer in `Lower..=Upper`.
///
/// Pinned to the oracle: both bounds are validated **first** and must be integers,
/// so `is_integer(5,1.0,10)`, `is_integer(5,1,10.0)` and `is_integer(5,1,inf)` are
/// `badarg`; a non-integer `Term` is `false`, never an error
/// (`is_integer(a,1,10)` ⇒ `false`); the range need not be ordered
/// (`is_integer(5,10,1)` ⇒ `false`). The comparison is the C's arithmetic
/// `CMP_LE`, so either bound may be a bignum.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `Lower` or `Upper` is
/// not an integer.
fn is_integer_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (term, lower, upper) = (args[0], args[1], args[2]);
    if !access::is_integer(lower) || !access::is_integer(upper) {
        return Err(badarg());
    }
    if !access::is_integer(term) {
        return ctx.boolean(false);
    }
    // Every operand is an integer, so the arithmetic order is the numeric one.
    let atoms = ctx.atoms();
    let not_greater = core::cmp::Ordering::Greater;
    let at_least_lower = beam_term::compare::cmp_arith(lower, term, atoms) != not_greater;
    let at_most_upper = beam_term::compare::cmp_arith(term, upper, atoms) != not_greater;
    ctx.boolean(at_least_lower && at_most_upper)
}

// ---------------------------------------------------------------------------
// display/1 and display_string/2
// ---------------------------------------------------------------------------

/// `display(Term)` (`bif.c:4148`) — the term as `~p` (the printable string rule,
/// no depth limit, **no line wrapping**), followed by a line terminator, written
/// through the [`OutputSink`](crate::context::OutputSink) `io:format/1,2` uses;
/// returns `true`.
///
/// The terminator is `\r\n`, not `\n`: the C is `erts_printf("%.*T\n", …)` and
/// `erts_printf` writes through `write_f_add_cr` on Unix
/// (`erts/lib_src/common/erl_printf.c:135`, whose flag
/// `erts_printf_add_cr_to_stdout` is set by `erts/emulator/sys/unix/sys.c:342`),
/// which turns every `\n` into `\r\n`. Pinned to the oracle — the bytes of
/// `erlang:display(<<"a b">>)` and `erlang:display([97,98])` are `<<"a b">>\r\n`
/// and `"ab"\r\n`, and the return value is `true`.
///
/// # Documented deviation: a printable latin-1 character ≥ `16#80`
///
/// The oracle's `%T` renderer emits **raw latin-1 code units**, so
/// `erlang:display("ström")` — the list `[115,116,114,246,109]` — writes
/// `22 73 74 72 F6 6D 22 0D 0A` (`erl -noshell -eval 'erlang:display("str\xF6m"), halt().'`).
/// This crate's leaf renderer ([`src/print.rs`](crate::print), `quoted_string`)
/// builds a Rust `String` from those code points — which is what makes the *same*
/// renderer byte-exact for `io:format("~p", …)`, whose oracle path (`io_lib`)
/// really does write UTF-8 — so the `ö` (`16#F6`) leaves here as the **UTF-8**
/// pair: `22 73 74 72 C3 B6 6D 22 0D 0A`. Every code unit ≤ `16#7F` (the whole ASCII
/// surface, i.e. everything an ordinary non-string term renders to) is
/// byte-identical to the oracle; only a latin-1 byte ≥ `16#80` inside a printable
/// string/binary leaf differs. Closing it needs a byte-oriented `OutputSink`
/// (a `context.rs`/`beam_rt` interface change) — the `&str` sink cannot carry a
/// bare `16#F6` — not a fix local to this BIF (see the `display_writes_a_printable_latin1_char_as_utf8`
/// test).
///
/// # Errors
///
/// Propagates the sink's I/O failure, and the leaf renderer's typed error for a
/// term outside the Milestone-1 printable set (never a truncated line).
fn display_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = flatten(&[term_chunk(ctx, args[0], true, UNLIMITED_DEPTH)?]);
    ctx.write_output(&text)?;
    ctx.write_output("\r\n")?;
    ctx.boolean(true)
}

/// The UTF-8 string of `erts_unicode_list_to_buf`'s charlist argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the list is improper,
/// has a non-integer element, or holds a code point that is not a Unicode scalar
/// value (`[16#D800]` — a surrogate — and `[16#110000]` are both `badarg` in the
/// oracle).
fn unicode_charlist_string(list: Eterm) -> Result<String> {
    let mut out = String::new();
    let mut cur = list;
    while access::is_list(cur) {
        let elem = access::car(cur);
        if !access::is_small(elem) {
            return Err(badarg());
        }
        // `char::from_u32` rejects the surrogate range and everything above
        // `16#10FFFF`; `u32::try_from` rejects a negative code point.
        let ch = u32::try_from(signed_value(elem))
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(badarg)?;
        out.push(ch);
        cur = access::cdr(cur);
    }
    if !access::is_nil(cur) {
        return Err(badarg());
    }
    Ok(out)
}

/// `display_string(Stream, String)` (`bif.c:4196`) — writes the string to
/// `Stream` and returns `true`; no line terminator is added.
///
/// `Stream` must be one of the atoms `stdout`/`stderr`/`stdin` (any other term is
/// `badarg`), and `String` must be a Unicode charlist or a byte-aligned binary —
/// `display_string(stdout, 5)`, `display_string(stdout, [<<1:1>>])` and
/// `display_string(stdout, <<1:1>>)` are all `badarg`, while the empty list writes
/// nothing. A charlist is encoded as UTF-8, a binary is written byte for byte:
/// pinned to the oracle, `display_string(stdout, [246])` writes `0xC3 0xB6`,
/// `display_string(stdout, [16#10FFFF])` writes `0xF4 0x8F 0xBF 0xBF`, and
/// `display_string(stdout, <<255,0>>)` writes the raw bytes `0xFF 0x00`.
///
/// Only `stdout` is reproduced: it is the stream the installed
/// [`OutputSink`](crate::context::OutputSink) models. `stderr`/`stdin` (the C's
/// `write(2)`/`ioctl(TIOCSTI)` paths) and a binary payload that is not valid UTF-8
/// (the sink takes a `&str`) report the typed `unsupported_bif` instead of writing
/// the bytes to the wrong place or substituting them.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) as described above;
/// `error:unsupported_bif` for the two unmodelled cases; otherwise the sink's I/O
/// failure.
fn display_string_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let stream = args[0];
    if !access::is_atom(stream) {
        return Err(badarg());
    }
    // The C accepts exactly `stdout`, `stderr` and (with TIOCSTI) `stdin`; every
    // other atom — including one that merely *names* a stream — is `badarg`.
    let name = ctx.atoms().name(atom_val(stream));
    let stdout = name == Some("stdout");
    if !stdout && name != Some("stderr") && name != Some("stdin") {
        return Err(badarg());
    }

    let payload = args[1];
    let text = if access::is_nil(payload) || access::is_list(payload) {
        unicode_charlist_string(payload)?
    } else if bits::is_bitstring(payload) {
        // `erts_get_aligned_binary_bytes`: a non-byte-aligned bitstring is `badarg`.
        match String::from_utf8(bits::bytes(payload)?) {
            Ok(text) => text,
            Err(_) => return Err(unsupported_bif()),
        }
    } else {
        return Err(badarg());
    };

    if !stdout {
        return Err(unsupported_bif());
    }
    ctx.write_output(&text)?;
    ctx.boolean(true)
}

// ---------------------------------------------------------------------------
// The textual pid/ref/port conversions
//
//   pid_to_list/1  list_to_pid/1  ref_to_list/1  list_to_ref/1
//   pid_to_binary/1  ref_to_binary/1  port_to_binary/1
// ---------------------------------------------------------------------------

/// The node number of the local node — the `%T` renderer's `pid_channel_no` /
/// `ref_channel_no` / `port_channel_no` (`erl_printf_term.c:512`, `:500`, `:525`),
/// which is `0` for the `nonode@nohost` node Milestone-1 always is. The same
/// constant `src/port.rs` uses for `#Port<0.Num>`.
const LOCAL_NODE_NUMBER: u64 = 0;

/// The largest `erlang:list_to_pid/1` string `intlist_to_buf` accepts: the C buffer
/// is 65 bytes and the call limit 64 (`bif.c:4538-4547`), so a longer list is
/// `badarg` even when it spells a valid pid (oracle: a 65-character
/// `<0.000…001.0>` ⇒ `badarg`, while the 64-character form parses).
const PID_TEXT_MAX: usize = 64;

/// The largest `erlang:list_to_ref/1` string: the C buffer is
/// `5 + (1 + ERTS_MAX_REF_NUMBERS)*(10 + 1) + 1 == 72` bytes with the call limit
/// `sizeof(buf) - 1 == 71` (`bif.c:4727-4732`) — a 72-character list is `badarg`
/// (oracle-verified at the 71/72 boundary).
const REF_TEXT_MAX: usize = 71;

/// The oracle's `MAX_REFERENCE = 1 << _REF_NUM_SIZE = 1 << 18 = 262144`
/// (`erl_term.h:768,771`): `list_to_ref_1` rejects an id `>= MAX_REFERENCE` with
/// `badarg` (`bif.c:4777`). This is the *C's* bound and is tighter than the
/// Milestone-1 ref thing's own reach (its `id` word is a 60-bit small integer,
/// `access::make_ref_thing`), so the oracle's bound is what a byte-faithful
/// `list_to_ref/1` must enforce. Oracle-verified at the boundary:
/// `#Ref<0.0.0.262143>` parses, `#Ref<0.0.0.262144>` and `#Ref<0.0.0.4294967295>`
/// are `badarg`.
const MAX_REFERENCE: u64 = 1 << 18;

/// The refusal for the textual conversions' one unreachable case: a pid/port/ref
/// naming **another** node.
///
/// [`pid_text_str`], [`port_text_str`] and [`ref_text_str`] all reject an external
/// identifier, and the rejection is deliberate: `access::is_pid`/`is_port`/
/// `is_reference` accept the external forms and the oracle *does* render them, so
/// this is the documented capability gap — never a fabricated channel number.
///
/// # Why the text is not derivable from the term
///
/// The `%T` renderer prints the identifier's **channel number**
/// (`erl_printf_term.c:500`, `:512`, `:525`), and for a *foreign* node the C defines
/// that as `atom_val(x->sysname)` (`dist_entry_channel_no`,
/// `erl_node_container_utils.h:103-116`): `(x) == erts_this_dist_entry ?
/// ERST_INTERNAL_CHANNEL_NO (= 0) : atom_val((x)->sysname)`. That is the index the
/// node-name atom happens to hold **in the running VM's atom table** — pure runtime
/// state. The frozen external-identifier term cannot carry it: the Milestone-1
/// substitution in `beam_term::thing::write_external_{pid,port,ref}` stores the node
/// name as an atom *term* (plus the creation) where the C stores a `dist_entry`
/// pointer, and this crate's `AtomTable` index is a third, unrelated number.
///
/// # Oracle record (OTP 29.1.1 / erts-17.1) — verbatim
///
/// One and the same wire pid printed by two runs that differ only in how many atoms
/// were interned before the decode. The channel is *exactly* the index `foo@bar`
/// received when the decode interned it:
///
/// ```text
/// $ erl -noshell -eval 'W = <<131,88,119,7,"foo@bar",0,0,0,10,0,0,0,0,0,0,0,0>>,
///     _ = [list_to_atom("zzzz" ++ integer_to_list(I)) || I <- lists:seq(1,0)],
///     io:format("atoms=~p channel=~s~n",[erlang:system_info(atom_count),
///                                        erlang:pid_to_list(binary_to_term(W))]),
///     halt().'
/// atoms=10638 channel=<10638.10.0>
/// $ ...the same, only lists:seq(1,100)...  (100 extra atoms interned first)
/// atoms=10738 channel=<10738.10.0>
/// ```
///
/// Two different foreign names decoded in one run print consecutive channels, in
/// decode order:
///
/// ```text
/// $ erl -noshell -eval 'W = fun(N) -> binary_to_term(
///     <<131,88,119,7,N/binary,0,0,0,1,0,0,0,0,0,0,0,0>>) end,
///     io:format("~s ~s~n",[erlang:pid_to_list(W(<<"foo@bar">>)),
///                          erlang:pid_to_list(W(<<"baz@qux">>))]), halt().'
/// <10636.1.0> <10637.1.0>
/// ```
///
/// All three identifier kinds for the foreign node `foo@barbazqux` (13 characters,
/// so a `nonode@nohost` image can be patched in place with `binary:replace/3`); the
/// channel is that name's atom index in the run:
///
/// ```text
/// node(P) == foo@barbazqux
/// erlang:pid_to_list(P)  == <10640.10.0>
/// erlang:ref_to_list(R)  == #Ref<10640.210497935.1828192262.122813>
/// erlang:port_to_list(P) == #Port<10640.7>
/// ```
///
/// The **local** node is the one branch the C resolves statically (the comparison is
/// by node name; the creation does not enter the printed channel) and it prints
/// channel `0` — the substitution the three functions below already make for the
/// internal forms:
///
/// ```text
/// binary_to_term(<<131,88,119,13,"nonode@nohost",0,0,0,1,0,0,0,7,0,0,0,0>>)
///   => <0.1.7>,  erlang:pid_to_list(…) == <0.1.7>
/// binary_to_term(<<131,120,119,13,"nonode@nohost",0,0,0,0,0,0,0,0,0,0,0,0>>)
///   => #Port<0.0>
/// ```
///
/// # What a faithful rendering would need
///
/// A **node-identity seam** this crate does not have: the runtime's own node name
/// (so an external identifier naming *this* node takes the channel-`0` branch) *and*
/// a node-atom -> dist-channel provider (the running VM's `atom_val`/dist-entry
/// registry) for every other node. A number taken from this crate's own `AtomTable`
/// would be a *silently wrong* result — the same conclusion `crate::print`'s module
/// docs reach for `~p`/`~w`. Both pieces are `beam_rt`-level state; adding either is
/// a new dependency edge and is reported upward rather than guessed here.
fn external_channel_gap() -> BeamError {
    badarg()
}

/// The `%T` text of an internal pid — `<channel.number.serial>`
/// (`erl_printf_term.c:508-519`): the channel is the local node's, then the
/// process-table number, then the serial.
///
/// This is the **one** home of that text: `crate::print`'s leaf renderer splices it
/// in for an internal pid (the oracle's `io_lib:write1/4` calls the same
/// `erlang:pid_to_list/1`) instead of re-deriving the shape.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a pid
/// (`erlang:pid_to_list(5)` ⇒ `badarg`). An **external** pid is likewise `badarg`:
/// its channel number is not derivable from the term (see
/// [`external_channel_gap`]).
pub(crate) fn pid_text_str(term: Eterm) -> Result<String> {
    if !access::is_pid(term) {
        return Err(badarg());
    }
    // Stated before the decode, which would reject it anyway (`ProcessId::from_eterm`
    // is the internal-only immediate): the *reason* is the channel number, see
    // [`external_channel_gap`].
    if access::is_external_pid(term) {
        return Err(external_channel_gap());
    }
    // `ProcessId::from_eterm` is the crate's pid decode.
    let pid = ProcessId::from_eterm(term)?;
    Ok(format!(
        "<{}.{}.{}>",
        LOCAL_NODE_NUMBER,
        pid.number(),
        pid.serial()
    ))
}

/// The `%T` text of an internal port — `#Port<channel.number>`
/// (`erl_printf_term.c:521-530`), the shape `src/port.rs`'s `port_to_list/1`
/// builds as a charlist and `crate::print`'s leaf renderer splices in.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a port,
/// or is an external one (see [`external_channel_gap`]).
pub(crate) fn port_text_str(term: Eterm) -> Result<String> {
    if !access::is_port(term) {
        return Err(badarg());
    }
    if access::is_external_port(term) {
        return Err(external_channel_gap());
    }
    let port = PortId::from_eterm(term)?;
    Ok(format!("#Port<{}.{}>", LOCAL_NODE_NUMBER, port.number()))
}

/// The `%T` text of an internal reference — `#Ref<channel.n2.n1.n0>`
/// (`erl_printf_term.c:493-507`; the ref numbers print most significant first).
///
/// The Milestone-1 ref thing is `[header, node, serial, id]`
/// (`access::make_ref_thing`, this crate's only producer of an internal ref), so it
/// carries **one** id word; the rendered text is the `#Ref<0.0.0.N>` shape
/// `crate::print`'s leaf renderer splices in.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a
/// reference (`erlang:ref_to_list(5)` ⇒ `badarg`), or is an external one (its
/// channel number is not derivable from the term — see
/// [`external_channel_gap`]).
pub(crate) fn ref_text_str(term: Eterm) -> Result<String> {
    if !access::is_reference(term) {
        return Err(badarg());
    }
    if access::is_external_ref(term) {
        return Err(external_channel_gap());
    }
    let id = access::ref_thing_id(term)?;
    Ok(format!("#Ref<{}.0.0.{}>", LOCAL_NODE_NUMBER, id))
}

/// Collects `term` into the bytes of a proper list of byte-sized small integers —
/// the C `intlist_to_buf` (`utils.c:2828`): `[]` yields no bytes, and a non-list, an
/// improper list, a non-integer element, an element outside `0..=255`, or a list
/// longer than `max_len` (the C's buffer bound) is `badarg`. The walk is bounded by
/// the list's own length.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) as described above.
fn string_bytes(term: Eterm, max_len: usize) -> Result<Vec<u8>> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    if elems.len() > max_len {
        return Err(badarg());
    }
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        bytes.push(u8::try_from(signed_value(elem)).map_err(|_| badarg())?);
    }
    Ok(bytes)
}

/// The charlist `[b0, b1, …]` of the bytes `bytes` (`buf_to_intlist`, `bif.c:4461`).
///
/// # Errors
///
/// Propagates a heap-allocation failure.
fn bytes_to_string(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(bytes.len());
    for byte in bytes {
        elems.push(ctx.int(i64::from(*byte))?);
    }
    ctx.list(&elems)
}

/// The `u64` value of a non-empty run of ASCII digits — the C's
/// `while (*cp >= '0' && *cp <= '9')` field scan (`bif.c:4555`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for an empty field, a sign,
/// whitespace, any non-digit, or a value past `u64` (`<999999999999999999999999.1.0>`
/// ⇒ `badarg`). The scan is bounded by the field's own length.
fn parse_digits(field: &str) -> Result<u64> {
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
        return Err(badarg());
    }
    field.parse::<u64>().map_err(|_| badarg())
}

/// Parses `<channel.number.serial>` into the internal pid it names (`bif.c:4532`).
///
/// The grammar is the C's: a leading `<`, three non-empty decimal fields separated
/// by `.`, a trailing `>`, and nothing else. The channel must be the local node's
/// (`0`); the number and serial must fit the internal pid immediate
/// (`beam_process::{MAX_PROCESS_NUMBER, MAX_PROCESS_SERIAL}`), the C's
/// `ERTS_MAX_INTERNAL_PID_NUMBER`/`_SERIAL` checks, so `<0.268435456.0>` and
/// `<0.0.4294967296>` are `badarg`. Leading zeros are accepted (`<0.01.0>` ⇒
/// `<0.1.0>`), as in the C.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for every deviation.
fn parse_pid_text(bytes: &[u8]) -> Result<ProcessId> {
    let text = core::str::from_utf8(bytes).map_err(|_| badarg())?;
    let body = text
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
        .ok_or_else(badarg)?;
    let mut fields = body.split('.');
    let channel = parse_digits(fields.next().ok_or_else(badarg)?)?;
    let number = parse_digits(fields.next().ok_or_else(badarg)?)?;
    let serial = parse_digits(fields.next().ok_or_else(badarg)?)?;
    if fields.next().is_some() || channel != LOCAL_NODE_NUMBER {
        return Err(badarg());
    }
    if number > MAX_PROCESS_NUMBER || serial > MAX_PROCESS_SERIAL {
        return Err(badarg());
    }
    Ok(ProcessId::new(number, serial))
}

/// Parses `#Ref<0.0.0.N>` (the shape [`ref_text`] renders) into the Milestone-1
/// ref thing's id (`bif.c:4712`).
///
/// The textual form is `#Ref<channel.n2.n1.n0>`. The Milestone-1 ref thing carries a
/// **single** id word plus the (always zero) node/serial words, so only the exact
/// `#Ref<0.0.0.N>` shape is representable: a non-local channel, a non-zero upper ref
/// number, or a shape with the wrong field count (`#Ref<0.0.0>`,
/// `#Ref<0.0.0.1.2>`) names a reference this runtime cannot materialize and is
/// `badarg` — never a fabricated term. The id must also satisfy the oracle's
/// `id < `[`MAX_REFERENCE`] (`bif.c:4777`): `#Ref<0.0.0.262144>` and above are
/// `badarg`, exactly as the C rejects them before the id can leave its range. Leading
/// zeros are accepted (`#Ref<00.00.00.01>` ⇒ `#Ref<0.0.0.1>`), as in the C.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for every deviation.
fn parse_ref_text(bytes: &[u8]) -> Result<u64> {
    let text = core::str::from_utf8(bytes).map_err(|_| badarg())?;
    let body = text
        .strip_prefix("#Ref<")
        .and_then(|rest| rest.strip_suffix('>'))
        .ok_or_else(badarg)?;
    let mut fields = body.split('.');
    let channel = parse_digits(fields.next().ok_or_else(badarg)?)?;
    let upper1 = parse_digits(fields.next().ok_or_else(badarg)?)?;
    let upper0 = parse_digits(fields.next().ok_or_else(badarg)?)?;
    let id = parse_digits(fields.next().ok_or_else(badarg)?)?;
    if fields.next().is_some() {
        return Err(badarg());
    }
    if channel != LOCAL_NODE_NUMBER || upper1 != 0 || upper0 != 0 {
        return Err(badarg());
    }
    if id >= MAX_REFERENCE {
        return Err(badarg());
    }
    Ok(id)
}

/// `pid_to_list(Pid)` (`bif.c:4512`) — the pid's `%T` text as a charlist.
///
/// Pinned to the oracle: `pid_to_list(list_to_pid("<0.123.7>"))` ⇒
/// `"<0.123.7>"` (`[60,48,46,49,50,51,46,55,62]`), `pid_to_list(self())` ⇒
/// `"<0.10.0>"`; a non-pid is `badarg`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) per [`pid_text_str`].
fn pid_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = pid_text_str(args[0])?;
    bytes_to_string(ctx, text.as_bytes())
}

/// `pid_to_binary(Pid)` (`bif.c:4879`) — the *same* `%T` bytes as
/// [`pid_to_list_1`], as a binary. An OTP-30 addition the 29.1.1 oracle does not
/// export; the bytes are the C's shared `term2binary_dsprintf` format.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) per [`pid_text_str`], or
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc) when the heap cannot fit the
/// binary.
fn pid_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = pid_text_str(args[0])?;
    bits::make_binary(ctx.heap(), text.as_bytes())
}

/// `list_to_pid(PidString)` (`bif.c:4532`) — the internal pid the text names.
///
/// Pinned to the oracle: `list_to_pid("<0.123.7>")` ⇒ `<0.123.7>` and
/// `list_to_pid(pid_to_list(P)) =:= P`. `list_to_pid("bogus")`, a missing field
/// (`"<0.10.0"`), a non-byte element (`[300]`), a binary argument and an
/// out-of-range field (`"<0.268435456.0>"`) are all `badarg`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed string, an
/// argument that is not a proper list of bytes, or a pid the internal immediate
/// cannot hold. The context is unused (nothing is allocated).
fn list_to_pid_1(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = string_bytes(args[0], PID_TEXT_MAX)?;
    Ok(parse_pid_text(&bytes)?.to_eterm())
}

/// `ref_to_list(Ref)` (`bif.c:4466`) — the reference's `%T` text as a charlist.
///
/// Pinned to the oracle: `ref_to_list(list_to_ref("#Ref<0.0.0.7>"))` ⇒
/// `"#Ref<0.0.0.7>"`, and a reference built by the runtime's own ref thing renders
/// the same `#Ref<0.0.0.N>` shape; a non-reference is `badarg`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) per [`ref_text_str`].
fn ref_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = ref_text_str(args[0])?;
    bytes_to_string(ctx, text.as_bytes())
}

/// `ref_to_binary(Ref)` (`bif.c:4893`) — the same `%T` bytes as [`ref_to_list_1`],
/// as a binary. OTP-30 only (see [`pid_to_binary_1`]).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) per [`ref_text_str`], or
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc).
fn ref_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = ref_text_str(args[0])?;
    bits::make_binary(ctx.heap(), text.as_bytes())
}

/// `port_to_binary(Port)` (`bif.c:4886`) — the port's `#Port<channel.number>` text as
/// a binary. OTP-30 only (see [`pid_to_binary_1`]).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) per [`port_text_str`], or
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc).
fn port_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let text = port_text_str(args[0])?;
    bits::make_binary(ctx.heap(), text.as_bytes())
}

/// `list_to_ref(RefString)` (`bif.c:4712`) — the internal reference the text names.
///
/// Pinned to the `#Ref<0.0.0.N>` shape this runtime's ref thing holds, so
/// `list_to_ref(ref_to_list(R)) =:= R`. `list_to_ref("bogus")`,
/// `list_to_ref("#Ref<0.0.0>")`, `list_to_ref("#Ref<0.0.0.1.2>")`,
/// `list_to_ref("#Ref<1.0.0.1>")` and `list_to_ref("#Ref<0.1.0.1>")` are all
/// `badarg`: the last two name a reference whose node/upper ref numbers this
/// runtime's single-id-word ref thing cannot carry, so they are an honest gap
/// rather than a fabricated reference (see [`parse_ref_text`]). The id is bounded by
/// the oracle's `id < `[`MAX_REFERENCE`]: `list_to_ref("#Ref<0.0.0.262144>")` is
/// `badarg` (`bif.c:4777`), so no id the oracle can never hold is materialized.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a malformed string, an
/// argument that is not a proper list of bytes, or an unrepresentable reference;
/// otherwise propagates [`BeamError::Alloc`](beam_common::BeamError::Alloc).
fn list_to_ref_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = string_bytes(args[0], REF_TEXT_MAX)?;
    let id = parse_ref_text(&bytes)?;
    access::make_ref_thing(ctx.heap(), id)
}

// ---------------------------------------------------------------------------
// external_size/1,2
// ---------------------------------------------------------------------------

/// `external_size(Term)` (`external.c:2196-2218`) — the byte size of `Term`'s
/// external term image, without building it.
///
/// The oracle's `external_size_1` runs `erts_encode_ext_size`, i.e. the size probe
/// under the **default** `TERM_TO_BINARY_DFLAGS` — exactly
/// [`T2bOptions::default`]. Pinned to the oracle:
/// `external_size(5)` ⇒ `3`, `external_size(a)` ⇒ `4`, `external_size(hello)` ⇒ `8`,
/// `external_size([1,2,3])` ⇒ `7`, `external_size({a,b,c})` ⇒ `12`,
/// `external_size(1.5)` ⇒ `10`, `external_size(1 bsl 100)` ⇒ `17` — each equal to
/// `byte_size(term_to_binary(Term))`, as the doc contract requires
/// (`Size1 =< Size2`, `../../erts/preloaded/src/erlang.erl:2658-2679`).
///
/// # Errors
///
/// The encoder's errors (see [`external_size`]).
fn external_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    external_size(ctx, args[0], &T2bOptions::default())
}

/// `external_size(Term, Options)` (`external.c:2221-2246`) — the same size under the
/// option list `term_to_binary/2` accepts.
///
/// The option list is validated by the **same** parser `term_to_binary/2` uses, so
/// the rejections are identical: `external_size(5, [bogus])`,
/// `external_size(5, [{minor_version, 3}])` and `external_size(5, [{compressed, 10}])`
/// are all `badarg`. Only `{minor_version, N}` changes the number — it selects the
/// tag set (`external_size(a, [{minor_version, 0}])` ⇒ `5` and
/// `[{minor_version, 1}]` ⇒ `5`, against the default `4`; the legacy `FLOAT_EXT` of
/// `{minor_version, 0}` makes `external_size(1.5, [{minor_version, 0}])` ⇒ `33`).
/// `compressed`/`{compressed, Level}` is validated but does **not** shrink the result
/// — see [`external_size`].
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for the option lists
/// `term_to_binary/2` rejects; the encoder's errors otherwise.
fn external_size_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let options = parse_term_to_binary_options(ctx.atoms(), args[1])?;
    external_size(ctx, args[0], &options)
}

/// The shared `external_size` core: the length of the **uncompressed** external image
/// of `term` under `options`, as an integer term.
///
/// The image is [`plain_image_of`]'s — the version byte plus the same body
/// `term_to_binary/2` writes under the option list's tag set — so the two BIFs share
/// one parser and one encoder. The compression level is deliberately ignored:
/// the oracle's `external_size_2` (`external.c:2221-2246`) never passes it to
/// `erts_encode_ext_size_2` (`external.c:736-743`), and the documented contract is
/// `byte_size(term_to_binary(T, Opts)) =< erlang:external_size(T, Opts)`
/// (`../../erts/preloaded/src/erlang.erl:2687-2710`).
///
/// The result is materialized through [`arith::int_from_i64`], the crate's single
/// small-int-or-bignum promotion path — mirroring the C's `IS_USMALL(0, size) ?
/// make_small(size) : uint_to_big(size, hp)` (`external.c:2212-2217`), so a size
/// beyond the 60-bit small range would be a bignum rather than an error.
///
/// # Errors
///
/// The encoder's errors: `error:unsupported_external_term` for a term the codec
/// cannot write (an internal pid/port id with no resolvable local node, an off-heap
/// `BinRef`), `error:unsupported_bif` for a local (`make_fun3`) fun with no identity
/// hook, `system_limit` for a wire bound that cannot be met. A term the codec cannot
/// encode is **never** approximated by a number.
fn external_size(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: &T2bOptions,
) -> Result<Eterm> {
    let image = plain_image_of(ctx, term, options)?;
    let size = i64::try_from(image.len()).map_err(|_| system_limit())?;
    arith::int_from_i64(ctx, size)
}

/// Registers this module's BIFs (seventeen keys: sixteen `erlang` and one `binary`).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "bitstring_to_list", 1, bitstring_to_list_1);
    reg.register("erlang", "delete_element", 2, delete_element_2);
    reg.register("erlang", "insert_element", 3, insert_element_3);
    reg.register("erlang", "iolist_to_iovec", 1, iolist_to_iovec_1);
    reg.register("erlang", "is_integer", 3, is_integer_3);
    reg.register("erlang", "display", 1, display_1);
    reg.register("erlang", "display_string", 2, display_string_2);
    reg.register("erlang", "external_size", 1, external_size_1);
    reg.register("erlang", "external_size", 2, external_size_2);
    reg.register("binary", "referenced_byte_size", 1, referenced_byte_size_1);
    reg.register("erlang", "pid_to_list", 1, pid_to_list_1);
    reg.register("erlang", "list_to_pid", 1, list_to_pid_1);
    reg.register("erlang", "ref_to_list", 1, ref_to_list_1);
    reg.register("erlang", "list_to_ref", 1, list_to_ref_1);
    reg.register("erlang", "pid_to_binary", 1, pid_to_binary_1);
    reg.register("erlang", "ref_to_binary", 1, ref_to_binary_1);
    reg.register("erlang", "port_to_binary", 1, port_to_binary_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    #[test]
    fn register_installs_the_roster() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        assert_eq!(reg.len(), 17);
        assert!(reg.contains("erlang", "bitstring_to_list", 1));
        assert!(reg.contains("binary", "referenced_byte_size", 1));
        for name in [
            "pid_to_list",
            "list_to_pid",
            "ref_to_list",
            "list_to_ref",
            "pid_to_binary",
            "ref_to_binary",
            "port_to_binary",
        ] {
            assert!(reg.contains("erlang", name, 1), "{name}/1 is registered");
        }
        // Both `external_size` arities are registered (`bif.tab:68-69`).
        assert!(reg.contains("erlang", "external_size", 1));
        assert!(reg.contains("erlang", "external_size", 2));
    }

    #[test]
    fn the_textual_conversions_round_trip_and_reject_bad_text() {
        with_ctx(|ctx| {
            // `pid_to_list/1` / `list_to_pid/1`: `<0.123.7>` round-trips.
            let pid = ProcessId::new(123, 7).to_eterm();
            let text = pid_to_list_1(ctx, &[pid]).expect("a pid");
            assert_eq!(string_of(text), "<0.123.7>");
            let back = list_to_pid_1(ctx, &[text]).expect("the text parses");
            assert_eq!(ProcessId::from_eterm(back).expect("a pid"), ProcessId::new(123, 7));

            // Leading zeros are accepted; out-of-range fields are `badarg`.
            let zeros = byte_chars(ctx, b"<0.01.0>");
            let back = list_to_pid_1(ctx, &[zeros]).expect("the text parses");
            assert_eq!(ProcessId::from_eterm(back).expect("a pid"), ProcessId::new(1, 0));
            for bad in ["<0.268435456.0>", "<0.0.4294967296>", "<1.0.0>", "<0.0.0.0>", "<0.1.0"] {
                let text = byte_chars(ctx, bad.as_bytes());
                assert!(matches!(list_to_pid_1(ctx, &[text]), Err(BeamError::BadArg)));
            }

            // `ref_to_list/1` / `list_to_ref/1`: `#Ref<0.0.0.7>` round-trips.
            let text = byte_chars(ctx, b"#Ref<0.0.0.7>");
            let reference = list_to_ref_1(ctx, &[text]).expect("the text parses");
            let back = ref_to_list_1(ctx, &[reference]).expect("a reference");
            assert_eq!(string_of(back), "#Ref<0.0.0.7>");
            // Leading zeros normalize.
            let zeros = byte_chars(ctx, b"#Ref<00.00.00.07>");
            let reference = list_to_ref_1(ctx, &[zeros]).expect("the text parses");
            let back = ref_to_list_1(ctx, &[reference]).expect("a reference");
            assert_eq!(string_of(back), "#Ref<0.0.0.7>");
            // A shape the single-id-word ref thing cannot carry is `badarg`
            // (`#Ref<1.0.0.1>`/`#Ref<0.1.0.1>` are fine in the oracle but name a node
            // or upper ref number this runtime cannot hold), and so is every
            // malformed prefix/suffix/field the oracle rejects with `badarg`.
            for bad in [
                "bogus",
                "#Ref<0.0.0>",
                "#Ref<0.0.0.1.2>",
                "#Ref<1.0.0.1>",
                "#Ref<0.1.0.1>",
                "#Ref<0.0.1.1>",
                "#Refx<0.0.0.1>",
                "#ref<0.0.0.1>",
                "#Ref<0.0.0.1>x",
                "#Ref<0.0.0.1",
                "#Ref<0.0.0.-1>",
                "#Ref<0.0.0.+1>",
                "#Ref<0.0.0. 1>",
                "#Ref<0.0.0.1.>",
                "#Ref<0.0.0.1.a>",
                "#Ref<0.0.0.1e2>",
            ] {
                let text = byte_chars(ctx, bad.as_bytes());
                assert!(
                    matches!(list_to_ref_1(ctx, &[text]), Err(BeamError::BadArg)),
                    "{bad} must be badarg"
                );
            }

            // A non-pid/non-ref and the `_to_binary` byte equality.
            let five = make_small(5);
            assert!(matches!(pid_to_list_1(ctx, &[five]), Err(BeamError::BadArg)));
            assert!(matches!(pid_to_binary_1(ctx, &[five]), Err(BeamError::BadArg)));
            assert!(matches!(ref_to_list_1(ctx, &[NIL]), Err(BeamError::BadArg)));
            assert!(matches!(ref_to_binary_1(ctx, &[NIL]), Err(BeamError::BadArg)));
            assert!(matches!(port_to_binary_1(ctx, &[NIL]), Err(BeamError::BadArg)));
            let pid = ProcessId::new(123, 7).to_eterm();
            let as_list = pid_to_list_1(ctx, &[pid]).expect("a pid");
            let as_binary = pid_to_binary_1(ctx, &[pid]).expect("a pid");
            assert_eq!(
                bits::bytes(as_binary).expect("a binary"),
                string_of(as_list).into_bytes()
            );
            let port = beam_process::PortId::new(3).to_eterm();
            assert_eq!(
                bits::bytes(port_to_binary_1(ctx, &[port]).expect("a port")).expect("a binary"),
                b"#Port<0.3>"
            );
        });
    }

    #[test]
    fn the_ref_id_is_bounded_by_the_oracles_max_reference() {
        // The C's `refn[0] >= MAX_REFERENCE` check (`bif.c:4777`), with
        // `MAX_REFERENCE = 1 << _REF_NUM_SIZE = 1 << 18 = 262144`
        // (`erl_term.h:768,771`). The oracle's bound — not the Milestone-1 ref thing's
        // 60-bit reach — is what `list_to_ref/1` must enforce, else an id the oracle
        // can never materialize is fabricated. ORACLE:
        //   erlang:list_to_ref("#Ref<0.0.0.262143>") => #Ref<0.0.0.262143>
        //   erlang:list_to_ref("#Ref<0.0.0.262144>") => error:badarg
        //   erlang:list_to_ref("#Ref<0.0.0.4294967295>") => error:badarg
        with_ctx(|ctx| {
            // The last representable ids round-trip with their exact value.
            for (text, id) in [("#Ref<0.0.0.0>", 0u64), ("#Ref<0.0.0.262143>", 262_143)] {
                let chars = byte_chars(ctx, text.as_bytes());
                let reference = list_to_ref_1(ctx, &[chars]).expect("an in-range id");
                assert_eq!(access::ref_thing_id(reference).expect("a ref thing"), id);
            }
            // `id >= MAX_REFERENCE` is `badarg`, never a fabricated reference.
            for bad in [
                "#Ref<0.0.0.262144>",
                "#Ref<0.0.0.262145>",
                "#Ref<0.0.0.4294967295>",
                "#Ref<0.0.0.4294967296>",
                "#Ref<0.0.0.1152921504606846975>",
            ] {
                let chars = byte_chars(ctx, bad.as_bytes());
                assert!(
                    matches!(list_to_ref_1(ctx, &[chars]), Err(BeamError::BadArg)),
                    "{bad} must be badarg (id >= MAX_REFERENCE)"
                );
            }
            // Leading-zero padding still normalizes to the small value, as in the C.
            let padded = byte_chars(ctx, b"#Ref<0.0.0.0000000000000001>");
            let reference = list_to_ref_1(ctx, &[padded]).expect("an in-range id");
            assert_eq!(access::ref_thing_id(reference).expect("a ref thing"), 1);
        });
    }

    /// The bytes of the charlist `list`, as a `String`.
    fn string_of(list: Eterm) -> String {
        let mut out = Vec::new();
        let mut cur = list;
        while access::is_list(cur) {
            out.push(u8::try_from(signed_value(access::car(cur))).expect("a byte"));
            cur = access::cdr(cur);
        }
        String::from_utf8(out).expect("ASCII")
    }

    /// The charlist of the bytes `bytes`.
    fn byte_chars(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = bytes.iter().map(|b| ctx.int(i64::from(*b)).expect("fits")).collect();
        ctx.list(&elems).expect("fits")
    }

    #[test]
    fn the_string_length_bounds_mirror_the_c_buffers() {
        // `list_to_pid/1`'s buffer is 65 bytes with a 64-byte call limit and
        // `list_to_ref/1`'s is 72 with a 71-byte limit (`bif.c:4538`, `bif.c:4727`):
        // a string one byte past either limit is `badarg` even when it parses.
        with_ctx(|ctx| {
            let mut pid_64 = String::from("<0.");
            pid_64.push_str(&"0".repeat(57));
            pid_64.push_str("1.0>");
            assert_eq!(pid_64.len(), 64);
            let text = byte_chars(ctx, pid_64.as_bytes());
            assert!(list_to_pid_1(ctx, &[text]).is_ok(), "64 bytes is accepted");

            let mut pid_65 = String::from("<0.");
            pid_65.push_str(&"0".repeat(58));
            pid_65.push_str("1.0>");
            assert_eq!(pid_65.len(), 65);
            let text = byte_chars(ctx, pid_65.as_bytes());
            assert!(matches!(list_to_pid_1(ctx, &[text]), Err(BeamError::BadArg)));

            let mut ref_71 = String::from("#Ref<0.");
            ref_71.push_str(&"0".repeat(58));
            ref_71.push_str("1.2.3>");
            assert_eq!(ref_71.len(), 71);
            let text = byte_chars(ctx, ref_71.as_bytes());
            // In range for the C buffer, but `1.2.3` is not the M1 shape.
            assert!(matches!(list_to_ref_1(ctx, &[text]), Err(BeamError::BadArg)));

            let mut ref_72 = String::from("#Ref<0.");
            ref_72.push_str(&"0".repeat(59));
            ref_72.push_str("1.2.3>");
            assert_eq!(ref_72.len(), 72);
            let text = byte_chars(ctx, ref_72.as_bytes());
            assert!(matches!(list_to_ref_1(ctx, &[text]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn bitstring_to_list_splits_the_trailing_bits() {
        with_ctx(|ctx| {
            // `<<1,2,3>>` ⇒ `[1,2,3]`.
            let three = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            let list = bitstring_to_list_1(ctx, &[three]).expect("a bitstring");
            assert_eq!(signed_value(access::car(list)), 1);
            let rest = access::cdr(access::cdr(list));
            assert_eq!(signed_value(access::car(rest)), 3);
            // `<<>>` ⇒ `[]`.
            let empty = bits::make_binary(ctx.heap(), &[]).expect("fits");
            assert_eq!(bitstring_to_list_1(ctx, &[empty]).expect("a bitstring"), NIL);
            // A non-bitstring is `badarg`.
            assert!(matches!(
                bitstring_to_list_1(ctx, &[make_small(7)]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn delete_and_insert_element_are_one_based() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let tuple = ctx.tuple(&[a, b]).expect("fits");

            let dropped = delete_element_2(ctx, &[make_small(1), tuple]).expect("fits");
            assert_eq!(access::tuple_size(dropped), 1);
            assert_eq!(access::tuple_element(dropped, 0), b);

            let inserted = insert_element_3(ctx, &[make_small(2), tuple, a]).expect("fits");
            assert_eq!(access::tuple_size(inserted), 3);
            assert_eq!(access::tuple_element(inserted, 1), a);

            // `0`, `arity + 1` (for delete) and a non-tuple are `badarg`.
            assert!(matches!(
                delete_element_2(ctx, &[make_small(0), tuple]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                delete_element_2(ctx, &[make_small(3), tuple]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                insert_element_3(ctx, &[make_small(4), tuple, a]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                insert_element_3(ctx, &[make_small(1), NIL, a]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn referenced_byte_size_is_the_byte_size() {
        with_ctx(|ctx| {
            for n in [0usize, 3, 64, 65, 1000] {
                let bin = bits::make_binary(ctx.heap(), &vec![7u8; n]).expect("fits");
                assert_eq!(
                    signed_value(referenced_byte_size_1(ctx, &[bin]).expect("a bitstring")),
                    n as i64
                );
            }
            // A partial trailing byte truncates, like `size/1`.
            let one = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert_eq!(signed_value(referenced_byte_size_1(ctx, &[one]).expect("a string")), 0);
            assert!(matches!(
                referenced_byte_size_1(ctx, &[NIL]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn is_integer_3_checks_the_range_arithmetically() {
        with_ctx(|ctx| {
            let five = ctx.int(5).expect("fits");
            let one = ctx.int(1).expect("fits");
            let ten = ctx.int(10).expect("fits");
            let within = is_integer_3(ctx, &[five, one, ten]).expect("integer bounds");
            assert_eq!(ctx.atoms().name(atom_val(within)), Some("true"));
            let without = is_integer_3(ctx, &[five, ten, one]).expect("integer bounds");
            assert_eq!(ctx.atoms().name(atom_val(without)), Some("false"));

            let float = ctx.make_float(1.0).expect("fits");
            assert!(matches!(
                is_integer_3(ctx, &[five, float, ten]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                is_integer_3(ctx, &[five, one, float]),
                Err(BeamError::BadArg)
            ));
            // A non-integer `Term` is `false` with valid bounds.
            let a = ctx.atom("a").expect("seeded");
            let non_integer = is_integer_3(ctx, &[a, one, ten]).expect("integer bounds");
            assert_eq!(ctx.atoms().name(atom_val(non_integer)), Some("false"));
        });
    }

    #[test]
    fn display_and_display_string_route_through_the_sink() {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = atoms();
        atoms.seed(["stdout", "stderr", "stdin"]);
        let mut sink = crate::context::BytesSink::new();
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_output(&mut sink);
            // `erlang:display(<<"a b">>)` ⇒ `<<"a b">>\r\n` (the oracle's bytes).
            let bin = bits::make_binary(ctx.heap(), b"a b").expect("fits");
            let result = display_1(&mut ctx, &[bin]).expect("renderable");
            assert_eq!(ctx.atoms().name(atom_val(result)), Some("true"));

            // `display_string(stdout, [246])` ⇒ the UTF-8 bytes `0xC3 0xB6`.
            let stdout = ctx.atom("stdout").expect("seeded");
            let umlaut = ctx.list(&[246].map(|c| ctx.int(c).expect("fits"))).expect("fits");
            display_string_2(&mut ctx, &[stdout, umlaut]).expect("writable");

            // `stderr` has no seam: a typed error, never a write to the wrong stream.
            let stderr = ctx.atom("stderr").expect("seeded");
            assert!(display_string_2(&mut ctx, &[stderr, NIL]).is_err());
        }
        assert_eq!(sink.contents(), b"<<\"a b\">>\r\n\xC3\xB6".to_vec());
    }

    /// The `display/1` latin-1 deviation, pinned exactly: `erlang:display("ström")`
    /// (the list `[115,116,114,246,109]`) writes UTF-8 `C3 B6` where the oracle's
    /// `%T` renderer writes the raw latin-1 `F6` (see the `display_1` doc). The rest
    /// of the line (`"str` … `m"` + `\r\n`) is byte-identical to the oracle, whose
    /// bytes are `22 73 74 72 F6 6D 22 0D 0A`.
    #[test]
    fn display_writes_a_printable_latin1_char_as_utf8() {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = atoms();
        let mut sink = crate::context::BytesSink::new();
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_output(&mut sink);
            let chars: Vec<Eterm> = [115, 116, 114, 246, 109]
                .into_iter()
                .map(|c| ctx.int(c).expect("fits"))
                .collect();
            let list = ctx.list(&chars).expect("fits");
            let result = display_1(&mut ctx, &[list]).expect("renderable");
            assert_eq!(ctx.atoms().name(atom_val(result)), Some("true"));
        }
        // `"str` + UTF-8 `C3 B6` + `m"` + `\r\n` (the oracle: `22 73 74 72 F6 6D 22 0D 0A`).
        assert_eq!(sink.contents(), b"\"str\xC3\xB6m\"\r\n".to_vec());
    }
}
