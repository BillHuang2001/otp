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

//! `io:format/1,2`, `io_lib:format/2,3` and the shared format-directive engine.
//!
//! Implements the Milestone-1 `io`/`io_lib` subset (compatibility contract Part I
//! §A.7). `Format` is a flat proper list of byte-sized small integers (`io_lib`
//! also accepts the same shape — the loader hands us bytes), `Args` a proper list.
//!
//! * `io:format/1,2` write the rendered text to the context's [`OutputSink`]
//!   ([`crate::context::BifContext::write_output`]) and return the atom `ok`.
//! * `io_lib:format/2,3` return the rendering as the oracle's **deep list** and
//!   write nothing: literal format characters and the "small" controls (`~s` of an
//!   atom, `~e`/`~f`/`~g`, `~b`/`~B`, `~x`/`~X`, `~c`, `~~`, `~n`, `~i`) are
//!   spliced flat as individual small integers, while the "non-small" controls
//!   (`~w`, `~p`, `~W`, `~P` and `~s` of a list/binary) each contribute a single
//!   **sub-list** element (the exact nesting `io_lib`/`io_lib_pretty` build — see
//!   [`crate::print::Chunk`]). So `io_lib:format("~w", [42])` is `[[52,50]]`,
//!   `io_lib:format("a~wb~n", [42])` is `[97,[52,50],98,10]` and
//!   `io_lib:format("hello", [])` is `[104,101,108,108,111]`.
//!
//! Both share one directive engine ([`render_format`]), so their text is
//! identical by construction: `io:format` flattens the chunk tree the
//! `io_lib:format` list is materialized from.
//!
//! # The supported directive set (verified against erts-17.1)
//!
//! `~w` (write), `~p` (print — the printable-string rule, see [`crate::print`]),
//! `~W`/`~P` (`~w`/`~p` with an explicit *depth* argument), `~s` (string: an atom's
//! name, a proper list of byte-sized small integers, or a **binary**), `~c` (a
//! character: `~c` takes the integer's low byte, `~tc` the full code point,
//! negative, surrogates and bignums included — see below), `~n` (newline), `~~` (a
//! literal `~`), `~i` (consume and ignore one argument), `~b`/`~B` (an integer in
//! base 10, or `~.<base>b`/`~.<base>B` for base 2–36), `~x`/`~X` (an integer in
//! `.<base>` with a **two-argument** `(Integer, Prefix)` shape), `~+`/`~#` (an
//! integer in `.<base>`, default 10, carrying the `Base#` **prefix** its own base
//! implies — lowercase digits for `~+`, uppercase for `~#`), and the float
//! directives `~e`/`~f`/`~g`.
//!
//! # `~x`/`~X`: the prefix, verbatim
//!
//! The `~x`/`~X` prefix is an atom's name or a **deep** character list
//! (`io_lib:deep_char_list/1`): an integer element is emitted **verbatim** — it is
//! not restricted to a byte, so `io_lib:format("~.16x",[255,[256]])` ⇒
//! `[256,102,102]`. Only integers `0 =< C < 16#D800`, `16#DFFF < C < 16#FFFE` or
//! `16#FFFF < C =< 16#10FFFF` are accepted (nested lists are walked); anything else
//! — a negative, a surrogate, `16#FFFE`/`16#FFFF`, a value past `16#10FFFF`, a
//! binary, an improper list, a non-list/non-atom prefix — is `badarg`. Each prefix
//! integer counts as **one** character for the field width / truncation
//! (`io_lib:chars_length/1`).
//!
//! # `~c`/`~tc`: the code point, verbatim
//!
//! `io_lib_format:control_small($c,…)` has two clauses: `~tc` renders the integer
//! **verbatim** (`char(A, …)` is `[A]`) and `~c` renders only its low byte
//! (`char(A band 255, …)`). So `io_lib:format("~tc",[1114112])` ⇒ `[1114112]`,
//! `~tc` of the surrogate `55296` ⇒ `[55296]`, `~tc` of `-1` ⇒ `[-1]`, `~tc` of a
//! bignum ⇒ that bignum, and `~c` of `257`/`-1`/`1 bsl 100` ⇒ `[1]`/`[255]`/`[0]`
//! — all reproduced here. Such a code point is carried as a [`crate::print::Chunk::Int`]
//! and `io:format` raises `badarg` for one the oracle's unicode device refuses
//! (`io:format("~tc",[1114112])` ⇒ `error:badarg`), while a valid scalar is written
//! as its UTF-8 encoding (`io:format("~tc",[16#10FFFF])` ⇒ `F4 8F BF BF`).
//!
//! # `~ts`: the unicode string (decoded, not raw bytes)
//!
//! `~t` sets the directive's encoding to `unicode`
//! (`io_lib_format:collect_cseq/2`; see [`Spec::unicode`]), which changes what
//! `~s` renders: the oracle's `control_limited($s, …, unicode, …)` runs
//! `cdata_to_chars/1` + `uniconv/1` in place of `iolist_to_chars/1`. The
//! **plain** (unlimited, `CharsLimit < 0`) `cdata_to_chars/1` walk is reproduced
//! here ([`cdata_chunk`]):
//!
//! * a **binary** is *decoded* as UTF-8 (`unicode:characters_to_list/1`) — so
//!   `io_lib:format("~ts",[<<230,151,165>>])` ⇒ `[[26085]]`, not the raw bytes
//!   `[[230,151,165]]` — and falls back to `binary_to_list/1` (the raw bytes)
//!   when it is not valid UTF-8 (`<<200,201>>` ⇒ `[[200,201]]`);
//! * a **list** keeps its nesting: a non-integer element recurses into a
//!   sub-list, so `io_lib:format("~ts",[[<<230,151,165>>,100]])` ⇒
//!   `[[[26085],100]]`;
//! * a **non-negative integer** element is emitted verbatim (no upper bound —
//!   `400`, the surrogate `55296` and a bignum all pass);
//! * a negative integer, a float, a tuple, an improper tail, a non-byte-aligned
//!   bitstring and a bare non-list/non-binary argument are the oracle's
//!   `function_clause`, i.e. `badarg`.
//!
//! `~ts` of an **atom** is unaffected: an atom is a *small* control
//! (`control_small($s, [A], …, unicode) when is_atom(A)`) and stays
//! `string(atom_to_list(A), …)` — `io_lib:format("~ts",[abc])` ⇒ `[97,98,99]`.
//!
//! `~ts` under an active `{chars_limit, N}` (the grapheme-cluster-limited
//! `cdata_to_chars/3` walk) is **not** this algorithm and stays the typed,
//! catchable [`unsupported_bif`] — see [`render_limited_s`].
//!
//! Every integer directive — `~b`/`~B`, `~x`/`~X`, `~+`/`~#` — renders an
//! **arbitrary-precision** integer: the digits come from the shared
//! [`crate::numeric::integer_digits`] renderer (the same one `integer_to_list/2`
//! uses), so a bignum prints in full. The base is the directive's *precision*
//! (`~.<base>…`); it must be `2..=36` exactly as `io_lib_format`'s
//! `unprefixed_integer`/`prefixed_integer` guard demands, else `badarg`. The sign
//! precedes the prefix (`io_lib:format("~.16#",[-255])` ⇒ `"-16#FF"`).
//!
//! Every directive accepts the full control-sequence prefix of
//! `io_lib_format:collect_cseq/2`: an optional `-` (**left** adjust), a decimal
//! **field width** (or `*` to take it from the next argument), an optional `.`
//! **precision** (decimal or `*`), an optional `.` **pad character** (or `.*`), and
//! the `t`/`l` modifiers (`t` = unicode encoding, `l` = disable the string rule).
//! Field widths reproduce `io_lib_format:term/6` exactly: a rendering longer than
//! the effective precision P prints exactly P `*` characters padded to F, otherwise
//! it is padded to F with the pad character honouring the adjust direction.
//!
//! # What is rejected (`badarg`)
//!
//! `~d` — the OTP oracle raises `badarg` for `~d` (`io_lib:format("~d",[255])` ⇒
//! `error:badarg`); `~b` is the base-10 integer directive. Also rejected: a base
//! outside `2..=36`, a precision below the directive's minimum
//! (`~.1e`, `~.0f`, `~.0g`), a precision on a directive whose effective precision
//! is smaller than the field width when the directive forbids it, a wrong argument
//! type or count for a directive, running out of arguments, leaving arguments
//! unconsumed, a `~` at the end of the format, a `~-<n>p`/`~-p` (a left-adjusted
//! `~p`, which has no clause in `io_lib_format`), and a `~p`/`~P` of a long
//! container outside the re-flow model (see below).
//!
//! # `~p`/`~P` re-flow (`io_lib_pretty`)
//!
//! The oracle prints a container under `~p`/`~P` with `io_lib_pretty`, which
//! breaks it across lines exactly when its single-line rendering is **at least**
//! `line_length - column` characters long (`line_length` is 80 by default, or the
//! directive's field width; `column` is 1 by default, or the directive's
//! precision). Terms the oracle never breaks — `~p` of an atom, number, ref, pid,
//! fun, **printable** list or **printable** binary — are always emitted on one
//! line however long.
//!
//! This engine reproduces the break ([`crate::print::pretty_chunk`]) for a
//! **proper or improper list, a tagged or untagged tuple, and a map** each of
//! whose elements renders single-line, at unlimited depth. It wraps with a break
//! separator `,` (an improper-list tail uses `|`), indenting continuation lines by
//! one space, by `tag_len + 2` after a tuple tag, and by two after `#{`; the
//! narrowest repro is the oracle's own boundary —
//! `io_lib:format("~p", [lists:seq(1,29)])` (79 chars) returns `[1,…,28,` / ` 29]`
//! (a break after the 28th element, one-space indent), while `lists:seq(1,28)`
//! (76 chars) and `lists:seq(1,28) ++ [x]` (78 chars) stay on one line. The flat
//! condition is the oracle's `length < line_length - column`
//! (`io_lib_pretty:print/10`, erts-17.1).
//!
//! Re-flow is **not** modelled for a container holding another container (nested
//! re-flow), an element too long to fit on a line of its own, a non-printable
//! bitstring (`pp_binary` packs its own number of bytes per line), a long tuple
//! tag (`cind/7` then selects a different indent), or a depth-limited `~P`; those
//! raise the typed `error:badarg` rather than emit a line the oracle would have
//! broken differently — never a wrong string.
//!
//! # Deviations from the oracle (documented)
//!
//! * **Output is a Rust `String` (UTF-8).** Raw format bytes `>= 128` and
//!   `~s`/`~c` values `>= 128` are emitted as their UTF-8 encoding, not raw latin1
//!   bytes. The ASCII subset is byte-exact.
//! * **`~k`/`~K` select the map printing order** for the directive that follows
//!   (`io_lib_format:modifiers/3`): `~k` sets `ordered` (ascending term order)
//!   without consuming an argument, and `~K` takes the order from the next
//!   argument. The order is not validated while parsing — it is interpreted only
//!   when a map is actually rendered (`maps:iterator(Map, Order)`): the atoms
//!   `ordered`/`reversed`/`undefined` select a built-in order, and a 2-arity fun
//!   is the order *comparator* the oracle would call — an honest, typed
//!   [`unsupported_bif`] here, since the printer has no `FunCaller` seam to invoke
//!   it. Any other order term is `error:badarg`, raised only when a map is
//!   reached (so `~Kp [M, ordered]` — no map — prints `ordered` with no error).
//! * **`~t` is the unicode modifier.** (The objective's wording calls `~t` a
//!   column/table directive; in erts-17.1 `~t` sets the encoding and `~tc`/`~ts`
//!   are the unicode forms — reproduced here. There is no column directive.)
//! * **Precision is the base for `~b`/`~B`/`~x`/`~X`/`~+`/`~#`** (as in the
//!   oracle); it is a maximum length for `~w`/`~W` and an initial column for
//!   `~p`/`~P`.
//! * **`io_lib:format/3` ignores unknown options but not `chars_limit`.** The only
//!   option erts-17.1 recognises is `{chars_limit, N}`; other elements are ignored
//!   (verified against the oracle). A non-integer `N` is `badarg`, and a non-list
//!   `Options` is `badarg`, exactly as in the oracle; a negative `chars_limit`
//!   (the oracle's default, i.e. no limit) behaves like `format/2`.
//! * **A *limiting* `chars_limit` (`N >= 0`) is reproduced** (`io_lib_format`'s
//!   two-pass `build/2`: `build_small/2` then `count_small/1` then
//!   `build_limited/5`). Literal format text is never limited, but each "big"
//!   control (`~w`/`~W`/`~p`/`~P`) is rendered at a per-directive budget and
//!   elided with `"..."`, so `io_lib:format("~w", [[1,2,3]], [{chars_limit,3}])`
//!   is `[[91,"...",93]]` and
//!   `io_lib:format("hello ~w world", [[1..10]], [{chars_limit,10}])` elides the
//!   middle, while `{chars_limit,100}`/`{chars_limit,-1}` are the normal deep
//!   list. Every *small* control (`~s` of an atom, `~e`/`~f`/`~g`, `~b`/`~B`,
//!   `~x`/`~X`, `~c`, `~~`, `~n`, `~i`) is covered — its characters count toward
//!   `Other` and the running column.
//! * **`~s` of a *list* or *binary* under an active limit is covered too**
//!   ([`render_limited_s`], the oracle's `control_limited($s, …)`): the iolist is
//!   limited to the per-directive budget by `iolist_to_chars/3` (eliding with
//!   `"..."`) and `string/6` then applies the directive's field width / precision
//!   to the *limited* text, so
//!   `io_lib:format("~s", ["hello world!!"], [{chars_limit,10}])` is
//!   `[[104,101,108,108,111,32,119,46,46,46]]` ("hello w..."). The `unicode`
//!   (`~ts`) arm — `cdata_to_chars/3` + `uniconv/1`, which is grapheme-based and
//!   flattens the iolist differently — **stays a typed, catchable
//!   `unsupported_bif`** (see [`render_limited_s`]), never a wrong rendering.

use beam_codec::term::big_parts;
use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access::{
    car, cdr, float_value, is_atom, is_big, is_float, is_list, is_nil, is_small, is_tuple,
    tuple_element, tuple_size,
};
use crate::arith::int_from_mag;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::numeric::integer_digits;
use crate::print::{
    adjust_chunk, dup_chars, dup_element, field_chunk, flatten_output, pretty_chunk,
    pretty_chunk_limited, pretty_chunk_order, pretty_write_limited, proper_list,
    string_field_chunk, term_chunk_order, tsub, Chunk, MapOrder, UNLIMITED_DEPTH,
};
use crate::registry::BifRegistry;

/// Registers `io:printable_range/0`, `io:format/1,2`, `io_lib:format/2,3` and
/// `io_lib:printable_list/1`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("io", "format", 1, format_1);
    reg.register("io", "format", 2, format_2);
    reg.register("io", "printable_range", 0, printable_range_0);
    reg.register("io_lib", "format", 2, format_lib_2);
    reg.register("io_lib", "format", 3, format_lib_3);
    reg.register("io_lib", "printable_list", 1, printable_list_1);
}

/// `io:printable_range/0` — the atom `latin1` or `unicode`, whichever the runtime
/// was started with.
///
/// Reproduces the oracle's `io_printable_range_0` (`../../erts/emulator/beam/
/// erl_unicode.c:2943-2948`), which returns `unicode` only when the printable
/// character range is [`ERL_PRINTABLE_CHARACTERS_UNICODE`]. That range defaults to
/// **`latin1`** and is upgraded to `unicode` only by the `+pc unicode` emulator
/// flag (`erl_init.c:1379-1382`); Milestone-1 has no flag plumbing, so this returns
/// the default `latin1` — exactly what the oracle reports without `+pc`
/// (`erl -noshell -eval 'io:printable_range()'` ⇒ `latin1`).
fn printable_range_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    // `latin1` is *produced* here, so it is interned on demand: the name is not
    // guaranteed to be in the runtime's table.
    ctx.intern_atom("latin1")
}

/// `io_lib:printable_list/1` — `true` iff the argument is a proper list of
/// printable **latin1** characters, else `false`.
///
/// Reproduces the pure `io_lib:printable_list/1`
/// (`../../lib/stdlib/src/io_lib.erl:1602-1610`), which dispatches on
/// `io:printable_range/0`. This runtime always reports `latin1`
/// ([`printable_range_0`]), so only the `latin1` path exists:
/// ```text
/// printable_latin1_list([C|Cs]) when is_integer(C) ->
///     printable_character(C, latin1) andalso printable_latin1_list(Cs);
/// printable_latin1_list([]) -> true;
/// printable_latin1_list(_)  -> false.
/// ```
/// and `printable_character(C, latin1)` is `16#20..=16#7E`, `16#A0..=16#FF`, or one
/// of `\t \n \r \v \b \f \e` (`\x09 \x0A \x0D \x0B \x08 \x0C \x1B`,
/// `io_lib.erl:1514-1523`). A non-list, an improper list and a non-integer element
/// are all `false`. Pinned against the oracle: `io_lib:printable_list([])` ⇒ `true`,
/// `io_lib:printable_list([1,2,3])` ⇒ `false`, `io_lib:printable_list("abc")` ⇒ `true`,
/// `io_lib:printable_list([127])` ⇒ `false`, `io_lib:printable_list([160])` ⇒ `true`.
fn printable_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let printable = match proper_list(args[0]) {
        Some(elems) => elems.iter().all(|&elem| is_printable_latin1(elem)),
        None => false,
    };
    ctx.boolean(printable)
}

/// `io_lib:printable_character/2` with `latin1` (`io_lib.erl:1514-1523`).
///
/// A bignum is out of every printable range, so only a small integer can be printable.
fn is_printable_latin1(term: Eterm) -> bool {
    if !is_small(term) {
        return false;
    }
    let c = signed_value(term);
    (0x20..=0x7E).contains(&c)
        || (0xA0..=0xFF).contains(&c)
        || matches!(c, 0x09 | 0x0A | 0x0D | 0x0B | 0x08 | 0x0C | 0x1B)
}

/// `io:format(Format)` — writes `Format` with no arguments and returns `ok`.
fn format_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_format(ctx, args[0], None)
}

/// `io:format(Format, Args)` — writes `Format` with `Args` and returns `ok`.
fn format_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_format(ctx, args[0], Some(args[1]))
}

/// `io_lib:format(Format, Args)` — returns the rendering as an iolist.
fn format_lib_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    io_lib_format(ctx, args[0], Some(args[1]), -1)
}

/// `io_lib:format(Format, Args, Options)` — the rendering as an iolist, with the
/// oracle's `format_options()` (`[{chars_limit, N}]`).
///
/// The `{chars_limit, N}` option is honoured (see [`check_options`]): a
/// non-negative `N` returns the oracle's *limited* deep list, a negative one the
/// normal unlimited rendering. The only control still not covered under a
/// limiting `chars_limit` is `~s` of a list/binary in the **unicode** (`~ts`)
/// encoding — the typed, catchable `unsupported_bif` gap (see
/// [`render_limited_s`]). The gap is a plain `BeamError::Raise` (reason atom
/// `unsupported_bif`), so interpreted/compiled Erlang code can
/// `try … catch error:unsupported_bif -> …` it; it never aborts the runtime. (The
/// CLI's `-eval` entry point reports every capability gap as an unrecoverable
/// `Flow::Fatal` by design — a `beam_rt` policy, not a property of this BIF.)
fn format_lib_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let chars_limit = check_options(ctx, args[2])?;
    io_lib_format(ctx, args[0], Some(args[1]), chars_limit)
}

/// `io_lib:format/3`'s `Options` list (`io_lib_format:get_option/3` over
/// `lists:keyfind/3`): the effective `chars_limit`, `-1` (the oracle's default,
/// i.e. no limit) when absent.
///
/// `Options` must be a proper list (`badarg` otherwise, as in the oracle).
/// Elements other than `{chars_limit, N}` are ignored — the oracle only reads that
/// one key — and a negative `N` is the oracle's default (no limit). A
/// `chars_limit` whose value is not a small integer is `badarg`.
fn check_options(ctx: &BifContext<'_>, options: Eterm) -> Result<i64> {
    let mut chars_limit = -1i64;
    for option in proper_list(options).ok_or_else(badarg)? {
        if let Some(limit) = chars_limit_option(ctx, option)? {
            chars_limit = limit;
        }
    }
    Ok(chars_limit)
}

/// The `N` of a `{chars_limit, N}` element, `None` for any other element (which
/// the oracle ignores). A `chars_limit` whose value is not a small integer is
/// `badarg` (the oracle's `chars_limit` is an `integer()`).
fn chars_limit_option(ctx: &BifContext<'_>, option: Eterm) -> Result<Option<i64>> {
    if !is_tuple(option) || tuple_size(option) != 2 {
        return Ok(None);
    }
    let tag = tuple_element(option, 0);
    if !is_atom(tag) || ctx.atoms().name(atom_val(tag)) != Some("chars_limit") {
        return Ok(None);
    }
    let value = tuple_element(option, 1);
    if !is_small(value) {
        return Err(badarg());
    }
    Ok(Some(signed_value(value)))
}

/// `io:format`: render, write the result, return `ok`.
fn io_format(ctx: &mut BifContext<'_>, format: Eterm, args: Option<Eterm>) -> Result<Eterm> {
    let chunks = render_format_of(ctx, format, args, -1)?;
    ctx.write_output(&flatten_output(&chunks)?)?;
    ctx.ok()
}

/// `io_lib:format`: render and return the oracle's **deep list**.
///
/// The list is *not* flat: each literal format character and each "small" control
/// contributes its own element, while every other control contributes a single
/// sub-list element (see [`render_format`]). `chars_limit` is
/// `io_lib:format/3`'s `{chars_limit, N}` (`-1` for `/2`, i.e. unlimited).
fn io_lib_format(
    ctx: &mut BifContext<'_>,
    format: Eterm,
    args: Option<Eterm>,
    chars_limit: i64,
) -> Result<Eterm> {
    let chunks = render_format_of(ctx, format, args, chars_limit)?;
    chunks_to_term(ctx, &chunks)
}

/// Decodes the format/argument terms and renders them to a chunk list.
fn render_format_of(
    ctx: &mut BifContext<'_>,
    format: Eterm,
    args: Option<Eterm>,
    chars_limit: i64,
) -> Result<Vec<Chunk>> {
    let bytes = format_bytes(format)?;
    let arg_terms = match args {
        Some(term) => proper_list(term).ok_or_else(badarg)?,
        None => Vec::new(),
    };
    let mut chunks = Vec::new();
    render_format(ctx, &bytes, &arg_terms, &mut chunks, chars_limit)?;
    Ok(chunks)
}

/// Decodes the format term: a flat proper list of byte-sized small integers.
fn format_bytes(format: Eterm) -> Result<Vec<u8>> {
    let elems = proper_list(format).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in elems {
        if !is_small(elem) {
            return Err(badarg());
        }
        let value = signed_value(elem);
        if !(0..=255).contains(&value) {
            return Err(badarg());
        }
        bytes.push(value as u8);
    }
    Ok(bytes)
}

/// The control-sequence prefix shared by every directive
/// (`io_lib_format:collect_cseq/2`).
#[derive(Clone, Copy)]
struct Spec {
    /// Field width; `None` when the format gave none.
    width: Option<usize>,
    /// `~-…` left justify.
    left: bool,
    /// `.` precision (never negative).
    precision: Option<usize>,
    /// Pad character (`.`-form), default space.
    pad: u8,
    /// `~t` — unicode encoding.
    unicode: bool,
    /// `~l` — disable the `~p` string rule.
    strings: bool,
    /// `~k`/`~K` — the map printing order for this directive (`ordered` for `~k`,
    /// the next argument for `~K`); the default storage order otherwise.
    maps_order: MapOrder,
}

impl Spec {
    fn plain() -> Self {
        Spec {
            width: None,
            left: false,
            precision: None,
            pad: b' ',
            unicode: false,
            strings: true,
            maps_order: MapOrder::Storage,
        }
    }
}

/// Renders `bytes` against `args` into `out` — the oracle's
/// `io_lib_format:build_small/2` + `build_limited/5`.
///
/// * a literal format character becomes **one element**;
/// * a *small* control (`io_lib_format:control_small/7`: `~s` of an **atom**,
///   `~e`/`~f`/`~g`, `~b`/`~B`, `~x`/`~X`, `~c`, `~~`, `~n`, `~i`) has its result
///   spliced **flat**, one character per element (`lists:flatten(S)`);
/// * any other control (`~w`, `~p`, `~W`, `~P`, `~s` of a list/binary) contributes
///   its result as a **single sub-list element** (`[S | …]`).
///
/// `io:format` flattens `out` to text; `io_lib:format` materializes it as the deep
/// list the oracle returns.
///
/// `chars_limit` is `io_lib:format/3`'s `{chars_limit, N}`: a **negative** value
/// (the oracle's default) is the plain unlimited rendering
/// ([`render_format_unlimited`]); a non-negative one runs the oracle's two-pass
/// limited build ([`render_format_limited`]).
fn render_format(
    ctx: &mut BifContext<'_>,
    bytes: &[u8],
    args: &[Eterm],
    out: &mut Vec<Chunk>,
    chars_limit: i64,
) -> Result<()> {
    if chars_limit < 0 {
        return render_format_unlimited(ctx, bytes, args, out);
    }
    render_format_limited(ctx, bytes, args, out, chars_limit)
}

/// The unlimited rendering — the single pass
/// `io_lib_format:build_small/2` runs when `CharsLimit < 0`.
fn render_format_unlimited(
    ctx: &mut BifContext<'_>,
    bytes: &[u8],
    args: &[Eterm],
    out: &mut Vec<Chunk>,
) -> Result<()> {
    let mut i = 0usize;
    let mut next_arg = 0usize;
    // The running indentation `I` of `io_lib_format:build_limited/5`: advanced by one
    // per literal character, reset at `\n`, rounded up to the next tab stop at `\t`,
    // and set to the column a `~…` control's own output ends at (`indentation/2`).
    // `~p`/`~P` use `I + 1` as their initial column when no precision is given.
    let mut column = 0i64;

    while i < bytes.len() {
        let byte = bytes[i];
        if byte != b'~' {
            out.push(Chunk::Char(byte as char));
            column = indent_char(byte as char, column);
            i += 1;
            continue;
        }
        i += 1;
        let (spec, ctrl) = parse_cseq(bytes, &mut i, args, &mut next_arg)?;
        let piece = dispatch(ctx, ctrl, &spec, args, &mut next_arg, column)?;
        column = indentation_of(&piece.chunk, column);
        if piece.small {
            splice_flat(out, piece.chunk);
        } else {
            out.push(piece.chunk);
        }
    }

    if next_arg != args.len() {
        return Err(badarg());
    }
    Ok(())
}

/// One element of `io_lib_format:build_small/2`'s result `Res1`.
enum BuildItem {
    /// A literal format character, or a *small* control's characters
    /// (`lists:flatten/1`-ed, so they count one character each toward `Other`).
    Flat(Chunk),
    /// A "big" control, kept as its captured directive state for pass 2
    /// (`io_lib_format:build_limited/5`).
    Big(BigControl),
}

/// A "big" control as pass 1 captured it (`~w`/`~W`/`~p`/`~P` and `~s` of a
/// list/binary — the `not_small` arms of the oracle's `control_small/7`).
struct BigControl {
    /// The control character.
    ctrl: u8,
    /// The parsed control-sequence prefix (width/precision/order…).
    spec: Spec,
    /// The term argument.
    term: Eterm,
    /// The `~W`/`~P` depth argument (`UNLIMITED_DEPTH` for `~w`/`~p`/`~s`).
    depth: i64,
}

/// `io_lib_format:build_limited/5` — the oracle's limited rendering
/// (`CharsLimit >= 0`).
///
/// Pass 1 is [`render_format_unlimited`]'s walk with the big controls *captured*
/// instead of rendered (`build_small/2`: a literal and every *small* control
/// become flat characters, `~w`/`~W`/`~p`/`~P`/`~s`-of-a-list-or-binary stay a
/// control). `count_small/1`
/// then sums the flat characters into `Other` and counts the big controls into
/// `P` (`~p`/`~P`) and `W` (`~w`/`~W`/`~s` — the oracle's own quirk keeps its `S`
/// counter at `0`). Pass 2 walks the captured items again with the per-directive
/// budget `MaxLen div Count` and charges each rendering's
/// [`Chunk::flat_len`] (`io_lib:chars_length/1`) back against the budget.
fn render_format_limited(
    ctx: &mut BifContext<'_>,
    bytes: &[u8],
    args: &[Eterm],
    out: &mut Vec<Chunk>,
    chars_limit: i64,
) -> Result<()> {
    // ---- pass 1: build_small/2 (with the big controls captured) --------------
    let mut items: Vec<BuildItem> = Vec::new();
    let mut i = 0usize;
    let mut next_arg = 0usize;
    let mut column = 0i64;

    while i < bytes.len() {
        let byte = bytes[i];
        if byte != b'~' {
            items.push(BuildItem::Flat(Chunk::Char(byte as char)));
            column = indent_char(byte as char, column);
            i += 1;
            continue;
        }
        i += 1;
        let (spec, ctrl) = parse_cseq(bytes, &mut i, args, &mut next_arg)?;
        match ctrl {
            b'w' | b'p' => {
                let term = take(args, &mut next_arg)?;
                items.push(BuildItem::Big(BigControl {
                    ctrl,
                    spec,
                    term,
                    depth: UNLIMITED_DEPTH,
                }));
            }
            b'W' | b'P' => {
                let term = take(args, &mut next_arg)?;
                let depth = take_depth(args, &mut next_arg)?;
                items.push(BuildItem::Big(BigControl {
                    ctrl,
                    spec,
                    term,
                    depth,
                }));
            }
            b's' => {
                // `build_small/2` splices a **small** control's characters flat and
                // keeps everything else as a control for pass 2. `~s` is small only
                // for an **atom** (`control_small($s, [A], …) when is_atom(A)`); a
                // list, a binary or anything else stays a `not_small` control.
                let term = take(args, &mut next_arg)?;
                if is_atom(term) {
                    let piece = string_piece(ctx, term, &spec)?;
                    column = indentation_of(&piece.chunk, column);
                    items.push(BuildItem::Flat(piece.chunk));
                } else {
                    items.push(BuildItem::Big(BigControl {
                        ctrl,
                        spec,
                        term,
                        depth: UNLIMITED_DEPTH,
                    }));
                }
            }
            _ => {
                // Every remaining control is one `control_small/7` renders, so its
                // characters are spliced flat; a `not_small` piece here would be a
                // bug (only `~s` of a non-atom is `not_small`), so it stays the
                // typed gap rather than a wrong rendering.
                let piece = dispatch(ctx, ctrl, &spec, args, &mut next_arg, column)?;
                if !piece.small {
                    return Err(unsupported_bif());
                }
                column = indentation_of(&piece.chunk, column);
                items.push(BuildItem::Flat(piece.chunk));
            }
        }
    }
    if next_arg != args.len() {
        return Err(badarg());
    }

    // ---- count_small/1 --------------------------------------------------------
    let mut num_of_ps = 0i64;
    let mut num_of_ws = 0i64;
    let mut other = 0i64;
    for item in &items {
        match item {
            BuildItem::Flat(chunk) => other += chunk.flat_len() as i64,
            BuildItem::Big(big) => match big.ctrl {
                b'p' | b'P' => num_of_ps += 1,
                b'w' | b'W' | b's' => num_of_ws += 1,
                _ => {}
            },
        }
    }
    let count0 = num_of_ps + num_of_ws;
    if count0 == 0 {
        // `build/2` returns `Res1` unchanged when `P + S + W =:= 0`.
        for item in items {
            if let BuildItem::Flat(chunk) = item {
                splice_flat(out, chunk);
            }
        }
        return Ok(());
    }

    // ---- pass 2: build_limited/5 ---------------------------------------------
    let mut count = count0;
    let mut max_len = tsub(chars_limit, other);
    let mut column = 0i64;
    for item in items {
        match item {
            BuildItem::Flat(chunk) => {
                column = indentation_of(&chunk, column);
                splice_flat(out, chunk);
            }
            BuildItem::Big(big) => {
                let budget = if max_len < 0 { -1 } else { max_len / count };
                let chunk = render_limited_control(ctx, &big, budget, column)?;
                // `indentation/2` runs before `NumOfPs` is decremented, but the
                // column only advances while `~p`/`~P` remain.
                let next_column = indentation_of(&chunk, column);
                if matches!(big.ctrl, b'p' | b'P') {
                    num_of_ps -= 1;
                }
                count -= 1;
                max_len = if max_len < 0 {
                    -1
                } else {
                    tsub(max_len, chunk.flat_len() as i64)
                };
                column = if num_of_ps > 0 { next_column } else { column };
                out.push(chunk);
            }
        }
    }
    Ok(())
}

/// `io_lib_format:control_limited/3` — one big control rendered under the
/// per-directive budget `max_chars` (`< 0` = unlimited) at running column `at`.
///
/// * `~w`/`~W` — `io_lib:write(Term, Depth, Enc, Ord, MaxChars)` (the
///   [`pretty_write_limited`] entry point) followed by the directive's field
///   (`term/6`, [`term_field`]);
/// * `~p`/`~P` — `io_lib_format:print/…` under the limit ([`render_p`] with a
///   non-negative `chars_limit`);
/// * `~s` — [`render_limited_s`] (`iolist_to_chars/3` then `string/6`).
///
/// # Errors
///
/// The typed [`unsupported_bif`] for the `~s` **unicode** arm, plus whatever the
/// renderer itself reports.
fn render_limited_control(
    ctx: &mut BifContext<'_>,
    big: &BigControl,
    max_chars: i64,
    at: i64,
) -> Result<Chunk> {
    match big.ctrl {
        b'w' | b'W' => {
            let chunk = pretty_write_limited(
                ctx,
                big.term,
                false,
                big.depth,
                max_chars,
                big.spec.maps_order,
            )?;
            Ok(term_field(chunk, &big.spec))
        }
        b'p' | b'P' => render_p(
            ctx,
            big.term,
            big.spec.strings,
            big.depth,
            &big.spec,
            at,
            max_chars,
        ),
        b's' => render_limited_s(big, max_chars),
        _ => Err(unsupported_bif()),
    }
}

/// `io_lib_format:control_limited($s, …)` — `~s` of a **list or binary** under
/// the per-directive budget `chars_limit` (`io_lib_format.erl`):
///
/// ```text
/// control_limited($s, [L0], F, Adj, P, Pad, latin1, _Str, _Ord, CL, _I) ->
///     L = iolist_to_chars(L0, F, CL),
///     string(L, limit_field(F, CL), Adj, P, Pad, latin1);
/// ```
///
/// [`iolist_to_chars_limited`] limits the iolist to
/// `limit_field`'s field rule and the per-directive budget (eliding with `"..."`),
/// and [`string_field_chunk`] then applies the directive's field width / precision
/// to the *limited* text — `io_lib_format:string/6`. The result is a single
/// sub-list element of the output, exactly like the un-limited `~s`
/// ([`render_format_limited`] pushes it as one item).
///
/// The `unicode` (`~ts`) arm — `cdata_to_chars(L0, F, CL)` then
/// `uniconv(string(L, …))` — is **not** this algorithm: `limit_cdata_to_chars/3`
/// advances by `string:next_grapheme/1` (so it flattens the iolist *and* counts
/// grapheme clusters), and the oracle's `uniconv/1` may re-encode to a binary.
/// It stays the typed, catchable [`unsupported_bif`], never a wrong rendering.
fn render_limited_s(big: &BigControl, chars_limit: i64) -> Result<Chunk> {
    if big.spec.unicode {
        return Err(unsupported_bif());
    }
    let limited = iolist_to_chars_limited(big.term, big.spec.width, chars_limit)?;
    string_field_chunk(
        Chunk::List(limited),
        limit_field(big.spec.width, chars_limit),
        big.spec.left,
        big.spec.precision,
        big.spec.pad,
    )
}

/// `io_lib_format:limit_field/2` — the field width a control's rendering uses
/// under the per-directive budget `chars_limit`.
///
/// A negative budget (the unlimited path) or no width at all leaves the width
/// unchanged; otherwise the width is clamped to at least 3 characters and to the
/// budget: `max(3, min(F, CharsLimit))`. The `min(3, …)` floor is what lets the
/// elision marker `"..."` still fit.
fn limit_field(field: Option<usize>, chars_limit: i64) -> Option<usize> {
    if chars_limit < 0 {
        return field;
    }
    field.map(|f| f.min(chars_limit.max(0) as usize).max(3))
}

/// `io_lib_format:iolist_to_chars/3` — the iolist rendering of `L0` under the
/// char limit `chars_limit`, with the directive's field width `field`.
///
/// The oracle returns the *plain* [`iolist_to_chars/1`] nesting when the limit is
/// negative or at least the field width — note that a **missing** width (`none`,
/// i.e. `None` here) is an atom in Erlang, and an integer is always ordered
/// *before* an atom, so `CharsLimit >= none` is false and the limited walk runs —
/// else it runs [`limit_iolist_to_chars`] with `CharsLimit - 3` characters and the
/// `"..."` elision (the `sub/2` clamp keeps that budget non-negative).
///
/// The result is the **element list** of the produced list: each element is a
/// [`Chunk::Char`] or a nested [`Chunk::List`], so the caller can materialize the
/// oracle's exact nesting (`io_lib:format("~s", [[<<"ab">>,[$c,$d]]],
/// [{chars_limit,100}])` is `[[[97,98],99,100]]`, not a flat `[97,98,99,100]`).
fn iolist_to_chars_limited(
    term: Eterm,
    field: Option<usize>,
    chars_limit: i64,
) -> Result<Vec<Chunk>> {
    let plain = chars_limit < 0 || field.is_some_and(|f| chars_limit >= f as i64);
    if plain {
        return iolist_to_chars(term);
    }
    limit_iolist_to_chars(
        LimitSource::Term(term),
        tsub(chars_limit, 3),
        &mut Vec::new(),
        LimitMode::Normal,
    )
}

/// `io_lib_format:iolist_to_chars/1` — the **unlimited** iolist rendering.
///
/// A byte-sized small integer is one element, a nested list keeps its nesting and a
/// byte-aligned binary becomes one flat sub-list; anything else (a float, a
/// bignum, a non-byte-aligned bitstring) is the oracle's `function_clause`, i.e.
/// the observable `badarg`.
fn iolist_to_chars(term: Eterm) -> Result<Vec<Chunk>> {
    if bits::is_bitstring(term) {
        return Ok(vec![Chunk::List(binary_chars(term)?)]);
    }
    if is_list(term) {
        let head = car(term);
        let mut out = match byte_char(head) {
            Some(byte) => vec![Chunk::Char(byte)],
            None => vec![Chunk::List(iolist_to_chars(head)?)],
        };
        out.extend(iolist_to_chars(cdr(term))?);
        return Ok(out);
    }
    if is_nil(term) {
        return Ok(Vec::new());
    }
    Err(badarg())
}

/// The `Cs` of `io_lib_format:limit_iolist_to_chars/4` — the source term, or the
/// byte slice a `split_binary/2` in the recursion continues with (the split tail
/// is a raw binary the caller never materialized as a term).
enum LimitSource<'a> {
    /// A term of the source iolist: a cons, `[]` or a byte-aligned binary.
    Term(Eterm),
    /// The tail of a binary `split_binary(B, Limit)` cut off.
    Bytes(&'a [u8]),
}

/// `io_lib_format:limit_iolist_to_chars/4`'s `Mode`: `normal` still has budget to
/// spend, `final` is the 4-character `"..."` probe the `0` budget triggers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LimitMode {
    Normal,
    Final,
}

/// `io_lib_format:limit_iolist_to_chars/4` — the limited iolist walk.
///
/// * a byte-sized small integer is one element and spends one character;
/// * a nested list is walked with its tail pushed on `stack` (the oracle's `S`),
///   so an inner list's characters splice into the **outer** element list while
///   keeping their own nesting;
/// * a byte-aligned binary becomes one flat sub-list, cut at the remaining budget;
/// * a non-byte-aligned bitstring, a float, a bignum, an improper tail and any
///   other term are the oracle's `function_clause`, i.e. the observable `badarg`.
///
/// The `Normal`/`Final` mode and the `stack` reproduce the oracle's mutually
/// recursive clauses: budget `0` in `Normal` mode probes 4 more characters in
/// `Final` mode and yields `"..."` when all 4 were spent; budget `0` in `Final`
/// mode ends the probe.
fn limit_iolist_to_chars(
    source: LimitSource<'_>,
    limit: i64,
    stack: &mut Vec<Eterm>,
    mode: LimitMode,
) -> Result<Vec<Chunk>> {
    let mut source = source;
    let limit = limit;
    loop {
        if limit == 0 {
            if mode == LimitMode::Final {
                return Ok(Vec::new());
            }
            let probed = limit_iolist_to_chars(source, 4, stack, LimitMode::Final)?;
            // `case iolist_size(L) of N when N < 4 -> L; 4 -> "..." end`.
            return Ok(if chunk_list_len(&probed) < 4 {
                probed
            } else {
                vec![Chunk::Char('.'), Chunk::Char('.'), Chunk::Char('.')]
            });
        }
        match source {
            LimitSource::Bytes(bytes) => return limit_iolist_to_binary(bytes, limit, stack, mode),
            LimitSource::Term(term) => {
                if bits::is_bitstring(term) {
                    if !bits::is_binary(term) {
                        return Err(badarg());
                    }
                    let bytes = bits::bytes(term)?;
                    return limit_iolist_to_binary(&bytes, limit, stack, mode);
                }
                if is_list(term) {
                    let head = car(term);
                    if let Some(byte) = byte_char(head) {
                        let mut out = vec![Chunk::Char(byte)];
                        out.extend(limit_iolist_to_chars(
                            LimitSource::Term(cdr(term)),
                            limit - 1,
                            stack,
                            mode,
                        )?);
                        return Ok(out);
                    }
                    // `[I|Cs] -> limit(I, Limit, [Cs|S], Mode)`.
                    stack.push(cdr(term));
                    source = LimitSource::Term(head);
                    continue;
                }
                if is_nil(term) {
                    // `[]` with `S == []` ends; otherwise the stack pops.
                    return limit_iolist_to_empty(limit, stack, mode);
                }
                return Err(badarg());
            }
        }
    }
}

/// The oracle's binary clause of `limit_iolist_to_chars/4`: a binary that fits
/// the budget is one flat sub-list and spends its bytes; a longer one is cut, its
/// prefix becoming a sub-list and its tail re-walked with a `0` budget.
fn limit_iolist_to_binary(
    bytes: &[u8],
    limit: i64,
    stack: &mut Vec<Eterm>,
    mode: LimitMode,
) -> Result<Vec<Chunk>> {
    let size = bytes.len() as i64;
    let mut out = Vec::new();
    if size > limit {
        // `limit >= 0` always (the `0` probe and the `- 1` steps never go below it).
        let cut = limit.max(0) as usize;
        out.push(Chunk::List(bytes_chars(&bytes[..cut])));
        out.extend(limit_iolist_to_chars(
            LimitSource::Bytes(&bytes[cut..]),
            0,
            stack,
            mode,
        )?);
    } else {
        out.push(Chunk::List(bytes_chars(bytes)));
        out.extend(limit_iolist_to_empty(limit - size, stack, mode)?);
    }
    Ok(out)
}

/// The oracle's `[]` clauses of `limit_iolist_to_chars/4`: an empty `S` ends the
/// walk, otherwise the top continuation is popped and resumed.
fn limit_iolist_to_empty(
    limit: i64,
    stack: &mut Vec<Eterm>,
    mode: LimitMode,
) -> Result<Vec<Chunk>> {
    match stack.pop() {
        Some(tail) => limit_iolist_to_chars(LimitSource::Term(tail), limit, stack, mode),
        None => Ok(Vec::new()),
    }
}

/// The `binary_to_list/1` of a byte-aligned binary **term** — a non-byte-aligned
/// bitstring is the oracle's `function_clause`, i.e. `badarg`.
fn binary_chars(term: Eterm) -> Result<Vec<Chunk>> {
    if !bits::is_binary(term) {
        return Err(badarg());
    }
    Ok(bytes_chars(&bits::bytes(term)?))
}

/// `binary_to_list/1` of a byte slice: one [`Chunk::Char`] per byte.
fn bytes_chars(bytes: &[u8]) -> Vec<Chunk> {
    bytes.iter().map(|&b| Chunk::Char(b as char)).collect()
}

/// `iolist_size/1` of an element list — the elements are only [`Chunk::Char`]s and
/// nested lists of them, so the flat character count *is* the byte count.
fn chunk_list_len(items: &[Chunk]) -> usize {
    items.iter().map(Chunk::flat_len).sum()
}

/// The `char` a byte-sized small integer denotes, `None` for anything else (a
/// bignum, a float, a list, a binary, …) — the oracle's `0 =< C =< 255` guard.
fn byte_char(term: Eterm) -> Option<char> {
    if !is_small(term) {
        return None;
    }
    let value = signed_value(term);
    if (0..=255).contains(&value) {
        Some(value as u8 as char)
    } else {
        None
    }
}

/// One directive's result: the chunk tree plus whether it is a *small* control.
struct Piece {
    /// The rendering (`io_lib_format:control_small`'s chars).
    chunk: Chunk,
    /// Whether the control is "small" — its characters are spliced flat into the
    /// outer list rather than inserted as a single sub-list element.
    small: bool,
}

impl Piece {
    /// A control whose characters are spliced flat into the outer list.
    fn small(chunk: Chunk) -> Self {
        Piece { chunk, small: true }
    }

    /// A control whose rendering is one sub-list element.
    fn element(chunk: Chunk) -> Self {
        Piece { chunk, small: false }
    }
}

/// One step of `io_lib_format:indentation/2`'s running column for a single
/// character: a newline resets it and a tab advances to the next multiple of eight.
fn indent_char(c: char, column: i64) -> i64 {
    match c {
        '\n' => 0,
        '\t' => ((column + 8) / 8) * 8,
        _ => column + 1,
    }
}

/// `io_lib_format:indentation/2` — the column the rendered `chunk` ends at.
fn indentation_of(chunk: &Chunk, column: i64) -> i64 {
    match chunk {
        Chunk::Char(c) => indent_char(*c, column),
        // `indentation([C|Cs], I) when is_integer(C) -> indentation(Cs, I+1)`.
        Chunk::Int(_) => column + 1,
        Chunk::List(items) => items.iter().fold(column, |col, item| indentation_of(item, col)),
    }
}

/// Splices `chunk` into `out` as individual characters (`lists:flatten/1`).
fn splice_flat(out: &mut Vec<Chunk>, chunk: Chunk) {
    match chunk {
        Chunk::Char(c) => out.push(Chunk::Char(c)),
        Chunk::Int(term) => out.push(Chunk::Int(term)),
        Chunk::List(items) => {
            for item in items {
                splice_flat(out, item);
            }
        }
    }
}

/// Materializes the top-level chunks as the deep list `io_lib:format` returns.
fn chunks_to_term(ctx: &mut BifContext<'_>, chunks: &[Chunk]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        elems.push(chunk_to_term(ctx, chunk)?);
    }
    ctx.list(&elems)
}

/// Materializes one chunk: a character is a small integer, a raw integer is that
/// integer itself, a list a sub-list.
fn chunk_to_term(ctx: &mut BifContext<'_>, chunk: &Chunk) -> Result<Eterm> {
    match chunk {
        Chunk::Char(c) => ctx.int(i64::from(*c as u32)),
        Chunk::Int(term) => int_element_term(ctx, *term),
        Chunk::List(items) => {
            let mut elems = Vec::with_capacity(items.len());
            for item in items {
                elems.push(chunk_to_term(ctx, item)?);
            }
            ctx.list(&elems)
        }
    }
}

/// Materializes a raw integer element on the context heap: the small integer
/// itself, or the same **bignum** (`~tc` of `1 bsl 100` returns the bignum
/// verbatim — the oracle's `char(A, …)` is `[A]`).
///
/// The limbs are borrowed from the term's own storage, so they are copied out
/// before anything is allocated on the context heap.
fn int_element_term(ctx: &mut BifContext<'_>, term: Eterm) -> Result<Eterm> {
    if is_small(term) {
        return ctx.int(signed_value(term));
    }
    let (negative, limbs) = big_parts(term)?;
    let limbs = limbs.to_vec();
    int_from_mag(ctx, negative, &limbs)
}

/// Parses one `~…` control sequence prefix, returning the [`Spec`] and the control
/// character; `*` forms consume their argument from `args`.
fn parse_cseq(
    bytes: &[u8],
    i: &mut usize,
    args: &[Eterm],
    next_arg: &mut usize,
) -> Result<(Spec, u8)> {
    let mut spec = Spec::plain();

    // 1. field width: optional `-`, then digits or `*`.
    let negated = bytes.get(*i) == Some(&b'-');
    if negated {
        *i += 1;
    }
    let fv = field_value(bytes, i, args, next_arg)?;
    spec.width = match fv {
        None => {
            if negated {
                // `field_width` negates the value; there is none to negate.
                return Err(badarg());
            }
            None
        }
        Some(f) => {
            let f = if negated { -f } else { f };
            if f < 0 {
                spec.left = true;
                Some(f.unsigned_abs() as usize)
            } else {
                Some(f as usize)
            }
        }
    };

    // 2. precision: optional `.`, then digits or `*`.
    if bytes.get(*i) == Some(&b'.') {
        *i += 1;
        if let Some(p) = field_value(bytes, i, args, next_arg)? {
            if p < 0 {
                return Err(badarg());
            }
            spec.precision = Some(p as usize);
        }
    }

    // 3. pad character: optional `.`, then `*` (from an argument) or one byte.
    if bytes.get(*i) == Some(&b'.') {
        *i += 1;
        if bytes.get(*i) == Some(&b'*') {
            *i += 1;
            let arg = take(args, next_arg)?;
            if !is_small(arg) {
                return Err(badarg());
            }
            let value = signed_value(arg);
            if !(0..=255).contains(&value) {
                return Err(badarg());
            }
            spec.pad = value as u8;
        } else if let Some(&pad) = bytes.get(*i) {
            spec.pad = pad;
            *i += 1;
        } else {
            return Err(badarg());
        }
    }

    // 4. modifiers.
    loop {
        match bytes.get(*i) {
            Some(b't') => {
                spec.unicode = true;
                *i += 1;
            }
            Some(b'l') => {
                spec.strings = false;
                *i += 1;
            }
            // `~k` selects the ascending (`ordered`) map printing order without
            // consuming an argument; `~K` takes the order from the next argument
            // (`io_lib_format:modifiers/3`). Neither is validated here — the order
            // is interpreted only when a map is rendered (see
            // [`crate::print::resolve_map_pairs`]).
            Some(b'k') => {
                spec.maps_order = MapOrder::Ordered;
                *i += 1;
            }
            Some(b'K') => {
                let order = take(args, next_arg)?;
                spec.maps_order = MapOrder::Explicit(order);
                *i += 1;
            }
            _ => break,
        }
    }

    // 5. the control character.
    let ctrl = *bytes.get(*i).ok_or_else(badarg)?;
    *i += 1;
    Ok((spec, ctrl))
}

/// `io_lib_format:field_value/2`: a decimal run, or `*` (taking an integer
/// argument), or absent.
fn field_value(
    bytes: &[u8],
    i: &mut usize,
    args: &[Eterm],
    next_arg: &mut usize,
) -> Result<Option<i64>> {
    if bytes.get(*i) == Some(&b'*') {
        *i += 1;
        let arg = take(args, next_arg)?;
        if !is_small(arg) {
            return Err(badarg());
        }
        return Ok(Some(signed_value(arg)));
    }
    let start = *i;
    let mut value: i64 = 0;
    while let Some(&byte) = bytes.get(*i) {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'));
        *i += 1;
    }
    if *i == start {
        Ok(None)
    } else {
        Ok(Some(value))
    }
}

/// Takes the next argument, or `badarg` when the list is exhausted.
fn take(args: &[Eterm], next: &mut usize) -> Result<Eterm> {
    let arg = args.get(*next).copied().ok_or_else(badarg)?;
    *next += 1;
    Ok(arg)
}

/// Dispatches one control character (`io_lib_format:control_small` /
/// `control_limited`), reporting whether it is a "small" control.
#[allow(clippy::too_many_arguments)]
fn dispatch(
    ctx: &mut BifContext<'_>,
    ctrl: u8,
    spec: &Spec,
    args: &[Eterm],
    next_arg: &mut usize,
    column: i64,
) -> Result<Piece> {
    match ctrl {
        b'~' => Ok(Piece::small(char_chunk('~', spec)?)),
        b'n' => Ok(Piece::small(newline_chunk(spec)?)),
        b'i' => {
            let _ = take(args, next_arg)?;
            Ok(Piece::small(Chunk::List(Vec::new())))
        }
        b'w' => {
            let arg = take(args, next_arg)?;
            let chunk = term_chunk_order(ctx, arg, false, UNLIMITED_DEPTH, spec.maps_order)?;
            Ok(Piece::element(term_field(chunk, spec)))
        }
        b'p' => {
            let arg = take(args, next_arg)?;
            Ok(Piece::element(render_p(
                ctx,
                arg,
                spec.strings,
                UNLIMITED_DEPTH,
                spec,
                column,
                -1,
            )?))
        }
        b'W' => {
            let arg = take(args, next_arg)?;
            let depth = take_depth(args, next_arg)?;
            let chunk = term_chunk_order(ctx, arg, false, depth, spec.maps_order)?;
            Ok(Piece::element(term_field(chunk, spec)))
        }
        b'P' => {
            let arg = take(args, next_arg)?;
            let depth = take_depth(args, next_arg)?;
            Ok(Piece::element(render_p(
                ctx,
                arg,
                spec.strings,
                depth,
                spec,
                column,
                -1,
            )?))
        }
        b's' => {
            let arg = take(args, next_arg)?;
            string_piece(ctx, arg, spec)
        }
        b'c' => {
            let arg = take(args, next_arg)?;
            let elem = char_element(arg, spec.unicode)?;
            Ok(Piece::small(char_elem_chunk(elem, spec)?))
        }
        b'b' | b'B' => {
            let arg = take(args, next_arg)?;
            let base = spec.precision.unwrap_or(10);
            Ok(Piece::small(radix_chunk(
                arg,
                base,
                Chunk::List(Vec::new()),
                ctrl == b'B',
                spec,
            )?))
        }
        b'x' | b'X' => {
            let arg = take(args, next_arg)?;
            let prefix = prefix_arg(ctx, take(args, next_arg)?)?;
            let base = spec.precision.unwrap_or(10);
            Ok(Piece::small(radix_chunk(
                arg,
                base,
                prefix,
                ctrl == b'X',
                spec,
            )?))
        }
        // `~+`/`~#`: the integer with the `Base#` prefix its own base implies
        // (`io_lib_format:control_small($+…)`/`($#…)`), lowercase for `~+`.
        b'+' | b'#' => {
            let arg = take(args, next_arg)?;
            let base = spec.precision.unwrap_or(10);
            Ok(Piece::small(radix_chunk(
                arg,
                base,
                radix_prefix(base),
                ctrl == b'#',
                spec,
            )?))
        }
        b'e' => {
            let f = take_float(args, next_arg)?;
            let precision = spec.precision.unwrap_or(6);
            if precision < 2 {
                return Err(badarg());
            }
            let text = float_e(f, precision);
            Ok(Piece::small(float_field(Chunk::text(&text), spec)))
        }
        b'f' => {
            let f = take_float(args, next_arg)?;
            let precision = spec.precision.unwrap_or(6);
            if precision < 1 {
                return Err(badarg());
            }
            let text = float_f(f, precision);
            Ok(Piece::small(float_field(Chunk::text(&text), spec)))
        }
        b'g' => {
            let f = take_float(args, next_arg)?;
            let precision = spec.precision.unwrap_or(6);
            let text = float_g(f, precision)?;
            Ok(Piece::small(float_field(Chunk::text(&text), spec)))
        }
        _ => Err(badarg()),
    }
}

/// The `~W`/`~P` depth argument (a small integer; a negative depth is unlimited).
fn take_depth(args: &[Eterm], next_arg: &mut usize) -> Result<i64> {
    let arg = take(args, next_arg)?;
    if !is_small(arg) {
        return Err(badarg());
    }
    Ok(signed_value(arg))
}

/// The `~e`/`~f`/`~g` argument (a float; an integer is `badarg`, as in the oracle).
fn take_float(args: &[Eterm], next_arg: &mut usize) -> Result<f64> {
    let arg = take(args, next_arg)?;
    if !is_float(arg) {
        return Err(badarg());
    }
    Ok(float_value(arg))
}

/// `io_lib_format:prefixed_integer`'s prefix: an atom's name or a **deep**
/// character list.
///
/// The oracle's non-atom clause is guarded by
/// `true = io_lib:deep_char_list(Prefix)` and then passes `Prefix` through
/// **unmapped** (`prefixed_integer` builds `term([Prefix|S], …)`), so a prefix
/// element is emitted **verbatim** — not restricted to a byte. `deep_char_list/1`
/// (`../../lib/stdlib/src/io_lib.erl`) accepts an integer `0 =< C < 16#D800`,
/// `16#DFFF < C < 16#FFFE` or `16#FFFF < C =< 16#10FFFF`, recursing into nested
/// lists, and `false` for everything else. Each accepted integer becomes a raw
/// [`Chunk::Int`] so it is materialized as itself and counted as **one** character
/// by [`Chunk::flat_len`] (`io_lib:chars_length/1`).
///
/// # Errors
///
/// `badarg` for a non-atom/non-list prefix, an improper list, a non-integer
/// element, or an integer outside the ranges above
/// (`io_lib:format("~.16x",[255,[65535]])` ⇒ `error:badarg`).
fn prefix_arg(ctx: &BifContext<'_>, arg: Eterm) -> Result<Chunk> {
    if is_atom(arg) {
        let name = ctx.atoms().name(atom_val(arg)).ok_or_else(badarg)?;
        return Ok(Chunk::text(name));
    }
    let mut out = Vec::new();
    push_prefix_elements(arg, &mut out)?;
    Ok(Chunk::List(out))
}

/// `io_lib:deep_char_list/1` over a term, appending one [`Chunk::Int`] per accepted
/// integer element (recursing into nested lists).
fn push_prefix_elements(term: Eterm, out: &mut Vec<Chunk>) -> Result<()> {
    let elems = proper_list(term).ok_or_else(badarg)?;
    for elem in elems {
        if is_list(elem) {
            push_prefix_elements(elem, out)?;
        } else if is_deep_char(elem) {
            out.push(Chunk::Int(elem));
        } else {
            return Err(badarg());
        }
    }
    Ok(())
}

/// `io_lib:deep_char_list/1`'s per-element range test: `0 =< C < 16#D800`,
/// `16#DFFF < C < 16#FFFE` or `16#FFFF < C =< 16#10FFFF`. A bignum is never in
/// range (its magnitude is at least `2^59`).
fn is_deep_char(term: Eterm) -> bool {
    if !is_small(term) {
        return false;
    }
    let value = signed_value(term);
    (0..0xD800).contains(&value)
        || (0xE000..0xFFFE).contains(&value)
        || (0x10000..=0x10FFFF).contains(&value)
}

/// Renders the `~s` argument (`io_lib_format:control_small($s,…)` /
/// `control_limited($s,…)`): the [`string_arg`] text wrapped in the directive's
/// field rule ([`string_field_chunk`] — `io_lib_format:string/6`).
///
/// Also reports whether the control is "small" ([`string_arg`]): only `~s` of an
/// **atom** is, so pass 1 of the limited path splices it flat while a list or a
/// binary is captured for [`render_limited_s`].
fn string_piece(ctx: &BifContext<'_>, arg: Eterm, spec: &Spec) -> Result<Piece> {
    let (chunk, small) = string_arg(ctx, arg, spec.unicode)?;
    let chunk = string_field_chunk(chunk, spec.width, spec.left, spec.precision, spec.pad)?;
    Ok(Piece { chunk, small })
}

/// Renders the `~s` argument: an atom's name, or — under the `latin1` encoding —
/// a proper list of byte-sized small integers or a binary's bytes, or — under the
/// `unicode` (`~ts`) encoding — the [`cdata_chunk`] walk of
/// `io_lib_format:cdata_to_chars/1`, which *decodes* a binary as UTF-8.
///
/// Also reports whether the control is "small": only `~s` of an **atom** is
/// (`io_lib_format:control_small/7` has a clause for atoms alone — for both
/// encodings), so a list or a binary is inserted as a single sub-list element
/// while an atom's characters are spliced flat.
fn string_arg(ctx: &BifContext<'_>, arg: Eterm, unicode: bool) -> Result<(Chunk, bool)> {
    if is_atom(arg) {
        let name = ctx.atoms().name(atom_val(arg)).ok_or_else(badarg)?;
        return Ok((Chunk::text(name), true));
    }
    if unicode {
        return Ok((cdata_chunk(arg)?, false));
    }
    if bits::is_bitstring(arg) {
        if !bits::is_binary(arg) {
            // A non-byte-aligned bitstring has no byte string rendering in M1.
            return Err(badarg());
        }
        let text: String = bits::bytes(arg)?.iter().map(|&b| b as char).collect();
        return Ok((Chunk::text(&text), false));
    }
    let elems = proper_list(arg).ok_or_else(badarg)?;
    Ok((Chunk::text(&byte_string(&elems)?), false))
}

/// `io_lib_format:cdata_to_chars/1` — the **plain** (unlimited) unicode rendering
/// of a `~ts` argument, as the chunk tree the oracle's nesting implies.
///
/// ```text
/// cdata_to_chars([C|Cs]) when is_integer(C), C >= $\000 -> [C | cdata_to_chars(Cs)];
/// cdata_to_chars([I|Cs]) -> [cdata_to_chars(I) | cdata_to_chars(Cs)];
/// cdata_to_chars([]) -> [];
/// cdata_to_chars(B) when is_binary(B) ->
///     try unicode:characters_to_list(B) of L when is_list(L) -> L;
///         _ -> binary_to_list(B) catch _:_ -> binary_to_list(B) end.
/// ```
///
/// The nesting is **kept**: a non-integer element recurses into a sub-list, so
/// `~ts` of `[<<230,151,165>>,100]` is `[[26085],100]` — and the directive's own
/// result is one further sub-list element of the output list
/// (`[[[26085],100]]`).
///
/// # Errors
///
/// `badarg` (the oracle's `function_clause`) for a negative integer, a float, an
/// atom, a tuple, an improper tail, a byte-aligned-ness failure, and for the bare
/// non-list/non-binary argument itself (`~ts` of an integer or a tuple).
fn cdata_chunk(term: Eterm) -> Result<Chunk> {
    if bits::is_bitstring(term) {
        // The only binary clause is guarded by `is_binary/1`; a non-byte-aligned
        // bitstring matches no clause at all.
        if !bits::is_binary(term) {
            return Err(badarg());
        }
        return Ok(Chunk::List(cdata_binary_chars(&bits::bytes(term)?)));
    }
    let mut items = Vec::new();
    let mut cursor = term;
    loop {
        if is_nil(cursor) {
            return Ok(Chunk::List(items));
        }
        if !is_list(cursor) {
            // Neither `[]` nor a cons — a bare integer, float, tuple or atom:
            // `cdata_to_chars/1` has no clause for it.
            return Err(badarg());
        }
        items.push(cdata_element(car(cursor))?);
        cursor = cdr(cursor);
    }
}

/// One element of `cdata_to_chars/1`'s list clause.
///
/// `head` is either the non-negative integer the first clause emits verbatim, or
/// anything the second clause recurses into — `[cdata_to_chars(I) | …]` — keeping
/// `I`'s own nesting (a sub-list for a list, the decoded/fallback bytes for a
/// binary). A recursion into a negative integer, a float, a tuple or an atom is
/// the oracle's `function_clause`.
fn cdata_element(head: Eterm) -> Result<Chunk> {
    if let Some(chunk) = cdata_integer_chunk(head) {
        return Ok(chunk);
    }
    cdata_chunk(head)
}

/// The chunk for `C` when it is a non-negative integer (the oracle's first
/// `cdata_to_chars/1` clause), `None` for anything else — including a **negative**
/// integer, which the second clause recurses into and so raises.
///
/// A valid Unicode scalar becomes a [`Chunk::Char`] (so a `\n`/`\t` element keeps
/// feeding the running indentation the way `io_lib_format:indentation/2` reads it);
/// every other non-negative integer — the surrogate `55296`, `1114112`, and any
/// bignum — is carried verbatim as a [`Chunk::Int`], with no upper bound.
fn cdata_integer_chunk(term: Eterm) -> Option<Chunk> {
    if !is_nonneg_integer(term) {
        return None;
    }
    if is_small(term) {
        if let Some(c) = u32::try_from(signed_value(term)).ok().and_then(char::from_u32) {
            return Some(Chunk::Char(c));
        }
    }
    Some(Chunk::Int(term))
}

/// `is_integer(T)` **and** `T >= 0` — the oracle's
/// `cdata_to_chars([C|Cs]) when is_integer(C), C >= $\000` guard.
///
/// A bignum is never zero (`external.c` strips its leading digits), so its sign
/// decides: a negative bignum takes the oracle's second clause and raises, a
/// positive one is emitted verbatim.
fn is_nonneg_integer(term: Eterm) -> bool {
    if is_small(term) {
        return signed_value(term) >= 0;
    }
    match big_parts(term) {
        Ok((negative, _)) => !negative,
        Err(_) => false,
    }
}

/// `cdata_to_chars/1`'s binary clause: `unicode:characters_to_list(Bin)`'s code
/// points when the whole binary is valid UTF-8, else `binary_to_list(Bin)` — the
/// raw bytes.
///
/// `characters_to_list/1` returns an `{error|incomplete, _, _}` tuple for **any**
/// invalid or truncated UTF-8 — an overlong sequence, a surrogate (the CESU-8
/// `0xED 0xA0 0x80`), a code point past `16#10FFFF`, a stray continuation byte, a
/// 5/6-byte lead, or a truncated trailing sequence, which poisons the whole image
/// even when a valid prefix decoded — and the oracle's `of L when is_list(L)` then
/// falls through to `binary_to_list/1`.
fn cdata_binary_chars(bytes: &[u8]) -> Vec<Chunk> {
    match decode_utf8_strict(bytes) {
        Some(scalars) => scalars.into_iter().map(Chunk::Char).collect(),
        None => bytes.iter().map(|&b| Chunk::Char(b as char)).collect(),
    }
}

/// A strict UTF-8 decode: `Some` (the scalar values, in order) when the whole
/// slice is well-formed UTF-8, `None` for any overlong, surrogate, out-of-range,
/// stray or truncated sequence.
///
/// Reproduces the rule set `unicode:characters_to_list/1` applies (verified
/// against erts-17.1 for the overlong, surrogate, `> 16#10FFFF`, 5/6-byte-lead,
/// stray-continuation and truncated cases): the lead byte's range fixes the
/// sequence length and its minimum code point, every continuation byte must be
/// `10xxxxxx`, and surrogates are rejected.
fn decode_utf8_strict(bytes: &[u8]) -> Option<Vec<char>> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let lead = bytes[i];
        if lead < 0x80 {
            out.push(lead as char);
            i += 1;
            continue;
        }
        // `0x80..=0xC1` (continuations and the two overlong leads) and
        // `0xF5..=0xFF` (including the 5/6-byte leads) are never lead bytes.
        let (len, min) = match lead {
            0xC2..=0xDF => (2usize, 0x80u32),
            0xE0..=0xEF => (3, 0x800),
            0xF0..=0xF4 => (4, 0x1_0000),
            _ => return None,
        };
        if i + len > bytes.len() {
            return None;
        }
        let mut code = u32::from(lead & (0x7F >> len));
        for &cont in &bytes[i + 1..i + len] {
            if cont & 0xC0 != 0x80 {
                return None;
            }
            code = (code << 6) | u32::from(cont & 0x3F);
        }
        if code < min || code > 0x10FFFF || (0xD800..=0xDFFF).contains(&code) {
            return None;
        }
        out.push(char::from_u32(code)?);
        i += len;
    }
    Some(out)
}

/// Renders a proper list of byte-sized small integers as its latin1 string.
fn byte_string(elems: &[Eterm]) -> Result<String> {
    let mut out = String::with_capacity(elems.len());
    for &elem in elems {
        if !is_small(elem) {
            return Err(badarg());
        }
        let value = signed_value(elem);
        if !(0..=255).contains(&value) {
            return Err(badarg());
        }
        out.push(value as u8 as char);
    }
    Ok(out)
}

/// `io_lib_format:print/…` for `~p`/`~P`: the printable rendering, re-flowed the
/// way `io_lib_pretty` breaks a container across lines when it does not fit
/// `line_length - column` (see [`crate::print::pretty_chunk`] and the module docs
/// for the supported forms and the remaining gap).
///
/// A **non-negative** `chars_limit` selects the limited path
/// ([`pretty_chunk_limited`], `io_lib_pretty:print/10` with `T = chars_limit`),
/// which elides with `"..."` instead of re-flowing.
#[allow(clippy::too_many_arguments)]
fn render_p(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    spec: &Spec,
    at: i64,
    chars_limit: i64,
) -> Result<Chunk> {
    if spec.left {
        // A left-adjusted `~p`/`~P` has no clause in `io_lib_format`.
        return Err(badarg());
    }
    let line_length = spec.width.unwrap_or(80);
    // `io_lib_format:print/12` defaults the column to `I + 1` — the running
    // indentation at the directive — not to 1 (`io_lib_pretty:print/10`'s `Column`).
    let column = spec.precision.unwrap_or((at + 1).max(1) as usize);
    if chars_limit >= 0 {
        return pretty_chunk_limited(
            ctx,
            term,
            printable,
            depth,
            line_length,
            column,
            chars_limit,
            spec.maps_order,
        );
    }
    match spec.maps_order {
        MapOrder::Storage => pretty_chunk(ctx, term, printable, depth, line_length, column),
        order => pretty_chunk_order(ctx, term, printable, depth, line_length, column, order),
    }
}
/// `io_lib_format:term/6` — the field-width / truncation rule for a term
/// rendering (`~w`/`~W` and the integer directives).
fn term_field(chunk: Chunk, spec: &Spec) -> Chunk {
    field_chunk(chunk, spec.width, spec.left, spec.precision, spec.pad)
}

/// `fwrite_e`/`fwrite_f`/`fwrite_g` use the field width as both the width and the
/// precision (`term(_Chars, F, Adj, F, Pad, _)`).
fn float_field(chunk: Chunk, spec: &Spec) -> Chunk {
    match spec.width {
        None => chunk,
        Some(field) => field_chunk(chunk, Some(field), spec.left, Some(field), spec.pad),
    }
}

/// The single element a `~c`/`~tc` directive contributes
/// (`io_lib_format:control_small($c,…)`).
///
/// The oracle has two clauses (`io_lib_format.erl:528-531`):
///
/// * with the `t` (unicode) modifier the argument is the code point **verbatim**
///   (`char(A, …)` is `[A]`), so an integer that is not a Unicode scalar value —
///   `1114112`, a surrogate `55296`, a negative value, a bignum — is carried as a
///   [`Chunk::Int`] (the oracle's `io_lib:format` returns the raw integer;
///   `io:format` then raises `badarg`, see [`flatten_output`]);
/// * without it only the low byte survives (`char(A band 255, …)`), which is
///   always a character.
///
/// # Errors
///
/// `badarg` for a non-integer argument (a float, an atom, a list, …), exactly as
/// the oracle's `is_integer(A)` guards do: `io_lib:format("~tc",[1.0])` ⇒
/// `error:badarg`.
fn char_element(arg: Eterm, unicode: bool) -> Result<Chunk> {
    if !unicode {
        return Ok(Chunk::Char(low_byte(arg)? as char));
    }
    if is_small(arg) {
        let value = signed_value(arg);
        return Ok(match u32::try_from(value).ok().and_then(char::from_u32) {
            Some(c) => Chunk::Char(c),
            None => Chunk::Int(arg),
        });
    }
    if is_big(arg) {
        return Ok(Chunk::Int(arg));
    }
    Err(badarg())
}

/// `A band 255` for an integer term — the low byte of its two's-complement
/// representation (`-1` ⇒ `255`, `256` ⇒ `0`, `1 bsl 100` ⇒ `0`).
///
/// # Errors
///
/// `badarg` for a non-integer term.
fn low_byte(term: Eterm) -> Result<u8> {
    if is_small(term) {
        return Ok((signed_value(term) & 0xff) as u8);
    }
    if is_big(term) {
        let (negative, limbs) = big_parts(term)?;
        // A bignum's limb vector is never empty (an all-zero magnitude is the
        // small integer `0`), but a malformed one is an error, not a zero byte.
        let low = limbs.first().copied().ok_or_else(badarg)? as u8;
        return Ok(if negative { 0u8.wrapping_sub(low) } else { low });
    }
    Err(badarg())
}

/// `io_lib_format:char/5` for `~~`.
fn char_chunk(ch: char, spec: &Spec) -> Result<Chunk> {
    char_elem_chunk(Chunk::Char(ch), spec)
}

/// `io_lib_format:char/5` for `~c`/`~tc`/`~~`: the single `elem` (a [`Chunk::Char`]
/// or a raw [`Chunk::Int`]) duplicated `Field`/`Precision` times — `chars/2` is
/// `lists:duplicate/2`, so a raw integer is duplicated just like a character.
fn char_elem_chunk(elem: Chunk, spec: &Spec) -> Result<Chunk> {
    match (spec.width, spec.precision) {
        (None, None) => Ok(Chunk::List(vec![elem])),
        (Some(field), None) => Ok(dup_element(elem, field)),
        (None, Some(p)) => Ok(dup_element(elem, p)),
        (Some(field), Some(p)) if field >= p => Ok(adjust_chunk(
            dup_element(elem, p),
            dup_chars(spec.pad as char, field - p),
            spec.left,
        )),
        _ => Err(badarg()),
    }
}

/// `io_lib_format:newline/4` for `~n` (only the right-adjust clause exists).
fn newline_chunk(spec: &Spec) -> Result<Chunk> {
    match spec.width {
        None => Ok(Chunk::List(vec![Chunk::Char('\n')])),
        Some(field) if !spec.left => Ok(dup_chars('\n', field)),
        _ => Err(badarg()),
    }
}

/// `unprefixed_integer`/`prefixed_integer`: sign, prefix and `.<base>` digits.
///
/// `value` may be an **arbitrary-precision** integer (a small integer or a bignum):
/// the magnitude digits come from the shared [`crate::numeric::integer_digits`]
/// renderer — the same one `integer_to_list/2` uses, so there is only one
/// integer-to-digits implementation in the crate — and are lowercased unless the
/// directive is one of the uppercase forms. The base is the directive's precision
/// (`~.<base>…`), defaulting to 10; the oracle's guard
/// (`unprefixed_integer`/`prefixed_integer`, `io_lib_format.erl`) is
/// `2 =< Base =< 1 + $Z-$A + 10`, i.e. `2..=36`.
///
/// The sign is emitted **before** the prefix
/// (`io_lib:format("~.16#",[-255])` ⇒ `"-16#FF"`); the prefix itself is never
/// case-folded, only the digits are. `prefix` is the directive's own prefix as a
/// chunk: empty for `~b`/`~B` and the literal `Base#` for `~+`/`~#` (see
/// [`radix_prefix`]), or the caller's `(Integer, Prefix)` argument for `~x`/`~X`
/// (see [`prefix_arg`]) — which is emitted verbatim, so a raw [`Chunk::Int`]
/// prefix element survives with [`Chunk::flat_len`] counting it as one character.
fn radix_chunk(value: Eterm, base: usize, prefix: Chunk, upper: bool, spec: &Spec) -> Result<Chunk> {
    if !(2..=36).contains(&base) {
        return Err(badarg());
    }
    if !is_small(value) && !is_big(value) {
        return Err(badarg());
    }
    let digits = integer_digits(value, base as u32)?;
    let (sign, magnitude) = match digits.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, digits.as_str()),
    };
    let magnitude = if upper {
        magnitude.to_owned()
    } else {
        magnitude.to_ascii_lowercase()
    };
    let mut items = Vec::with_capacity(3);
    if sign {
        items.push(Chunk::Char('-'));
    }
    items.push(prefix);
    items.push(Chunk::text(&magnitude));
    Ok(field_chunk(
        Chunk::List(items),
        spec.width,
        spec.left,
        None,
        spec.pad,
    ))
}

/// The `Base#` prefix the `~+`/`~#` forms imply
/// (`io_lib_format:control_small($+…)`).
fn radix_prefix(base: usize) -> Chunk {
    Chunk::text(&format!("{base}#"))
}

/// `fwrite_e`/`float_e` — a float in scientific notation with `p` significands
/// after the leading digit (`p >= 2`).
fn float_e(f: f64, p: usize) -> String {
    let (digits, scale) = float_data(f);
    let (mantissa, exponent) = abs_float_e(&digits, scale, p);
    format!("{}{mantissa}{}", signbit(f), float_exp(exponent))
}

/// `abs_float_e/3`.
fn abs_float_e(digits: &[u8], scale: i64, p: usize) -> (String, i64) {
    let (mantissa, carry) = float_man3(digits, 1, p as i64 - 1);
    if carry {
        // The leading digit carried out of the number: `[$0|Fs]` becomes `1Fs`.
        let rest = mantissa.strip_prefix('0').unwrap_or(&mantissa);
        (format!("1{rest}"), scale)
    } else {
        (mantissa, scale - 1)
    }
}

/// `fwrite_f`/`float_f` — a float in fixed notation with `p` decimals (`p >= 1`).
fn float_f(f: f64, p: usize) -> String {
    let (digits, scale) = float_data(f);
    let (digits, scale) = if scale <= 0 {
        // Prepend `-E+1` zeros so the point lands after the first integer digit.
        let mut padded = vec![b'0'; (-scale + 1) as usize];
        padded.extend_from_slice(&digits);
        (padded, 1i64)
    } else {
        (digits, scale)
    };
    let (mantissa, carry) = float_man3(&digits, scale, p as i64);
    let body = if carry {
        format!("1{mantissa}")
    } else {
        mantissa
    };
    format!("{}{body}", signbit(f))
}

/// `fwrite_g/5` — `f` form for `[0.1, 1.0e4)` when it carries enough significant
/// digits, else the `e` form (precision = number of significant digits).
fn float_g(f: f64, p: usize) -> Result<String> {
    if p < 1 {
        return Err(badarg());
    }
    let magnitude = f.abs();
    if magnitude < 1.0e4 {
        let size: i64 = if magnitude < 1.0e-1 {
            -2
        } else if magnitude < 1.0 {
            -1
        } else if magnitude < 1.0e1 {
            0
        } else if magnitude < 1.0e2 {
            1
        } else if magnitude < 1.0e3 {
            2
        } else {
            3
        };
        if (p <= 1 && size == -1) || ((p as i64 - 1) > size && size >= -1) {
            // `f` form with enough decimals: `p - 1 - size` (always >= 1 here).
            return Ok(float_f(f, (p as i64 - 1 - size) as usize));
        }
    }
    if p <= 1 {
        Ok(float_e(f, 2))
    } else {
        Ok(float_e(f, p))
    }
}

/// `io_lib_format:float_data/1` — the decimal digits and exponent of
/// `float_to_list(F, [{scientific, 20}])` (21 significant digits, exact).
fn float_data(f: f64) -> (Vec<u8>, i64) {
    // Rust's `{:.20e}` produces the same correctly-rounded 21 significant digits
    // as Erlang's `float_to_list(F, [{scientific,20}])`; only the exponent
    // formatting differs, which we parse here.
    let formatted = format!("{:.20e}", f.abs());
    let mut digits = Vec::with_capacity(21);
    let mut exponent = 0i64;
    if let Some((mantissa, exp)) = formatted.split_once('e') {
        digits.extend(mantissa.bytes().filter(|b| b.is_ascii_digit()));
        exponent = exp.parse().unwrap_or(0);
    }
    (digits, exponent + 1)
}

/// `io_lib_format:signbit/1` — `-` for a negative float, including `-0.0`.
fn signbit(f: f64) -> &'static str {
    if f.is_sign_negative() {
        "-"
    } else {
        ""
    }
}

/// `io_lib_format:float_exp/1` — the exponent with an explicit sign.
fn float_exp(exponent: i64) -> String {
    if exponent >= 0 {
        format!("e+{exponent}")
    } else {
        format!("e{exponent}")
    }
}

/// `float_man/3`: `i` digits before the point, `decimals` after, with carry.
fn float_man3(digits: &[u8], i: i64, decimals: i64) -> (String, bool) {
    if i == 0 {
        let (fraction, carry) = float_man2(digits, decimals);
        return (format!(".{fraction}"), carry);
    }
    match digits.split_first() {
        Some((&digit, rest)) => {
            let (tail, carry) = float_man3(rest, i - 1, decimals);
            round_digit(digit, tail, carry)
        }
        None => (
            format!("{}.{}", "0".repeat(i as usize), "0".repeat(decimals as usize)),
            false,
        ),
    }
}

/// `float_man/2`: `decimals` decimals (no integer part), with carry.
fn float_man2(digits: &[u8], decimals: i64) -> (String, bool) {
    if digits.is_empty() {
        return ("0".repeat(decimals.max(0) as usize), false);
    }
    if decimals == 0 {
        return (String::new(), digits[0] >= b'5');
    }
    let (tail, carry) = float_man2(&digits[1..], decimals - 1);
    round_digit(digits[0], tail, carry)
}

/// Shared rounding step of `float_man/2` and `float_man/3`.
fn round_digit(digit: u8, tail: String, carry: bool) -> (String, bool) {
    if carry {
        if digit == b'9' {
            (format!("0{tail}"), true)
        } else {
            (format!("{}{tail}", (digit + 1) as char), false)
        }
    } else {
        (format!("{}{tail}", digit as char), false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::print::flatten;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{atom_val, make_small, AtomTable, NIL};

    use crate::context::BytesSink;

    const SEED: [&str; 14] = [
        "true", "false", "ok", "hello", "a", "b", "c", "abc", "hi", "foo", "abcdefgh", "", "e",
        "chars_limit",
    ];

    /// Runs `io:format` over a fresh context, returning the captured output and the
    /// BIF result. `build_args` returns the argument list (or `None` for arity 1).
    fn run_io(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Option<Eterm>,
    ) -> (String, Result<Eterm>) {
        run_io_seeded(SEED, format, build_args)
    }

    /// [`run_io`] over a caller-chosen seeded atom table (the `~K` map orders, a
    /// nested-map key and a bogus atom are not in [`SEED`]).
    fn run_io_seeded<'a, I>(
        seed: I,
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Option<Eterm>,
    ) -> (String, Result<Eterm>)
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(seed);
        let mut sink = BytesSink::new();
        let result = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, format.as_bytes());
            match build_args(&mut ctx) {
                Some(args) => format_2(&mut ctx, &[format_term, args]),
                None => format_1(&mut ctx, &[format_term]),
            }
        };
        (sink.as_str(), result)
    }

    /// Runs `io_lib:format/2` and flattens the returned **deep list** back to a
    /// `String`.
    fn run_io_lib(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Eterm,
    ) -> (String, Result<Eterm>) {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let (result, rendered) = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, format.as_bytes());
            let args = build_args(&mut ctx);
            let result = format_lib_2(&mut ctx, &[format_term, args]);
            let rendered = match result {
                Ok(term) => deep_text(term),
                Err(BeamError::BadArg) => String::new(),
                Err(other) => panic!("unexpected error {other:?}"),
            };
            (result.map(|_| ()).map_err(|e| e), rendered)
        };
        (rendered, result.map(|()| make_small(0)).map_err(|e| e))
    }

    /// Deep-flattens an `io_lib:format` result (the oracle's nested char list) to
    /// its text — every nested sub-list contributes its characters in order.
    fn deep_text(term: Eterm) -> String {
        let mut out = String::new();
        let elems = proper_list(term).expect("io_lib:format returns a proper list");
        for elem in elems {
            if is_small(elem) {
                out.push(signed_value(elem) as u8 as char);
            } else {
                out.push_str(&deep_text(elem));
            }
        }
        out
    }

    /// Builds a format list term from raw ASCII bytes on `ctx`'s heap.
    fn list_from_bytes(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = bytes.iter().map(|&b| make_small(i64::from(b))).collect();
        ctx.list(&elems).expect("the format list fits")
    }

    /// A one-element argument list builder.
    fn one(ctx: &mut BifContext<'_>, term: Eterm) -> Option<Eterm> {
        Some(ctx.list(&[term]).expect("fits"))
    }

    fn ints(ctx: &mut BifContext<'_>, values: &[i64]) -> Option<Eterm> {
        let elems: Vec<Eterm> = values.iter().map(|&v| ctx.int(v).expect("fits")).collect();
        Some(ctx.list(&elems).expect("fits"))
    }

    fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = bytes.iter().map(|&b| ctx.int(i64::from(b)).expect("fits")).collect();
        ctx.list(&elems).expect("fits")
    }

    // ---- the original, still-required behaviours -------------------------------

    #[test]
    fn format_p_and_newline() {
        let (output, result) = run_io("~p~n", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one]).expect("fits");
            one_element(ctx, tuple)
        });
        assert_eq!(output, "{a,1}\n");
        assert!(result.is_ok());
    }

    fn one_element(ctx: &mut BifContext<'_>, term: Eterm) -> Option<Eterm> {
        one(ctx, term)
    }

    #[test]
    fn format_w_s_b_writes_exact_bytes() {
        let (output, result) = run_io("~w ~s ~b~n", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            let hi = byte_list(ctx, b"hi");
            let byte = ctx.int(255).expect("fits");
            Some(ctx.list(&[abc, hi, byte]).expect("fits"))
        });
        assert_eq!(output, "abc hi 255\n");
        assert!(result.is_ok());
    }

    #[test]
    fn tilde_d_is_badarg() {
        let (_, result) = run_io("~d", |ctx| ints(ctx, &[255]));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn integer_directive_rejects_non_integers() {
        let (_, result) = run_io("~b", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn newline_and_literal_tilde() {
        let (output, result) = run_io("a~~b~n", |_ctx| Some(NIL));
        assert_eq!(output, "a~b\n");
        assert!(result.is_ok());
    }

    #[test]
    fn ignore_directive_consumes_without_output() {
        let (output, result) = run_io("~i~w", |ctx| ints(ctx, &[5, 7]));
        assert_eq!(output, "7");
        assert!(result.is_ok());
    }

    #[test]
    fn format_1_uses_empty_args() {
        let (output, result) = run_io("hello~n", |_ctx| None);
        assert_eq!(output, "hello\n");
        assert!(result.is_ok());
    }

    #[test]
    fn bad_format_term_is_badarg() {
        let mut heap = Heap::new(64).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let atom = ctx.atom("abc").expect("seeded");
        assert!(matches!(format_1(&mut ctx, &[atom]), Err(BeamError::BadArg)));
        let bad = ctx
            .list(&[ctx.int(1000).expect("fits")])
            .expect("the list fits");
        assert!(matches!(format_1(&mut ctx, &[bad]), Err(BeamError::BadArg)));
    }

    #[test]
    fn int_to_base_digits_edge_cases() {
        // The shared renderer behind `~b`/`~x`/`~+` is
        // `numeric::integer_digits`, the same one `integer_to_list/2` uses.
        assert_eq!(integer_digits(make_small(0), 10).unwrap(), "0");
        assert_eq!(integer_digits(make_small(0), 16).unwrap(), "0");
        assert_eq!(integer_digits(make_small(255), 10).unwrap(), "255");
        assert_eq!(integer_digits(make_small(255), 16).unwrap(), "FF");
        assert_eq!(integer_digits(make_small(35), 36).unwrap(), "Z");
        assert_eq!(integer_digits(make_small(-255), 16).unwrap(), "-FF");
    }

    // ---- field widths, precision, adjust (`term/6`) ----------------------------

    #[test]
    fn field_width_pads_right_and_left() {
        let (output, _) = run_io("~10w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            one(ctx, abc)
        });
        assert_eq!(output, "       abc");

        let (output, _) = run_io("~-10w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one_]).expect("fits");
            one(ctx, tuple)
        });
        assert_eq!(output, "{a,1}     ");
    }

    #[test]
    fn overlong_rendering_becomes_stars() {
        // `io_lib:format("~4w",[{a,1}])` => "****".
        let (output, _) = run_io("~4w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one_]).expect("fits");
            one(ctx, tuple)
        });
        assert_eq!(output, "****");

        // `io_lib:format("~5.3w",["abcdefgh"])` => "  ***".
        let (output, _) = run_io("~5.3w", |ctx| {
            let s = byte_list(ctx, b"abcdefgh");
            one(ctx, s)
        });
        assert_eq!(output, "  ***");

        // `io_lib:format("~.0w",["abc"])` => "".
        let (output, _) = run_io("~.0w", |ctx| {
            let s = byte_list(ctx, b"abc");
            one(ctx, s)
        });
        assert_eq!(output, "");
    }

    #[test]
    fn newline_counts_and_adjust() {
        let (output, _) = run_io("~3n", |_ctx| Some(NIL));
        assert_eq!(output, "\n\n\n");
        let (output, _) = run_io("~0n", |_ctx| Some(NIL));
        assert_eq!(output, "");
        let (_, result) = run_io("~-3n", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn bad_base_and_precision_are_badarg() {
        for format in ["~.0b", "~.1b", "~.37b", "~.1+", "~.0#", "~.40#", "~.37+"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[5]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }

        // A `.` that is not followed by a digit or `*` is **not** a precision; the
        // oracle reads it as "no precision" and renders the directive normally
        // (verified against erts-17.1: `io_lib:format("~.b",[5])` => "5"). Only the
        // *second* `.` (the pad modifier) is malformed in that position.
        let (output, result) = run_io("~.b", |ctx| ints(ctx, &[5]));
        assert!(result.is_ok(), "~.b is not badarg in the oracle");
        assert_eq!(output, "5");
        for format in ["~.1e", "~.0f", "~.0g"] {
            let (_, result) = run_io(format, |ctx| {
                let f = ctx.make_float(1.5).expect("fits");
                one(ctx, f)
            });
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }
    }

    // ---- radix directives ------------------------------------------------------

    #[test]
    fn base_directives_and_hex_prefix() {
        let (output, _) = run_io("~.16B", |ctx| ints(ctx, &[255]));
        assert_eq!(output, "FF");
        let (output, _) = run_io("~.16b", |ctx| ints(ctx, &[255]));
        assert_eq!(output, "ff");
        let (output, _) = run_io("~.2B", |ctx| ints(ctx, &[5]));
        assert_eq!(output, "101");
        let (output, _) = run_io("~.16B", |ctx| ints(ctx, &[-255]));
        assert_eq!(output, "-FF");
        let (output, _) = run_io("~8.16b", |ctx| ints(ctx, &[255]));
        assert_eq!(output, "      ff");

        // `~x`/`~X` take `(Integer, Prefix)`; the prefix is a char list or atom.
        let (output, _) = run_io("~x", |ctx| {
            let n = ctx.int(255).expect("fits");
            let prefix = byte_list(ctx, b"#");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "#255");
        let (output, _) = run_io("~.16x", |ctx| {
            let n = ctx.int(255).expect("fits");
            let prefix = byte_list(ctx, b"#");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "#ff");
        let (output, _) = run_io("~.16X", |ctx| {
            let n = ctx.int(255).expect("fits");
            let prefix = byte_list(ctx, b"#");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        assert_eq!(output, "#FF");
        let (output, _) = run_io("~x", |ctx| {
            let n = ctx.int(-255).expect("fits");
            let prefix = byte_list(ctx, b"0x");
            Some(ctx.list(&[n, prefix]).expect("fits"))
        });
        // The sign precedes the prefix (verified against erts-17.1:
        // `io_lib:format("~x",[-255,"0x"])` => "-0x255").
        assert_eq!(output, "-0x255");

        // A single-argument `~x` is badarg (it needs two arguments) — this is the
        // shape the crate's integration test asserts.
        let (_, result) = run_io("~x", |ctx| ints(ctx, &[255]));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    /// The bignum term for `(negative, digits)` on `ctx`'s heap (the digits are
    /// least significant limb first). `BifContext` has no bignum constructor.
    fn bignum(ctx: &mut BifContext<'_>, negative: bool, digits: &[u64]) -> Eterm {
        beam_codec::term::make_big(ctx.heap(), negative, digits).expect("test bignum")
    }

    /// The limbs of `123456789012345678901234567890` (little-endian `u64`s).
    const BIG30: &[u64] = &[0xc373_e0ee_4e3f_0ad2, 0x18ee_90ff_6];

    #[test]
    fn base_prefix_directives_match_the_oracle() {
        // `io_lib_format:control_small($+…)`/`($#…)`: the integer in base `P`
        // (default 10) with the literal prefix `Base#`, digits lowercased for `~+`
        // and uppercased for `~#`. Every expectation is
        // `erl -noshell -eval 'io:format("~s~n",[lists:flatten(io_lib:format(F,A))]).'`
        // on erts-17.1.
        for (format, value, expected) in [
            ("~+", 255, "10#255"),
            ("~#", 255, "10#255"),
            ("~+", 0, "10#0"),
            ("~#", 0, "10#0"),
            ("~+", -255, "-10#255"),
            ("~#", -1, "-10#1"),
            ("~.16+", 255, "16#ff"),
            ("~.16#", 255, "16#FF"),
            ("~.2+", 5, "2#101"),
            ("~.2#", -5, "-2#101"),
            ("~.36+", 255, "36#73"),
            ("~.36#", 1295, "36#ZZ"),
            ("~.36+", 0, "36#0"),
            ("~.16#", -255, "-16#FF"),
            ("~8+", 255, "  10#255"),
            ("~-8+", 255, "10#255  "),
            ("~8.16+", 255, "   16#ff"),
            ("~-8.16#", 255, "16#FF   "),
            // The `term/6` truncation rule: a rendering longer than the field is
            // exactly F `*` characters.
            ("~3+", 255, "***"),
            ("~6+", 255, "10#255"),
            ("~7+", 255, " 10#255"),
            ("~4.16#", 255, "****"),
            // A trailing character after the control is a literal.
            ("~+#", 255, "10#255#"),
            ("~++", 255, "10#255+"),
            ("~.16#+", 255, "16#FF+"),
        ] {
            let (output, result) = run_io(format, |ctx| ints(ctx, &[value]));
            assert!(result.is_ok(), "{format} should succeed");
            assert_eq!(output, expected, "format {format}");
        }

        // A non-integer argument is `badarg` for both.
        for format in ["~+", "~#"] {
            let (_, result) = run_io(format, |ctx| {
                let f = ctx.make_float(1.0).expect("fits");
                one(ctx, f)
            });
            assert!(matches!(result, Err(BeamError::BadArg)), "{format} of a float");
        }
        // `~+`/`~#` take exactly one argument.
        for format in ["~+", "~#"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[255, 1]));
            assert!(matches!(result, Err(BeamError::BadArg)), "{format} extra arg");
        }
        // The `.*` pad form consumes its argument before the integer's
        // (`io_lib:format("~.16.*#",[255,$x])` ⇒ `"16#78"`).
        let (output, result) = run_io("~.16.*#", |ctx| ints(ctx, &[255, 0x78]));
        assert!(result.is_ok());
        assert_eq!(output, "16#78");
    }
    #[test]
    fn radix_directives_render_arbitrary_precision_integers() {
        // Verified against erts-17.1:
        //   io_lib:format("~.16#", [123456789012345678901234567890])
        //     => "16#18EE90FF6C373E0EE4E3F0AD2"
        for (format, negative, expected) in [
            ("~b", false, "123456789012345678901234567890"),
            ("~B", false, "123456789012345678901234567890"),
            ("~b", true, "-123456789012345678901234567890"),
            ("~.16b", false, "18ee90ff6c373e0ee4e3f0ad2"),
            ("~.16B", false, "18EE90FF6C373E0EE4E3F0AD2"),
            ("~.16+", false, "16#18ee90ff6c373e0ee4e3f0ad2"),
            ("~.16#", false, "16#18EE90FF6C373E0EE4E3F0AD2"),
            ("~.16+", true, "-16#18ee90ff6c373e0ee4e3f0ad2"),
            ("~.16#", true, "-16#18EE90FF6C373E0EE4E3F0AD2"),
            ("~.35+", true, "-35#jukwca2sruj23ym3kt0"),
            // Longer than the field -> F `*` characters.
            ("~8.16b", false, "********"),
            ("~40b", false, "          123456789012345678901234567890"),
            // `~.2#` of the negative bignum: the whole 97-bit magnitude, with the
            // sign ahead of the `2#` prefix.
            ("~.2#", true, "-2#1100011101110100100001111111101101100001101110011111000001110111001001110001111110000101011010010"),
        ] {
            let (output, result) = run_io(format, |ctx| {
                let term = bignum(ctx, negative, BIG30);
                one(ctx, term)
            });
            assert!(result.is_ok(), "{format} should succeed");
            assert_eq!(output, expected, "format {format}");
        }

        // `~x`/`~X` too (the `(Integer, Prefix)` shape).
        for (format, upper) in [("~.16x", false), ("~.16X", true)] {
            let (output, result) = run_io(format, |ctx| {
                let term = bignum(ctx, false, BIG30);
                let prefix = byte_list(ctx, b"#");
                Some(ctx.list(&[term, prefix]).expect("fits"))
            });
            assert!(result.is_ok(), "{format} should succeed");
            let digits = if upper {
                "18EE90FF6C373E0EE4E3F0AD2"
            } else {
                "18ee90ff6c373e0ee4e3f0ad2"
            };
            assert_eq!(output, format!("#{digits}"), "format {format}");
        }
    }

    // ---- floats ----------------------------------------------------------------

    #[test]
    fn float_directives_match_the_oracle() {
        for (format, value, expected) in [
            ("~e", 1.5, "1.50000e+0"),
            ("~f", 1.5, "1.500000"),
            ("~g", 1.5, "1.50000"),
            ("~e", 1234.5678, "1.23457e+3"),
            ("~f", 1234.5678, "1234.567800"),
            ("~g", 1234.5678, "1234.57"),
            ("~e", 0.0, "0.00000e+0"),
            ("~f", 0.0, "0.000000"),
            ("~g", 0.0, "0.00000e+0"),
            ("~g", 0.1, "0.100000"),
            ("~g", 100000.0, "1.00000e+5"),
            ("~g", 99.0, "99.0000"),
            ("~g", 9999.0, "9999.00"),
            ("~g", 10000.0, "1.00000e+4"),
            ("~g", -1.5, "-1.50000"),
            ("~f", 1.0e10, "10000000000.000000"),
            ("~.1f", 1.5, "1.5"),
            ("~.2f", 0.1, "0.10"),
            ("~.2f", 1.567, "1.57"),
            ("~.2e", 1.567, "1.6e+0"),
            ("~.3g", 1.567, "1.57"),
            ("~12.4f", 1.567, "      1.5670"),
            ("~-12.4f", 1.567, "1.5670      "),
            ("~e", -0.0, "-0.00000e+0"),
        ] {
            let (output, result) = run_io(format, |ctx| {
                let f = ctx.make_float(value).expect("fits");
                one(ctx, f)
            });
            assert!(result.is_ok(), "{format} of {value} errored");
            assert_eq!(output, expected, "{format} of {value}");
        }
    }

    #[test]
    fn float_directives_reject_integers() {
        for format in ["~e", "~f", "~g"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[1]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "{format} of an integer should be badarg"
            );
        }
    }

    // ---- strings and characters ------------------------------------------------

    #[test]
    fn string_directive_accepts_atoms_lists_and_binaries() {
        let (output, _) = run_io("~s", |ctx| {
            let hello = ctx.atom("hello").expect("seeded");
            one(ctx, hello)
        });
        assert_eq!(output, "hello");

        let (output, _) = run_io("~s", |ctx| {
            let hi = byte_list(ctx, b"hi");
            one(ctx, hi)
        });
        assert_eq!(output, "hi");

        let (output, _) = run_io("~s", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"hi").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "hi");

        // Width/truncation: `~2s` of "hello" => "he"; `~10.3s` => "       hel".
        let (output, _) = run_io("~2s", |ctx| {
            let hi = byte_list(ctx, b"hello");
            one(ctx, hi)
        });
        assert_eq!(output, "he");
        let (output, _) = run_io("~10.3s", |ctx| {
            let hi = byte_list(ctx, b"hello");
            one(ctx, hi)
        });
        assert_eq!(output, "       hel");
        let (output, _) = run_io("~-10.3s", |ctx| {
            let hi = byte_list(ctx, b"hello");
            one(ctx, hi)
        });
        assert_eq!(output, "hel       ");

        // A list element outside 0..=255 is badarg.
        let (_, result) = run_io("~s", |ctx| ints(ctx, &[8364]));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn character_directive() {
        let (output, _) = run_io("~c", |ctx| ints(ctx, &[65]));
        assert_eq!(output, "A");
        let (output, _) = run_io("~3c", |ctx| ints(ctx, &[65]));
        assert_eq!(output, "AAA");
        let (output, _) = run_io("~tc", |ctx| ints(ctx, &[65]));
        assert_eq!(output, "A");
        let (_, result) = run_io("~c", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    // ---- `~c`/`~tc`: the code point verbatim ----------------------------------

    /// `1 bsl 100` as little-endian limbs (`2^100 = 2^64 * 2^36`).
    const SHIFTED_100: &[u64] = &[0, 1 << 36];

    /// `(1 bsl 64) + 255`: a bignum whose low byte is `255`.
    const BIGNUM_LOW_255: &[u64] = &[255, 1];

    /// The flattened element list of an `io_lib:format` result — each element's
    /// decimal (`~w`-style), comma-joined. [`deep_text`] cannot be used here: it
    /// assumes every element is a byte-sized character, while `~tc` of an
    /// out-of-range code point carries the integer verbatim.
    fn run_io_lib_codes(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Option<Eterm>,
    ) -> (String, Result<Eterm>) {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let (result, codes) = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, format.as_bytes());
            let args = build_args(&mut ctx).expect("an argument list");
            let result = format_lib_2(&mut ctx, &[format_term, args]);
            let codes = match &result {
                Ok(term) => element_codes(*term),
                Err(BeamError::BadArg) => String::new(),
                Err(other) => panic!("unexpected error {other:?}"),
            };
            (result, codes)
        };
        (codes, result.map(|_| make_small(0)).map_err(|e| e))
    }

    /// Flattens an `io_lib:format` result to its element codes (see
    /// [`run_io_lib_codes`]).
    fn element_codes(term: Eterm) -> String {
        let mut out: Vec<String> = Vec::new();
        collect_codes(term, &mut out);
        out.join(",")
    }

    fn collect_codes(term: Eterm, out: &mut Vec<String>) {
        if !is_small(term) && !is_big(term) {
            for elem in proper_list(term).expect("io_lib:format returns a proper list") {
                collect_codes(elem, out);
            }
            return;
        }
        out.push(if is_small(term) {
            signed_value(term).to_string()
        } else {
            integer_digits(term, 10).expect("a bignum renders")
        });
    }

    /// `io:format/2` captures the **raw bytes** (see [`run_io_bytes`]).
    fn run_io_bytes(
        format: &str,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Option<Eterm>,
    ) -> (Vec<u8>, Result<Eterm>) {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut sink = BytesSink::new();
        let result = {
            let mut ctx =
                BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, format.as_bytes());
            let args = build_args(&mut ctx).expect("an argument list");
            format_2(&mut ctx, &[format_term, args])
        };
        (sink.contents().to_vec(), result)
    }

    /// `~tc` renders the integer **verbatim**, even when it is not a Unicode
    /// scalar (the oracle's `char(A, …)` is `[A]`). Every expectation is the
    /// oracle's (`erl -noshell -eval 'io:format("~w~n",[io_lib:format(F,A)]).'`).
    #[test]
    fn tilde_tc_carries_the_code_point_verbatim() {
        // `io_lib:format("~tc",[1114112])` ⇒ `[1114112]` (past `16#10FFFF`).
        let (codes, result) = run_io_lib_codes("~tc", |ctx| ints(ctx, &[1114112]));
        assert!(result.is_ok());
        assert_eq!(codes, "1114112");

        // A surrogate (`16#D800`) is carried too: ⇒ `[55296]`.
        let (codes, _) = run_io_lib_codes("~tc", |ctx| ints(ctx, &[55296]));
        assert_eq!(codes, "55296");

        // A negative code point and zero: ⇒ `[-1]`, `[0]`.
        let (codes, _) = run_io_lib_codes("~tc", |ctx| ints(ctx, &[-1]));
        assert_eq!(codes, "-1");
        let (codes, _) = run_io_lib_codes("~tc", |ctx| ints(ctx, &[0]));
        assert_eq!(codes, "0");

        // A valid scalar is still the character itself: `[16#10FFFF]`.
        let (codes, _) = run_io_lib_codes("~tc", |ctx| ints(ctx, &[0x10FFFF]));
        assert_eq!(codes, "1114111");

        // A bignum is carried in full: `~tc` of `1 bsl 100` ⇒ the bignum.
        let (codes, _) = run_io_lib_codes("~tc", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), false, SHIFTED_100).expect("test big");
            one(ctx, big)
        });
        assert_eq!(codes, "1267650600228229401496703205376");
        let (codes, _) = run_io_lib_codes("~tc", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), true, SHIFTED_100).expect("test big");
            one(ctx, big)
        });
        assert_eq!(codes, "-1267650600228229401496703205376");

        // Width/precision duplicate the raw element (`chars/2`): `~3tc` ⇒ three
        // copies, `~3.2tc` ⇒ one pad space and two copies.
        let (codes, _) = run_io_lib_codes("~3tc", |ctx| ints(ctx, &[1114112]));
        assert_eq!(codes, "1114112,1114112,1114112");
        let (codes, _) = run_io_lib_codes("~3.2tc", |ctx| ints(ctx, &[1114112]));
        assert_eq!(codes, "32,1114112,1114112");
    }

    /// `~c` (latin1) keeps only the low byte of its integer argument —
    /// `char(A band 255, …)` — bignums included.
    #[test]
    fn tilde_c_masks_to_the_low_byte() {
        let (codes, _) = run_io_lib_codes("~c", |ctx| ints(ctx, &[256]));
        assert_eq!(codes, "0");
        let (codes, _) = run_io_lib_codes("~c", |ctx| ints(ctx, &[257]));
        assert_eq!(codes, "1");
        let (codes, _) = run_io_lib_codes("~c", |ctx| ints(ctx, &[-1]));
        assert_eq!(codes, "255");
        let (codes, _) = run_io_lib_codes("~c", |ctx| ints(ctx, &[1114112]));
        assert_eq!(codes, "0");

        // A bignum's low byte (`(1 bsl 64) + 255` ⇒ `255`; negated ⇒ `1`; and
        // `1 bsl 100` has an all-zero low limb ⇒ `0`).
        let (codes, _) = run_io_lib_codes("~c", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), false, BIGNUM_LOW_255).expect("test big");
            one(ctx, big)
        });
        assert_eq!(codes, "255");
        let (codes, _) = run_io_lib_codes("~c", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), true, BIGNUM_LOW_255).expect("test big");
            one(ctx, big)
        });
        assert_eq!(codes, "1");
        let (codes, _) = run_io_lib_codes("~c", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), false, SHIFTED_100).expect("test big");
            one(ctx, big)
        });
        assert_eq!(codes, "0");
    }

    /// `io:format` writes a valid code point as its **UTF-8** encoding and raises
    /// `badarg` for one the oracle's unicode device refuses (never a fabricated
    /// byte).
    #[test]
    fn tilde_tc_write_path_is_utf8_or_badarg() {
        // `io:format("~tc",[16#10FFFF])` ⇒ the UTF-8 bytes `F4 8F BF BF`.
        let (bytes, result) = run_io_bytes("~tc", |ctx| ints(ctx, &[0x10FFFF]));
        assert!(result.is_ok());
        assert_eq!(bytes, vec![0xF4, 0x8F, 0xBF, 0xBF]);

        // `io:format("~c",[255])` ⇒ the UTF-8 bytes `C3 BF`.
        let (bytes, result) = run_io_bytes("~c", |ctx| ints(ctx, &[255]));
        assert!(result.is_ok());
        assert_eq!(bytes, vec![0xC3, 0xBF]);

        // Out of range / surrogate / negative / bignum ⇒ `error:badarg`.
        for value in [1114112i64, 55296, -1] {
            let (bytes, result) = run_io_bytes("~tc", move |ctx| ints(ctx, &[value]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "~tc of {value} should be badarg, wrote {bytes:?}"
            );
            assert!(bytes.is_empty(), "nothing is written on badarg");
        }
        let (bytes, result) = run_io_bytes("~tc", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), false, SHIFTED_100).expect("test big");
            one(ctx, big)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
        assert!(bytes.is_empty());
    }    // ---- width from the argument, depth ---------------------------------------

    #[test]
    fn width_and_precision_from_arguments() {
        let (output, _) = run_io("~*w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            Some(ctx.list(&[ctx.int(6).expect("fits"), abc]).expect("fits"))
        });
        assert_eq!(output, "   abc");

        let (output, _) = run_io("~*w", |ctx| {
            let abc = ctx.atom("abc").expect("seeded");
            Some(ctx.list(&[ctx.int(-6).expect("fits"), abc]).expect("fits"))
        });
        assert_eq!(output, "abc   ");

        let (output, _) = run_io("~.*b", |ctx| {
            let n = ctx.int(255).expect("fits");
            Some(ctx.list(&[ctx.int(16).expect("fits"), n]).expect("fits"))
        });
        assert_eq!(output, "ff");

        let (output, _) = run_io("~*w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one_]).expect("fits");
            Some(ctx.list(&[ctx.int(4).expect("fits"), tuple]).expect("fits"))
        });
        assert_eq!(output, "****");
    }

    #[test]
    fn depth_directives() {
        let term = |ctx: &mut BifContext<'_>| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let inner = ctx.tuple(&[c, one_]).expect("fits");
            let mid = ctx.tuple(&[b, inner]).expect("fits");
            ctx.tuple(&[a, mid]).expect("fits")
        };
        let (output, _) = run_io("~W", |ctx| {
            let t = term(ctx);
            Some(ctx.list(&[t, ctx.int(3).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "{a,{...}}");
        let (output, _) = run_io("~W", |ctx| {
            let t = term(ctx);
            Some(ctx.list(&[t, ctx.int(2).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "{a,...}");
        let (output, _) = run_io("~10W", |ctx| {
            let t = term(ctx);
            Some(ctx.list(&[t, ctx.int(3).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, " {a,{...}}");

        // `~P` of a list at depth 3.
        let (output, _) = run_io("~P", |ctx| {
            let list = ctx
                .list(&[
                    ctx.int(1).expect("fits"),
                    ctx.int(2).expect("fits"),
                    ctx.int(3).expect("fits"),
                    ctx.int(4).expect("fits"),
                    ctx.int(5).expect("fits"),
                ])
                .expect("fits");
            Some(ctx.list(&[list, ctx.int(3).expect("fits")]).expect("fits"))
        });
        assert_eq!(output, "[1,2|...]");
    }

    // ---- the extended printer --------------------------------------------------

    #[test]
    fn write_and_print_of_binaries_maps_and_bignums() {
        let (output, _) = run_io("~w", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<97,98,99>>");

        let (output, _) = run_io("~p", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<\"abc\">>");

        let (output, _) = run_io("~p", |ctx| {
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<1,2,3>>");

        let (output, _) = run_io("~w", |ctx| {
            let bin = bits::make_binary(ctx.heap(), b"").expect("fits");
            one(ctx, bin)
        });
        assert_eq!(output, "<<>>");

        // A flat map renders `#{k => v,…}` in the map's stored order — for a flat map
        // the flatmap key order (a < b). `make_flatmap` preserves the pair order it is
        // handed, so canonicalise exactly as the writers do (`maps.rs::build_map`).
        let (output, _) = run_io("~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one_ = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let mut pairs = [(b, one_), (a, two)];
            pairs.sort_by(|x, y| beam_term::compare::cmp_flatmap_keys(x.0, y.0, ctx.atoms()));
            let map = crate::access::make_flatmap(ctx.heap(), &pairs).expect("fits");
            one(ctx, map)
        });
        assert_eq!(output, "#{a => 2,b => 1}");

        // A bignum renders in decimal.
        let (output, _) = run_io("~w", |ctx| {
            let big = beam_codec::term::make_big(ctx.heap(), false, &[0, 1]).expect("fits");
            one(ctx, big)
        });
        assert_eq!(output, "18446744073709551616");
    }

    #[test]
    fn print_of_a_long_container_reflows_like_the_oracle() {
        // The oracle breaks a 40-element list under `~p` at the 80-column default;
        // erts-17.1 `io:format("~p",[lists:seq(0,39)])` is exactly:
        //   [0,1,…,27,28,
        //    29,…,39]
        let (output, result) = run_io("~p", |ctx| {
            let elems: Vec<Eterm> = (0..40).map(|i| ctx.int(i).expect("fits")).collect();
            let list = ctx.list(&elems).expect("fits");
            one(ctx, list)
        });
        assert!(result.is_ok(), "re-flowed: {result:?}");
        assert_eq!(
            output,
            "[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n 29,30,31,32,33,34,35,36,37,38,39]"
        );

        // A short list is fine.
        let (output, result) = run_io("~p", |ctx| {
            let elems: Vec<Eterm> = (1..=5).map(|i| ctx.int(i).expect("fits")).collect();
            let list = ctx.list(&elems).expect("fits");
            one(ctx, list)
        });
        assert!(result.is_ok());
        assert_eq!(output, "[1,2,3,4,5]");

        // A left-adjusted `~p` has no clause in the oracle.
        let (_, result) = run_io("~-10p", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    // ---- io_lib:format ---------------------------------------------------------

    #[test]
    fn io_lib_format_returns_an_iolist() {
        let (rendered, result) = run_io_lib("~b~n", |ctx| {
            ctx.list(&[ctx.int(255).expect("fits")]).expect("fits")
        });
        assert!(result.is_ok());
        assert_eq!(rendered, "255\n");

        // The empty rendering is `NIL`.
        let mut heap = Heap::new(128).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let format_term = list_from_bytes(&mut ctx, b"");
        let result = format_lib_2(&mut ctx, &[format_term, NIL]).expect("fits");
        assert_eq!(result.raw(), NIL.raw());

        // `io_lib:format` writes nothing to the output sink.
        let mut sink = BytesSink::new();
        {
            let mut heap = Heap::new(128).expect("test heap");
            let mut atoms = AtomTable::new();
            atoms.seed(SEED);
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_output(&mut sink);
            let format_term = list_from_bytes(&mut ctx, b"hi~n");
            assert!(format_lib_2(&mut ctx, &[format_term, NIL]).is_ok());
        }
        assert_eq!(sink.contents(), b"");
    }

    /// The result of one `io_lib:format` run, materialized *before* the context's
    /// heap is dropped (the returned `Eterm`s must not outlive it).
    #[derive(Debug)]
    enum Outcome {
        Text(String),
        Err(BeamError),
    }

    fn outcome(result: Result<Eterm>) -> Outcome {
        match result {
            Ok(term) => Outcome::Text(deep_text(term)),
            Err(err) => Outcome::Err(err),
        }
    }

    fn text(outcome: Outcome) -> String {
        match outcome {
            Outcome::Text(text) => text,
            Outcome::Err(err) => panic!("expected a rendering, got {err:?}"),
        }
    }

    /// The nested-list **shape** of an `io_lib:format` result — the oracle's
    /// structure, which a flattened-text comparison cannot distinguish
    /// (`[[91,[46,46,46],93]]` vs `[[91,46,46,46,93]]`).
    fn deep_shape(term: Eterm) -> String {
        if is_small(term) {
            return signed_value(term).to_string();
        }
        let elems = proper_list(term).expect("io_lib:format returns a proper list");
        let parts: Vec<String> = elems.iter().map(|&e| deep_shape(e)).collect();
        format!("[{}]", parts.join(","))
    }

    /// Renders `format` with the argument list `build_args` builds under
    /// `[{chars_limit, Limit}]` through `io_lib:format/3`, materializing both the
    /// flattened text and the result's [`deep_shape`] before the heap is dropped.
    fn run_limited(
        format: &str,
        limit: i64,
        build_args: impl FnOnce(&mut BifContext<'_>) -> Eterm,
    ) -> Result<(String, String)> {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let format_term = list_from_bytes(&mut ctx, format.as_bytes());
        let args = build_args(&mut ctx);
        let limit_term = ctx.int(limit).expect("fits");
        let options = chars_limit(&mut ctx, limit_term);
        format_lib_3(&mut ctx, &[format_term, args, options])
            .map(|term| (deep_text(term), deep_shape(term)))
    }

    /// A proper list of small-int elements on `ctx`'s heap.
    fn int_list(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
        let elems: Vec<Eterm> = values.iter().map(|&v| ctx.int(v).expect("fits")).collect();
        ctx.list(&elems).expect("fits")
    }

    /// The argument list `[List]` for a one-argument `~w`/`~p` directive.
    fn list_arg(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
        let list = int_list(ctx, values);
        ctx.list(&[list]).expect("fits")
    }
    /// Renders `format` with the small-int `args` under `build_options`, both
    /// through `io_lib:format/3` and (as the reference) `io_lib:format/2`.
    fn run_lib3(
        format: &str,
        args: &[i64],
        build_options: impl Fn(&mut BifContext<'_>) -> Eterm,
    ) -> (Outcome, Outcome) {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let format_term = list_from_bytes(&mut ctx, format.as_bytes());
        let arg_elems: Vec<Eterm> = args.iter().map(|&v| ctx.int(v).expect("fits")).collect();
        let args3 = ctx.list(&arg_elems).expect("fits");
        let args2 = ctx.list(&arg_elems).expect("fits");
        let options = build_options(&mut ctx);
        let three = outcome(format_lib_3(&mut ctx, &[format_term, args3, options]));
        let two = outcome(format_lib_2(&mut ctx, &[format_term, args2]));
        (three, two)
    }

    fn chars_limit(ctx: &mut BifContext<'_>, limit: Eterm) -> Eterm {
        let tag = ctx.atom("chars_limit").expect("seeded");
        let option = ctx.tuple(&[tag, limit]).expect("fits");
        ctx.list(&[option]).expect("fits")
    }

    fn option_empty_tuple(ctx: &mut BifContext<'_>) -> Eterm {
        let empty = ctx.tuple(&[]).expect("fits");
        ctx.list(&[empty]).expect("fits")
    }

    fn option_foo_atom(ctx: &mut BifContext<'_>) -> Eterm {
        let foo = ctx.atom("foo").expect("seeded");
        ctx.list(&[foo]).expect("fits")
    }

    fn option_chars_limit_neg(ctx: &mut BifContext<'_>) -> Eterm {
        let neg = ctx.int(-1).expect("fits");
        chars_limit(ctx, neg)
    }

    fn option_chars_limit_three(ctx: &mut BifContext<'_>) -> Eterm {
        let three = ctx.int(3).expect("fits");
        chars_limit(ctx, three)
    }

    fn option_chars_limit_atom(ctx: &mut BifContext<'_>) -> Eterm {
        let value = ctx.atom("foo").expect("seeded");
        chars_limit(ctx, value)
    }

    fn option_not_a_list(ctx: &mut BifContext<'_>) -> Eterm {
        ctx.atom("foo").expect("seeded")
    }

    #[test]
    fn io_lib_format_3_options_are_validated() {
        // Empty options reproduce `format/2` exactly.
        let (three, two) = run_lib3("~w", &[42], |ctx| ctx.list(&[]).expect("fits"));
        let reference = text(two);
        assert_eq!(text(three), reference);
        assert_eq!(reference, "42");

        // Unknown options are ignored (the oracle only reads `chars_limit`).
        for build in [option_empty_tuple, option_foo_atom] {
            let (three, two) = run_lib3("~w", &[42], build);
            assert_eq!(text(three), text(two), "an unknown option must be ignored");
        }

        // A negative `chars_limit` is the oracle's default (no limit).
        let (three, two) = run_lib3("~w", &[42], option_chars_limit_neg);
        assert_eq!(text(three), text(two));

        // A limiting `chars_limit` is honoured: `~w` of `42` has room to spare in
        // a 3-character budget, so the oracle renders it unchanged (`[[52,50]]`).
        let (three, _) = run_lib3("~w", &[42], option_chars_limit_three);
        assert_eq!(text(three), "42");

        // A non-integer `chars_limit` value and a non-list `Options` are `badarg`.
        let (three, _) = run_lib3("~w", &[42], option_chars_limit_atom);
        assert!(matches!(three, Outcome::Err(BeamError::BadArg)));
        let (three, _) = run_lib3("~w", &[42], option_not_a_list);
        assert!(matches!(three, Outcome::Err(BeamError::BadArg)));

        // `format/3` still renders directives normally.
        let (three, _) = run_lib3("~b~n", &[255], |ctx| ctx.list(&[]).expect("fits"));
        assert_eq!(text(three), "255\n");
    }

    #[test]
    fn io_lib_format_3_chars_limit_elides_a_term_control() {
        // A zero budget elides the whole control — the oracle's `["..."]`.
        let (text, shape) =
            run_limited("~w", 0, |ctx| list_arg(ctx, &[1, 2, 3])).expect("renders");
        assert_eq!(text, "...");
        assert_eq!(shape, "[[46,46,46]]");

        // The `[1,2,3]` rendering is 7 characters, so any budget below that elides
        // the container's contents to `"..."`: the oracle's `[[91,"...",93]]`.
        for limit in [1, 2, 3, 5] {
            let (text, shape) = run_limited("~w", limit, |ctx| list_arg(ctx, &[1, 2, 3]))
                .unwrap_or_else(|err| panic!("limit {limit}: {err:?}"));
            assert_eq!(text, "[...]", "limit {limit}");
            assert_eq!(shape, "[[91,[46,46,46],93]]", "limit {limit}");
        }

        // A budget the rendering fits into is the plain unlimited deep list
        // (`[[91,["1",44,"2",44,"3"],93]]`), i.e. the elements stay sub-lists.
        for limit in [7, 100] {
            let (text, shape) = run_limited("~w", limit, |ctx| list_arg(ctx, &[1, 2, 3]))
                .unwrap_or_else(|err| panic!("limit {limit}: {err:?}"));
            assert_eq!(text, "[1,2,3]", "limit {limit}");
            assert_eq!(
                shape, "[[91,[[49],44,[50],44,[51]],93]]",
                "limit {limit}"
            );
        }

        // `~p` takes the same limited path (the flat `write/1` arm).
        let (text, shape) =
            run_limited("~p", 3, |ctx| list_arg(ctx, &[1, 2, 3])).expect("renders");
        assert_eq!(text, "[...]");
        assert_eq!(shape, "[[91,[46,46,46],93]]");

        // `~W`/`~P` too (the depth argument is consumed before the budget).
        let (text, _) = run_limited("~W", 4, |ctx| {
            let list = int_list(ctx, &[1, 2, 3]);
            let depth = ctx.int(2).expect("fits");
            ctx.list(&[list, depth]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "[...]");
    }

    #[test]
    fn io_lib_format_3_chars_limit_accounts_for_literal_text() {
        // The literal `hello ` / ` world` is never limited but is charged against
        // the budget first (`Other`): a 10-character limit leaves nothing, so the
        // `[1..10]` control elides to `"..."` (the oracle's
        // `[104,101,108,108,111,32,"...",32,…]`).
        let (text, shape) = run_limited("hello ~w world", 10, |ctx| {
            list_arg(ctx, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10])
        })
        .expect("renders");
        assert_eq!(text, "hello ... world");
        assert_eq!(
            shape,
            "[104,101,108,108,111,32,[46,46,46],32,119,111,114,108,100]"
        );

        // A small control's characters count toward `Other` the same way.
        let (text, shape) = run_limited("~c~w", 6, |ctx| {
            let ch = ctx.int(i64::from(b'x')).expect("fits");
            let list = int_list(ctx, &[1, 2, 3]);
            ctx.list(&[ch, list]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "x[...]");
        assert_eq!(shape, "[120,[91,[46,46,46],93]]");
    }

    #[test]
    fn io_lib_format_3_chars_limit_splits_the_budget_between_controls() {
        // `~w ~w` with `{chars_limit,8}`: the literal space is `Other = 1`, so the
        // remaining 7 is split `7 div 2 = 3` for the first control, then
        // `7 - 3 = 4` for the second — both elide (the oracle's
        // `[[91,"...",93],32,[91,"...",93]]`). Giving each control the whole 7
        // would render `[1,2,3]` twice, so this pins the per-directive budget.
        let (text, shape) = run_limited("~w ~w", 8, |ctx| {
            let first = int_list(ctx, &[1, 2, 3]);
            let second = int_list(ctx, &[4, 5, 6]);
            ctx.list(&[first, second]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "[...] [...]");
        assert_eq!(shape, "[[91,[46,46,46],93],32,[91,[46,46,46],93]]");
    }

    #[test]
    fn io_lib_format_3_chars_limit_limits_a_string_control() {
        // `~s` of a **list** under an active limit is a "big" control: its text is
        // limited to the per-directive budget and elided with `"..."`.
        // `io_lib:format("~s", ["hello world!!"], [{chars_limit,10}])` on the oracle
        // is `["hello w..."]`, i.e. the deep list
        // `[[104,101,108,108,111,32,119,46,46,46]]`.
        let (text, shape) = run_limited("~s", 10, |ctx| {
            let s = byte_list(ctx, b"hello world!!");
            ctx.list(&[s]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "hello w...");
        assert_eq!(shape, "[[104,101,108,108,111,32,119,46,46,46]]");

        // A budget the text fits into keeps it whole; a `0` budget still probes a
        // further four characters (the oracle's `final` mode), so a 3-character
        // string survives every budget. `~s` of "abc" is `[[97,98,99]]` throughout.
        for limit in [0, 3, 100] {
            let (text, shape) = run_limited("~s", limit, |ctx| {
                let s = byte_list(ctx, b"abc");
                ctx.list(&[s]).expect("fits")
            })
            .unwrap_or_else(|err| panic!("limit {limit}: {err:?}"));
            assert_eq!(text, "abc", "limit {limit}");
            assert_eq!(shape, "[[97,98,99]]", "limit {limit}");
        }

        // A negative budget is the unlimited path (byte-identical to `format/2`).
        let (text, shape) = run_limited("~s", -1, |ctx| {
            let s = byte_list(ctx, b"hello world!!");
            ctx.list(&[s]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "hello world!!");
        assert_eq!(shape, "[[104,101,108,108,111,32,119,111,114,108,100,33,33]]");

        // The directive's field width is clamped to the budget (`limit_field/2`):
        // `io_lib:format("~8s", ["abc"], [{chars_limit,6}])` is `[["   ",97,98,99]]`.
        let (text, shape) = run_limited("~8s", 6, |ctx| {
            let s = byte_list(ctx, b"abc");
            ctx.list(&[s]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "   abc");
        assert_eq!(shape, "[[[32,32,32],97,98,99]]");

        // A field width below 3 is raised to 3 (`limit_field`'s floor) and the text
        // is truncated to it: `~2s` of "hello" with limit 10 is `["hel"]`.
        let (text, shape) = run_limited("~2s", 10, |ctx| {
            let s = byte_list(ctx, b"hello");
            ctx.list(&[s]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "hel");
        assert_eq!(shape, "[[104,101,108]]");

        // A precision truncates the limited text without a field: `~.2s` of "hello"
        // with limit 10 is `["he"]`.
        let (text, shape) = run_limited("~.2s", 10, |ctx| {
            let s = byte_list(ctx, b"hello");
            ctx.list(&[s]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "he");
        assert_eq!(shape, "[[104,101]]");

        // The literal text is charged against the budget first and is never elided:
        // `io_lib:format("hello ~s world", ["abcdefghij"], [{chars_limit,10}])`
        // elides only the `~s` (`[104,…,32,[46,46,46],32,…]`, "hello ... world").
        let (text, shape) = run_limited("hello ~s world", 10, |ctx| {
            let s = byte_list(ctx, b"abcdefghij");
            ctx.list(&[s]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "hello ... world");
        assert_eq!(
            shape,
            "[104,101,108,108,111,32,[46,46,46],32,119,111,114,108,100]"
        );

        // `~s` and `~w` share the `w` bucket, so `~s ~w` with `{chars_limit,6}`
        // splits the budget: the `~s` gets 3, the `~w` the rest, and each elides its
        // own overflow (`[[97,98,99],32,[91,[46,46,46],93]]`, "abc [...]").
        let (text, shape) = run_limited("~s ~w", 6, |ctx| {
            let s = byte_list(ctx, b"abc");
            let l = int_list(ctx, &[1, 2, 3]);
            ctx.list(&[s, l]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "abc [...]");
        assert_eq!(shape, "[[97,98,99],32,[91,[46,46,46],93]]");

        // An invalid `~s` iolist is `badarg` (the oracle's `function_clause`), never
        // a wrong rendering: an improper tail, a float element and an out-of-range
        // integer element.
        let improper = run_limited("~s", 5, |ctx| {
            // `[97,98|99]`.
            let tail = ctx.cons(ctx.int(98).expect("fits"), ctx.int(99).expect("fits"))
                .expect("fits");
            let list = ctx.cons(ctx.int(97).expect("fits"), tail).expect("fits");
            ctx.list(&[list]).expect("fits")
        });
        assert!(matches!(improper, Err(BeamError::BadArg)));
        let float = run_limited("~s", 5, |ctx| {
            // `[1.5]` — a float element.
            let f = ctx.make_float(1.5).expect("fits");
            let inner = ctx.cons(f, NIL).expect("fits");
            ctx.list(&[inner]).expect("fits")
        });
        assert!(matches!(float, Err(BeamError::BadArg)));
        let big = run_limited("~s", 5, |ctx| {
            // `[400]` — an integer outside the byte range.
            let inner = ctx.cons(ctx.int(400).expect("fits"), NIL).expect("fits");
            ctx.list(&[inner]).expect("fits")
        });
        assert!(matches!(big, Err(BeamError::BadArg)));

        // `~s` of an **atom** is a *small* control, so it stays supported: its
        // characters are spliced flat and no control is budgeted.
        let (text, shape) = run_limited("~s", 100, |ctx| {
            let atom = ctx.atom("abc").expect("seeded");
            ctx.list(&[atom]).expect("fits")
        })
        .expect("renders");
        assert_eq!(text, "abc");
        assert_eq!(shape, "[97,98,99]");
    }

    #[test]
    fn io_lib_format_2_agrees_with_io_format() {
        for format in ["~w", "~p", "~s", "~c", "~b", "~x", "~e", "~f", "~g", "~W", "~P"] {
            let (io_out, io_res) = run_io(format, |ctx| {
                let a = ctx.atom("a").expect("seeded");
                let one_ = ctx.int(1).expect("fits");
                let tuple = ctx.tuple(&[a, one_]).expect("fits");
                let prefix = byte_list(ctx, b"#");
                match format {
                    "~x" => Some(ctx.list(&[ctx.int(255).expect("fits"), prefix]).expect("fits")),
                    "~e" | "~f" | "~g" => {
                        let f = ctx.make_float(1.5).expect("fits");
                        Some(ctx.list(&[f]).expect("fits"))
                    }
                    "~W" | "~P" => {
                        Some(ctx.list(&[tuple, ctx.int(2).expect("fits")]).expect("fits"))
                    }
                    "~s" => Some(ctx.list(&[ctx.atom("abc").expect("seeded")]).expect("fits")),
                    _ => one(ctx, tuple),
                }
            });
            let (lib_out, lib_res) = run_io_lib(format, |ctx| {
                let a = ctx.atom("a").expect("seeded");
                let one_ = ctx.int(1).expect("fits");
                let tuple = ctx.tuple(&[a, one_]).expect("fits");
                let prefix = byte_list(ctx, b"#");
                match format {
                    "~x" => ctx.list(&[ctx.int(255).expect("fits"), prefix]).expect("fits"),
                    "~e" | "~f" | "~g" => {
                        let f = ctx.make_float(1.5).expect("fits");
                        ctx.list(&[f]).expect("fits")
                    }
                    "~W" | "~P" => {
                        ctx.list(&[tuple, ctx.int(2).expect("fits")]).expect("fits")
                    }
                    "~s" => ctx.list(&[ctx.atom("abc").expect("seeded")]).expect("fits"),
                    _ => ctx.list(&[tuple]).expect("fits"),
                }
            });
            assert_eq!(io_res.is_ok(), lib_res.is_ok(), "{format}");
            assert_eq!(io_out, lib_out, "{format}");
        }
    }

    // ---- error paths -----------------------------------------------------------

    #[test]
    fn argument_count_is_exact() {
        let (_, result) = run_io("~w ~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            one(ctx, a)
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        let (_, result) = run_io("~w", |ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            Some(ctx.list(&[a, b]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)));

        let (_, result) = run_io("abc~", |_ctx| Some(NIL));
        assert!(matches!(result, Err(BeamError::BadArg)));
    }

    #[test]
    fn unsupported_directives_are_badarg() {
        // `~5~~` is badarg in the oracle: after `~5~` the trailing `~` starts an
        // empty control sequence.
        for (format, arg) in [(0u8, 255i64), (1, 255), (2, 255)] {
            let _ = (format, arg);
        }
        // `~k w`/`~K w` stay badarg: the `k`/`K` modifier is accepted (K consuming
        // `255` as its order term), but the space that follows is an invalid control
        // character — the order is never interpreted, so the oracle agrees.
        for format in ["~5~~", "~-p", "~k w", "~K w", "~z"] {
            let (_, result) = run_io(format, |ctx| ints(ctx, &[255]));
            assert!(
                matches!(result, Err(BeamError::BadArg)),
                "format {format} should be badarg"
            );
        }
        // `~+w`/`~#w` are NOT badarg: `~+`/`~#` consume the argument and the
        // trailing `w` is a literal (`io_lib:format("~+w",[255])` ⇒ `"10#255w"`).
        let (output, result) = run_io("~+w", |ctx| ints(ctx, &[255]));
        assert!(result.is_ok());
        assert_eq!(output, "10#255w");
        let (output, result) = run_io("~#w", |ctx| ints(ctx, &[255]));
        assert!(result.is_ok());
        assert_eq!(output, "10#255w");
    }

    // ---- the `~k`/`~K` map-order modifiers -------------------------------------

    /// The seeded atoms the map-order tests need beyond [`SEED`]: the three built-in
    /// order atoms, a nested-map key and a bogus atom.
    const ORDER_SEED: [&str; 19] = [
        "true", "false", "ok", "hello", "a", "b", "c", "abc", "hi", "foo", "abcdefgh", "", "e",
        "chars_limit", "ordered", "reversed", "undefined", "x", "bogus",
    ];

    /// `#{hello=>1,a=>2,b=>3,c=>4}` with the **storage** order the writers produce —
    /// `cmp_flatmap_keys` (atom-index) order `hello, a, b, c`, which differs from the
    /// ascending **name** order `a, b, c, hello` an `ordered` render yields, so the
    /// two orders are distinguishable.
    fn order_map(ctx: &mut BifContext<'_>) -> Eterm {
        let hello = ctx.atom("hello").expect("seeded");
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let three = ctx.int(3).expect("fits");
        let four = ctx.int(4).expect("fits");
        let mut pairs = [(hello, one), (a, two), (b, three), (c, four)];
        pairs.sort_by(|k, l| beam_term::compare::cmp_flatmap_keys(k.0, l.0, ctx.atoms()));
        crate::access::make_flatmap(ctx.heap(), &pairs).expect("fits")
    }

    /// A `~K`/`~k` order is applied only when a map is rendered; `~k` takes the
    /// ascending `ordered` order, `~K` the built-in order named by its argument.
    #[test]
    fn map_order_modifiers_select_the_map_rendering_order() {
        // The default (no modifier): the map's stored order.
        let (output, result) = run_io_seeded(ORDER_SEED, "~p", |ctx| {
            let map = order_map(ctx);
            one(ctx, map)
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "#{hello => 1,a => 2,b => 3,c => 4}");

        // `~k` — ascending name order, on both the `~p` and the `~w` path.
        let (output, result) = run_io_seeded(ORDER_SEED, "~kp", |ctx| {
            let map = order_map(ctx);
            one(ctx, map)
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "#{a => 2,b => 3,c => 4,hello => 1}");
        let (output, result) = run_io_seeded(ORDER_SEED, "~kw", |ctx| {
            let map = order_map(ctx);
            one(ctx, map)
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "#{a => 2,b => 3,c => 4,hello => 1}");

        // `~K` with each built-in order atom, on the flat `~w` path.
        for (order, expected) in [
            ("ordered", "#{a => 2,b => 3,c => 4,hello => 1}"),
            ("reversed", "#{hello => 1,c => 4,b => 3,a => 2}"),
            ("undefined", "#{hello => 1,a => 2,b => 3,c => 4}"),
        ] {
            let (output, result) = run_io_seeded(ORDER_SEED, "~Kw", move |ctx| {
                let map = order_map(ctx);
                let atom = ctx.atom(order).expect("seeded");
                Some(ctx.list(&[atom, map]).expect("fits"))
            });
            assert!(result.is_ok(), "{order}: {result:?}");
            assert_eq!(output, expected, "{order}");
        }

        // The order follows `~W`'s depth form and a `~P` with an explicit depth.
        let (output, result) = run_io_seeded(ORDER_SEED, "~kW", |ctx| {
            let map = order_map(ctx);
            let depth = ctx.int(5).expect("fits");
            Some(ctx.list(&[map, depth]).expect("fits"))
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "#{a => 2,b => 3,c => 4,hello => 1,...}");
        let (output, result) = run_io_seeded(ORDER_SEED, "~kP", |ctx| {
            let map = order_map(ctx);
            let depth = ctx.int(5).expect("fits");
            Some(ctx.list(&[map, depth]).expect("fits"))
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "#{a => 2,b => 3,c => 4,hello => 1}");

        // The order applies to every map at any depth: the nested map is ordered too.
        let (output, result) = run_io_seeded(ORDER_SEED, "~kp", |ctx| {
            let inner = order_map(ctx);
            let x = ctx.atom("x").expect("seeded");
            let outer = crate::access::make_flatmap(ctx.heap(), &[(x, inner)]).expect("fits");
            one(ctx, outer)
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            output,
            "#{x => #{a => 2,b => 3,c => 4,hello => 1}}"
        );

        // An order with no map in the directive is simply unused.
        let (output, result) = run_io_seeded(ORDER_SEED, "~kb", |ctx| ints(ctx, &[255]));
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "255");
        let (output, result) = run_io_seeded(ORDER_SEED, "~Kp", |ctx| {
            let atom = ctx.atom("ordered").expect("seeded");
            let elems = [ctx.int(1).expect("fits"), ctx.int(2).expect("fits")];
            let list = ctx.list(&elems).expect("fits");
            Some(ctx.list(&[atom, list]).expect("fits"))
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "[1,2]");
    }

    /// The `~k`/`~K` modifier needs a following control character; the order term is
    /// validated only when a map is actually rendered.
    #[test]
    fn map_order_modifier_errors_match_the_oracle() {
        // No control character follows the modifier.
        let (_, result) = run_io_seeded(ORDER_SEED, "~k", |ctx| {
            let map = order_map(ctx);
            one(ctx, map)
        });
        assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");
        let (_, result) = run_io_seeded(ORDER_SEED, "~K", |ctx| {
            let map = order_map(ctx);
            one(ctx, map)
        });
        assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");

        // An invalid order term is `badarg` — but only once a map is reached.
        for order in OrderArg::rejected() {
            let (_, result) = run_io_seeded(ORDER_SEED, "~Kw", move |ctx| {
                let map = order_map(ctx);
                let term = order.build(ctx);
                Some(ctx.list(&[term, map]).expect("fits"))
            });
            assert!(matches!(result, Err(BeamError::BadArg)), "{order:?}: {result:?}");
        }

        // A **2-arity fun** is the oracle's order *comparator*; with no `FunCaller`
        // seam the printer answers the typed `unsupported_bif`, never a wrong order.
        let (_, result) = run_io_seeded(ORDER_SEED, "~Kw", |ctx| {
            let map = order_map(ctx);
            let fun = OrderArg::fun(ctx, 2);
            Some(ctx.list(&[fun, map]).expect("fits"))
        });
        assert!(
            matches!(&result, Err(BeamError::Raise(r)) if r.reason.as_atom() == Some("unsupported_bif")),
            "{result:?}"
        );

        // A **bogus order is never interpreted when no map is rendered**, so a
        // `~Kp` of a non-map prints it with no error (the oracle's own behaviour).
        let (output, result) = run_io_seeded(ORDER_SEED, "~Kp", |ctx| {
            let atom = ctx.atom("bogus").expect("seeded");
            let elems = [ctx.int(1).expect("fits"), ctx.int(2).expect("fits")];
            let list = ctx.list(&elems).expect("fits");
            Some(ctx.list(&[atom, list]).expect("fits"))
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "[1,2]");
    }

    /// An empty map is special on the two paths: `~p` never reaches the pair
    /// iterator (`print_length/7` answers `#{}` first), while `~w` calls
    /// `maps:iterator/2` eagerly — so a bogus order is `badarg` there and not under
    /// `~p`. Verified against the oracle.
    #[test]
    fn map_order_modifier_on_an_empty_map_follows_the_oracle() {
        let empty = |ctx: &mut BifContext<'_>| crate::access::make_flatmap(ctx.heap(), &[]).expect("fits");

        let (output, result) = run_io_seeded(ORDER_SEED, "~Kp", |ctx| {
            let map = empty(ctx);
            let atom = ctx.atom("bogus").expect("seeded");
            Some(ctx.list(&[atom, map]).expect("fits"))
        });
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(output, "#{}");

        let (_, result) = run_io_seeded(ORDER_SEED, "~Kw", |ctx| {
            let map = empty(ctx);
            let atom = ctx.atom("bogus").expect("seeded");
            Some(ctx.list(&[atom, map]).expect("fits"))
        });
        assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");
    }

    /// The `~K` order arguments a test needs — an integer, an empty list, a bogus
    /// atom and a fun of a given arity.
    #[derive(Clone, Copy, Debug)]
    enum OrderArg {
        Int(i64),
        EmptyList,
        Bogus,
        Fun(usize),
    }

    impl OrderArg {
        /// The order terms the oracle rejects with `badarg` (`maps:iterator/2`).
        fn rejected() -> [OrderArg; 4] {
            [
                OrderArg::Int(255),
                OrderArg::EmptyList,
                OrderArg::Bogus,
                OrderArg::Fun(1),
            ]
        }

        fn build(self, ctx: &mut BifContext<'_>) -> Eterm {
            match self {
                OrderArg::Int(v) => ctx.int(v).expect("fits"),
                OrderArg::EmptyList => NIL,
                OrderArg::Bogus => ctx.atom("bogus").expect("seeded"),
                OrderArg::Fun(arity) => Self::fun(ctx, arity),
            }
        }

        /// An `External` (`fun M:F/A`) fun of `arity` — the header carries the arity
        /// [`crate::access::fun_arity`] reads.
        fn fun(ctx: &mut BifContext<'_>, arity: usize) -> Eterm {
            let foo = atom_val(ctx.atom("foo").expect("seeded"));
            beam_codec::term::make_fun_external(ctx.heap(), arity, foo, foo).expect("fits")
        }
    }    #[test]
    fn printable_range_is_latin1_by_default() {
        // `io:printable_range/0` ⇒ `latin1`: the `ERL_PRINTABLE_CHARACTERS_LATIN1`
        // default (`erl_init.c:1379-1382`), which only `+pc unicode` upgrades, and
        // Milestone-1 has no flag plumbing — exactly what the oracle reports without
        // `+pc` (`erl -noshell -eval 'io:printable_range().'`).
        let mut heap = Heap::new(1024).expect("test heap");
        let mut atoms = AtomTable::new();
        // Both atoms are seeded, so a hard-coded `unicode` result could not pass by
        // accident (an unseeded name would be `TermNotFound`, not a wrong answer).
        atoms.seed(["latin1", "unicode"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let result = printable_range_0(&mut ctx, &[]).expect("latin1 is seeded");
        let latin1 = ctx.atom("latin1").expect("seeded");
        let unicode = ctx.atom("unicode").expect("seeded");
        assert_eq!(result, latin1);
        assert_ne!(result, unicode);
        assert!(crate::registry::default_registry().contains("io", "printable_range", 0));
    }

    #[test]
    fn field_value_and_adjust_helpers() {
        // `print.rs`'s `dup_chars` (the oracle's `chars/2`), `adjust_chunk`
        // (`adjust/3`) and `string_field_chunk` (`string/6`, `flat_trunc/3`),
        // asserted through their flattened text.
        let pad = || dup_chars('*', 3);
        assert_eq!(flatten(&[adjust_chunk(Chunk::text("x"), pad(), false)]), "***x");
        assert_eq!(flatten(&[adjust_chunk(Chunk::text("x"), pad(), true)]), "x***");
        assert_eq!(
            flatten(&[adjust_chunk(
                Chunk::text("x"),
                Chunk::List(Vec::new()),
                true
            )]),
            "x"
        );
        assert_eq!(flatten(&[dup_chars('0', 3)]), "000");
        assert_eq!(
            flatten(&[string_field_chunk(Chunk::text("hello"), Some(3), false, None, b' ')
                .expect("fits")]),
            "hel"
        );
    }
}
