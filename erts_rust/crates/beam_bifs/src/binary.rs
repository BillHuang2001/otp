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

//! The `binary` module BIFs (`binary:at/2`, `part/2,3`, `copy/1,2`,
//! `list_to_bin/1`, `bin_to_list/1,2,3`, `decode_unsigned/1,2`,
//! `encode_unsigned/1,2`, `first/1`, `last/1`, `compile_pattern/1`, `split/2,3`,
//! `match/2,3`, `matches/2,3`, `longest_common_prefix/1`,
//! `longest_common_suffix/1`) **and** the four `erlang` iolist/bitstring BIFs
//! built on the same C machinery — `iolist_to_binary/1`, `iolist_size/1` and
//! `list_to_bitstring/1` (`../../erts/emulator/beam/erl_iolist.c`) plus
//! `split_binary/2` (`../../erts/emulator/beam/binary.c:476`).
//!
//! Note the two distinct "split" BIFs: `binary:split/2,3` (a *pattern* split,
//! returning a list) and `erlang:split_binary/2` (a *position* split, returning a
//! `{Before, After}` tuple).
//!
//! Ground truth (read-only): [`../../erts/emulator/beam/erl_bif_binary.c`]
//! (`binary_at_2`, `erts_binary_part`, `do_binary_copy`, `start_longest_common`,
//! `binary_first_1`/`binary_last_1`, `do_encode_unsigned`/`do_decode_unsigned`,
//! `binary_match`, `binary_matches`, `binary_split`, `do_binary_match_compile`,
//! `parse_match_opts_list`/`parse_split_opts_list`) and the `bif.tab` roster
//! (`bif binary:at/2`, `bif binary:part/2 binary_binary_part_2`, `…`). Every
//! value below is pinned against the OTP 29 / erts-17.1 oracle.
//!
//! The roster is the `bif.tab` `binary` surface: `compile_pattern/1`,
//! `match/2,3`, `matches/2,3`, `longest_common_prefix/1`,
//! `longest_common_suffix/1`, `first/1`, `last/1`, `at/2`, `part/2,3`,
//! `list_to_bin/1`, `copy/1,2`, `referenced_byte_size/1`, `encode_unsigned/1,2`,
//! `decode_unsigned/1,2`, `split/2,3` (minus the documented gaps).
//!
//! Registers its BIFs through [`register`]; the shared heap-bitstring helpers live
//! in [`crate::bits`] and the "is this a proper list?" rule in [`crate::print`].
//!
//! # Pinned against the oracle
//!
//! * `at(<<1,2,3>>, 2)` ⇒ `3`; a `Pos` at or past `byte_size`, a negative or
//!   non-integer `Pos`, a non-binary subject and a non-byte-aligned bitstring are
//!   all `badarg` (`at(<<>>, 0)` ⇒ `badarg`).
//! * `part(<<1,2,3>>, {0,2})` ⇒ `<<1,2>>`, `part(<<1,2,3>>, 3, 0)` ⇒ `<<>>`;
//!   `part(<<1,2,3>>, {1,3})` (end past the binary), a negative `Start`,
//!   `part(<<1,2,3>>, {0})` (not a 2-tuple) ⇒ `badarg`.
//! * A **negative `Len`** is the reverse-part form `erts_binary_part` implements:
//!   `Len` bytes *ending* at `Start` (`part(<<1,2,3>>, 1, -1)` ⇒ `<<1>>`,
//!   `part(<<1,2,3>>, 2, -2)` ⇒ `<<1,2>>`); `part(<<1,2,3>>, 1, -3)` (start
//!   before 0) ⇒ `badarg`. `bin_to_list/3` and the `{scope, …}` option share this
//!   exact rule (all three call the same C helper / parse loop).
//! * `copy(<<1,2>>, 3)` ⇒ `<<1,2,1,2,1,2>>`, `copy(<<1,2>>, 0)` ⇒ `<<>>`,
//!   `copy(<<>>, 3)` ⇒ `<<>>`; a negative/float count ⇒ `badarg`.
//! * `list_to_bin([1,[2,[3]]])` ⇒ `<<1,2,3>>`, `list_to_bin([<<1,2>>,3])` ⇒
//!   `<<1,2,3>>`, `list_to_bin([])` ⇒ `<<>>`; a byte outside `0..=255`, a
//!   non-byte-aligned binary leaf, an improper tail, a non-list *top level*
//!   (`list_to_bin(<<1,2>>)` ⇒ `badarg`) ⇒ `badarg`. An improper cons tail that is
//!   a **bitstring** is a valid iolist (`list_to_bin([1|<<2>>])` ⇒ `<<1,2>>`, the
//!   `erl_iolist.c` cons-tail rule).
//! * `iolist_to_binary([<<"ab">>,[99],$d])` ⇒ `<<"abcd">>`,
//!   `iolist_to_binary([1|<<2>>])` ⇒ `<<1,2>>`, `iolist_to_binary(<<1,2,3>>)` ⇒
//!   `<<1,2,3>>` (a bare byte-aligned binary is accepted, unlike `list_to_bin/1`);
//!   an out-of-range byte, a bare atom/integer, a non-byte-aligned bitstring leaf
//!   (`[<<1:3>>]`) and a bare non-byte-aligned bitstring ⇒ `badarg`.
//! * `iolist_size([<<"ab">>,[99]])` ⇒ `3`, `iolist_size([1,2|<<3>>])` ⇒ `3`,
//!   `iolist_size(<<1,2,3>>)` ⇒ `3`; the same `badarg` set as `iolist_to_binary/1`.
//! * `list_to_bitstring([<<1:3>>])` ⇒ `<<1:3>>`,
//!   `list_to_bitstring([<<1:3>>,<<1:5>>])` ⇒ `<<"!">>`,
//!   `list_to_bitstring([<<1:3>>,255])` ⇒ `<<63,7:3>>` (bit-level concatenation);
//!   a non-list top level (`list_to_bitstring(<<1:3>>)`), a bad byte and a bad leaf
//!   ⇒ `badarg`.
//! * `split_binary(<<"abcd">>,2)` ⇒ `{<<"ab">>,<<"cd">>}`, `(_,0)` ⇒
//!   `{<<>>, <the whole binary>}`, `(_, ByteSize)` ⇒ `{<the whole binary>, <<>>}`;
//!   a non-binary subject, a non-integer/negative position and `Position >
//!   byte_size(Binary)` ⇒ `badarg` (and the bound is `bit_size / 8`, so
//!   `split_binary(<<1:9>>, 1)` ⇒ `{<<0>>, <<1:1>>}` while `(_, 2)` ⇒ `badarg`).
//! * `decode_unsigned(<<>>)` ⇒ `0`, `(<<1,0>>)` ⇒ `256`, `(<<1,0>>, little)` ⇒ `1`,
//!   `(<<0,0,1,0>>)` ⇒ `256` (leading zeros are stripped); a non-byte-aligned
//!   bitstring or a non-binary ⇒ `badarg`.
//! * `encode_unsigned(0)` ⇒ `<<0>>`, `(255)` ⇒ `<<255>>`, `(256)` ⇒ `<<1,0>>`,
//!   `(256, little)` ⇒ `<<0,1>>`; a negative or non-integer value ⇒ `badarg`.
//! * `first(<<1,2,3>>)` ⇒ `1`, `last(<<1,2,3>>)` ⇒ `3`; an empty binary ⇒ `badarg`.
//! * `split(<<"aXbXc">>, <<"X">>)` ⇒ `[<<"a">>,<<"bXc">>]` (the *first* match),
//!   `[global]` ⇒ `[<<"a">>,<<"b">>,<<"c">>]`, `[trim]`/`[trim_all]` drop the
//!   leading/trailing empty pieces exactly as `do_split_*_result` does.
//! * `match(<<"abcdef">>, <<"cd">>)` ⇒ `{2,2}`, a miss ⇒ `nomatch`; alternatives
//!   (`[<<"xy">>,<<"cd">>]`) pick the match with the **leftmost start**, and
//!   among alternatives matching at that start the **longest** (`match(<<"ab">>,
//!   [<<"a">>,<<"ab">>])` ⇒ `{0,2}`). An empty alternative ⇒ `badarg`.
//! * `matches(<<"abcabcab">>, <<"ab">>)` ⇒ `[{0,2},{3,2},{6,2}]` (every
//!   non-overlapping match, leftmost-longest); no match ⇒ `[]` (not `nomatch`).
//!   An empty pattern and a non-list option ⇒ `badarg`.
//! * `compile_pattern(Pattern)`: `Pattern` is a **non-empty byte-aligned binary**
//!   or a **non-empty list** of them (an integer or a list of integers is
//!   `badarg` in erts-17.1, despite older docs). **One** search string ⇒
//!   `{bm, Ref}`; **more than one** ⇒ `{ac, Ref}` — the C `words == 1` test
//!   (`am_bm`/`am_ac`). An empty binary, a non-byte-aligned binary, an empty
//!   list, a non-binary element, an improper list and any other argument are
//!   `badarg`. `match/2,3`, `matches/2,3` and `split/2,3` accept a compiled
//!   `{bm,Ref}`/`{ac,Ref}` in place of the raw pattern with identical results; a
//!   forged/unknown `Ref`, a `{bm, Ref}` whose `Ref` was compiled as `ac` (and
//!   vice versa), and any other 2-tuple are `badarg` (`maybe_binary_match_compile`
//!   + the magic-ref destructor check).
//! * `longest_common_prefix([<<"abc">>,<<"abd">>])` ⇒ `2`,
//!   `longest_common_suffix([<<"abc">>,<<"abd">>])` ⇒ `0`; an empty list, a
//!   non-binary element and a non-list argument ⇒ `badarg`.
//!
//! # Documented Milestone-1 gaps (never a silently wrong result)
//!
//! * **The endian atom is `big` or `little` only.** The C BIFs compare against
//!   `am_big`/`am_little`, so `native` is `badarg` in real erts-17.1 (verified)
//!   even though `binary.erl`'s doc lists it; this crate matches the oracle.
//! * **Compiled patterns live in a process-global registry, never collected.**
//!   `compile_pattern/1` mints a fresh `Ref` thing ([`access::make_ref_thing`];
//!   the M1 substitution for the C magic-ref binary) and stores the compiled
//!   alternatives in a module-local registry keyed by the ref's numeric id.
//!   Real ERTS frees a compiled pattern when its magic ref is garbage-collected;
//!   Milestone-1 has no off-heap magic ref and no GC for this registry, so an
//!   entry lives for the process's lifetime (a bounded, monotonic leak). The
//!   registry is process-global because [`BifContext`] exposes no seam for it —
//!   see the module's `compiled_patterns` section.
//! * **`referenced_byte_size/1` is not registered** (it needs the off-heap
//!   `BinRef` of a sub-binary, which Milestone-1 does not build; every result is
//!   a fresh on-heap binary).
//! * **`decode_unsigned`/`encode_unsigned` span the full integer range.**
//!   `decode_unsigned` accepts any width and promotes a value past the 60-bit
//!   small range to a bignum (the oracle's behaviour); `encode_unsigned` accepts a
//!   bignum and emits its minimal byte image. Only a magnitude beyond the bignum
//!   limb bound is `system_limit`.
//! * **Sub-binaries are copied, not referenced.** `erts_binary_part` returns a
//!   `SUB_BITS` sharing the source; Milestone-1 has no off-heap `BinRef`, so
//!   every result is a fresh on-heap binary (`crate::bits::make_binary`).
//! * **`binary:copy/2` cannot outgrow the fixed-size test heap**: a result larger
//!   than the destination heap can hold reports `system_limit` rather than
//!   allocating without bound (the same convention `convert::make_tuple` uses).
//! * **`bin_to_list/1,2,3` are Erlang functions in OTP** (`lib/stdlib/src/
//!   binary.erl`, wrapping `binary_to_list/1,3`); they are registered here as
//!   BIFs so the runtime can resolve the documented call set, with the
//!   `binary.erl` wrapper's observable behaviour (verified against the oracle for
//!   every case below). They are *not* `bif.tab` entries.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use beam_codec::term as codec_term;
use beam_common::Result;
use beam_term::{atom_val, signed_value, Eterm};

use crate::access;
use crate::arith;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, system_limit};
use crate::print::proper_list;
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// Argument decoding
// ---------------------------------------------------------------------------

/// `term_to_Uint`: a non-negative integer (Milestone-1 has only 60-bit smalls, so
/// a bignum — which real Erlang accepts — is `None`).
fn as_uint(term: Eterm) -> Option<u64> {
    if access::is_small(term) {
        u64::try_from(signed_value(term)).ok()
    } else {
        None
    }
}

/// `term_to_Sint`: an integer of either sign (a float is not one).
fn as_sint(term: Eterm) -> Option<i64> {
    if access::is_small(term) {
        Some(signed_value(term))
    } else {
        None
    }
}

/// Whether `term` is the atom `name`.
fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// Validates the `[Start, Start + Len)` byte range against a buffer of `size`
/// bytes, exactly like `erts_binary_part`
/// (`erts/emulator/beam/erl_bif_binary.c:2137`): a negative `Len` is the
/// reverse-part form (`Len` bytes *ending* at `Start`), and a range that is
/// negative or reaches past the end is `badarg`.
fn checked_range(size: usize, epos: Eterm, elen: Eterm) -> Result<(usize, usize)> {
    let mut start = as_uint(epos).ok_or_else(badarg)?;
    let mut len = as_sint(elen).ok_or_else(badarg)?;
    if len < 0 {
        // `-(i64::MIN)` overflows: `checked_neg` reports that as `badarg`.
        let back = u64::try_from(len.checked_neg().ok_or_else(badarg)?).map_err(|_| badarg())?;
        if back > start {
            return Err(badarg());
        }
        len = -len;
        start -= back;
    }
    let len = u64::try_from(len).map_err(|_| badarg())?;
    let end = start.checked_add(len).ok_or_else(badarg)?;
    if end > size as u64 {
        return Err(badarg());
    }
    Ok((start as usize, len as usize))
}

/// The validated byte slice of `bytes` (see [`checked_range`]).
fn checked_slice<'a>(bytes: &'a [u8], epos: Eterm, elen: Eterm) -> Result<&'a [u8]> {
    let (start, len) = checked_range(bytes.len(), epos, elen)?;
    Ok(&bytes[start..start + len])
}

/// Parses the `{scope, {Pos, Len}}` option, returning the `[start, end)` byte
/// range to search within (the C `parse_match_opts_list` inner block).
fn parse_scope(ctx: &BifContext<'_>, term: Eterm, size: usize) -> Result<(usize, usize)> {
    if !access::is_tuple(term) || access::tuple_size(term) != 2 {
        return Err(badarg());
    }
    if !is_atom_named(ctx, access::tuple_element(term, 0), "scope") {
        return Err(badarg());
    }
    let range = access::tuple_element(term, 1);
    if !access::is_tuple(range) || access::tuple_size(range) != 2 {
        return Err(badarg());
    }
    let (start, len) = checked_range(
        size,
        access::tuple_element(range, 0),
        access::tuple_element(range, 1),
    )?;
    // `checked_range` validated `Start + Len <= size`, so this is the `end`.
    Ok((start, start + len))
}

/// The `{scope, …}` range of `match/3`'s options (or the whole binary).
///
/// The C loop is a `do … while (is_list(l))` over the option list, so an improper
/// tail is *ignored* — reproduced here rather than normalised.
fn match_options(
    ctx: &BifContext<'_>,
    opts: Option<Eterm>,
    size: usize,
) -> Result<(usize, usize)> {
    let Some(opts) = opts else {
        return Ok((0, size));
    };
    if access::is_nil(opts) {
        return Ok((0, size));
    }
    if !access::is_list(opts) {
        return Err(badarg());
    }
    let mut range = (0, size);
    let mut cur = opts;
    while access::is_list(cur) {
        range = parse_scope(ctx, access::car(cur), size)?;
        cur = access::cdr(cur);
    }
    Ok(range)
}

/// The parsed `split/3` options (`parse_split_opts_list`).
#[derive(Debug, Clone, Copy)]
struct SplitOptions {
    global: bool,
    trim: bool,
    trim_all: bool,
    start: usize,
    end: usize,
}

impl SplitOptions {
    /// Whether an empty piece is dropped (`BF_FLAG_SPLIT_TRIM*`).
    fn trimming(&self) -> bool {
        self.trim || self.trim_all
    }
}

/// Parses `split/3`'s options list: `global`, `trim`, `trim_all`, `{scope, …}`
/// (the last `{scope, …}` wins), with an improper tail ignored as in C.
fn split_options(
    ctx: &BifContext<'_>,
    opts: Option<Eterm>,
    size: usize,
) -> Result<SplitOptions> {
    let mut options = SplitOptions { global: false, trim: false, trim_all: false, start: 0, end: size };
    let Some(opts) = opts else {
        return Ok(options);
    };
    if access::is_nil(opts) {
        return Ok(options);
    }
    if !access::is_list(opts) {
        return Err(badarg());
    }
    let mut cur = opts;
    while access::is_list(cur) {
        let item = access::car(cur);
        if is_atom_named(ctx, item, "global") {
            options.global = true;
        } else if is_atom_named(ctx, item, "trim") {
            options.trim = true;
        } else if is_atom_named(ctx, item, "trim_all") {
            options.trim_all = true;
        } else {
            let (start, end) = parse_scope(ctx, item, size)?;
            options.start = start;
            options.end = end;
        }
        cur = access::cdr(cur);
    }
    Ok(options)
}

/// The endianness atoms `big`/`little` `do_encode_unsigned`/`do_decode_unsigned`
/// accept (`native` is `badarg` in the oracle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Endian {
    Big,
    Little,
}

/// Decodes the second argument of `decode_unsigned/2` / `encode_unsigned/2`.
fn endian_of(ctx: &BifContext<'_>, term: Eterm) -> Result<Endian> {
    if !access::is_atom(term) {
        return Err(badarg());
    }
    match ctx.atoms().name(atom_val(term)) {
        Some("big") => Ok(Endian::Big),
        Some("little") => Ok(Endian::Little),
        _ => Err(badarg()),
    }
}

/// The search alternatives of a `Pattern` argument, whether it is a raw pattern
/// (a binary or a list of binaries) or a compiled `{bm, Ref}`/`{ac, Ref}` the
/// oracle's `binary_match`/`binary_split` accept (C `maybe_binary_match_compile`).
///
/// The C dispatch is "a tuple is a compiled pattern, anything else goes through
/// `do_binary_match_compile`", so a tuple that is not a valid compiled pattern is
/// `badarg` — never re-interpreted as a raw pattern.
fn parse_pattern(ctx: &BifContext<'_>, pattern: Eterm) -> Result<Vec<Vec<u8>>> {
    if access::is_tuple(pattern) {
        return lookup_compiled(ctx, pattern);
    }
    parse_raw_pattern(pattern)
}

/// A raw pattern: one or more alternative non-empty byte strings
/// (`do_binary_match_compile`).
fn parse_raw_pattern(pattern: Eterm) -> Result<Vec<Vec<u8>>> {
    // `bits::bytes` rejects a non-byte-aligned bitstring, matching the C
    // `TAIL_BITS(size) != 0` check.
    if bits::is_bitstring(pattern) {
        let bytes = bits::bytes(pattern)?;
        if bytes.is_empty() {
            return Err(badarg());
        }
        return Ok(vec![bytes]);
    }
    let items = proper_list(pattern).ok_or_else(badarg)?;
    if items.is_empty() {
        return Err(badarg());
    }
    let mut alternatives = Vec::with_capacity(items.len());
    for item in items {
        let bytes = bits::bytes(item)?;
        if bytes.is_empty() {
            return Err(badarg());
        }
        alternatives.push(bytes);
    }
    Ok(alternatives)
}

// ---------------------------------------------------------------------------
// Compiled patterns (`binary:compile_pattern/1`, `do_binary_match_compile` +
// `maybe_binary_match_compile`)
// ---------------------------------------------------------------------------

/// The two compiled-pattern flavours `do_binary_match_compile` produces: `bm`
/// (a single search string — the Boyer-Moore fast path, C `am_bm`) and `ac`
/// (two or more search strings — the Aho-Corasick trie, C `am_ac`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternTag {
    Bm,
    Ac,
}

impl PatternTag {
    /// The tag atom of the compiled `{Tag, Ref}` tuple.
    fn atom(self) -> &'static str {
        match self {
            PatternTag::Bm => "bm",
            PatternTag::Ac => "ac",
        }
    }
}

/// A compiled search pattern: its tag and the alternative non-empty byte strings
/// (exactly the inputs the search primitives take, so nothing else needs to know
/// a pattern was compiled).
#[derive(Debug, Clone)]
struct CompiledPattern {
    tag: PatternTag,
    alternatives: Vec<Vec<u8>>,
}

/// The process-global compiled-pattern registry.
///
/// # Why a module-local static
///
/// [`BifContext`] exposes no seam for compiled patterns, and `binary:compile_pattern/1`
/// must return an opaque handle that a *later* BIF call (with a fresh context, and
/// possibly a different process) resolves — precisely the object the C keeps alive
/// through a magic-ref binary's refcount. Where the C uses a refcounted off-heap
/// object, Milestone-1 (which has no off-heap magic ref and no per-object GC for
/// one) uses a process-global registry keyed by the ref's numeric id. The crate
/// already carries process-global state the same way (`time.rs`'s monotonic
/// origin and unique-integer counter, `beam_ets`' table-id counter), so this adds
/// no new pattern.
///
/// Ids start at [`ID_BASE`] so they can never collide with the small ids the
/// `ets` table registry hands out ([`crate::ets`]) — both handle families share
/// the `Ref` thing's single id word, and a disjoint range keeps a forged
/// `{bm, Tid}` from resolving against an unrelated table.
fn compiled_patterns() -> &'static Mutex<CompiledPatterns> {
    static REGISTRY: OnceLock<Mutex<CompiledPatterns>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(CompiledPatterns::new()))
}

/// The first id a compiled pattern is assigned (see [`compiled_patterns`]).
const ID_BASE: u64 = 1 << 32;

/// The compiled patterns minted so far, keyed by the ref id that names them.
#[derive(Debug)]
struct CompiledPatterns {
    next_id: u64,
    entries: HashMap<u64, CompiledPattern>,
}

impl CompiledPatterns {
    fn new() -> Self {
        Self { next_id: ID_BASE, entries: HashMap::new() }
    }
}

/// Locks the registry, recovering a poisoned mutex rather than panicking.
///
/// A panic in an unrelated BIF must not permanently disable compiled patterns:
/// the registry holds no invariant a panic could break (ids are never reused and
/// an entry is immutable once inserted), so the guard is recovered with
/// [`PoisonError::into_inner`] — never a `unwrap`.
fn lock_patterns() -> MutexGuard<'static, CompiledPatterns> {
    compiled_patterns().lock().unwrap_or_else(PoisonError::into_inner)
}

/// `compile_pattern/1`: builds the `{bm, Ref}`/`{ac, Ref}` handle
/// (`binary_compile_pattern_1` over `do_binary_match_compile`).
fn compile_pattern_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (tag, alternatives) = compile_pattern_arg(args[0])?;
    intern_compiled(ctx, tag, alternatives)
}

/// Decodes `compile_pattern/1`'s argument into `(tag, alternatives)` — the C
/// `do_binary_match_compile`, including its `words`/`characters` rules.
///
/// The tag is `bm` iff there is exactly **one** search string (`words == 1`);
/// anything else that yields at least one string is `ac`. The cases the C sends
/// to its `badarg` label are: a list element that is not a byte-aligned binary, a
/// zero-length search string (`characters` never grows), an improper list, and an
/// argument that is neither a list nor a bitstring (an integer, tuple, atom …,
/// which leaves `characters == 0`).
fn compile_pattern_arg(pattern: Eterm) -> Result<(PatternTag, Vec<Vec<u8>>)> {
    if bits::is_bitstring(pattern) {
        let bytes = bits::bytes(pattern)?;
        if bytes.is_empty() {
            return Err(badarg());
        }
        return Ok((PatternTag::Bm, vec![bytes]));
    }
    let items = proper_list(pattern).ok_or_else(badarg)?;
    let mut alternatives = Vec::with_capacity(items.len());
    for item in items {
        let bytes = bits::bytes(item)?;
        if bytes.is_empty() {
            return Err(badarg());
        }
        alternatives.push(bytes);
    }
    match alternatives.len() {
        0 => Err(badarg()),
        1 => Ok((PatternTag::Bm, alternatives)),
        _ => Ok((PatternTag::Ac, alternatives)),
    }
}

/// Mints a fresh `Ref` thing and records `tag`/`alternatives` under its id,
/// returning the `{Tag, Ref}` tuple the oracle returns.
///
/// The tag atom is resolved with [`BifContext::intern_atom`] rather than
/// [`BifContext::atom`]: `bm`/`ac` are the C's pre-interned `am_bm`/`am_ac`, and a
/// runtime that has not seeded them still gets them (through the `AtomInterner`
/// seam) instead of a spurious `TermNotFound` — the same on-demand intern the
/// `nif`/`error` modules already do for their reason atoms. An already-interned
/// name never needs the seam, so the common path costs nothing.
///
/// The id is reserved (and the ref built) *before* the entry is inserted, so a
/// pattern that fails to construct — an exhausted id space, a heap that cannot
/// fit the ref, an uninternable tag — leaves no orphan entry, and a
/// reserved-but-never-inserted id is simply skipped (ids are never reused).
fn intern_compiled(
    ctx: &mut BifContext<'_>,
    tag: PatternTag,
    alternatives: Vec<Vec<u8>>,
) -> Result<Eterm> {
    let id = {
        let mut registry = lock_patterns();
        let id = registry.next_id;
        registry.next_id = id.checked_add(1).ok_or_else(system_limit)?;
        id
    };
    let ref_term = access::make_ref_thing(ctx.heap(), id)?;
    let tag_term = ctx.intern_atom(tag.atom())?;
    let handle = ctx.tuple(&[tag_term, ref_term])?;
    lock_patterns().entries.insert(id, CompiledPattern { tag, alternatives });
    Ok(handle)
}

/// Resolves a compiled `{Tag, Ref}` handle to its alternatives
/// (`maybe_binary_match_compile`): the argument must be a `{Tag, Ref}` tuple with
/// an atom tag, and `Ref` a live handle of the *matching* kind.
///
/// A `Ref` the registry does not know (a forged handle), a handle compiled with
/// the other tag, a `Ref` that is not a ref thing at all, and any tuple that is
/// not a `{bm|ac, _}` pair are all `badarg` — the oracle's `is_internal_magic_ref`
/// + magic-ref-destructor check.
fn lookup_compiled(ctx: &BifContext<'_>, pattern: Eterm) -> Result<Vec<Vec<u8>>> {
    if access::tuple_size(pattern) != 2 {
        return Err(badarg());
    }
    let tag_term = access::tuple_element(pattern, 0);
    if !access::is_atom(tag_term) {
        return Err(badarg());
    }
    let tag = match ctx.atoms().name(atom_val(tag_term)) {
        Some("bm") => PatternTag::Bm,
        Some("ac") => PatternTag::Ac,
        _ => return Err(badarg()),
    };
    let ref_id = access::ref_thing_id(access::tuple_element(pattern, 1))?;
    let registry = lock_patterns();
    let compiled = registry.entries.get(&ref_id).ok_or_else(badarg)?;
    if compiled.tag != tag {
        return Err(badarg());
    }
    Ok(compiled.alternatives.clone())
}

// ---------------------------------------------------------------------------
// The search itself (leftmost start, then longest — `bin`'s BM/AC first match)
// ---------------------------------------------------------------------------

/// The first match of `alternatives` inside `subject[start..end)`: the match with
/// the **leftmost start**, and among alternatives matching there the **longest**.
///
/// This is the observable result of both `bm_find_first_match` (a single
/// alternative) and `ac_find_first_match` (the alternative set).
fn find_first(
    subject: &[u8],
    alternatives: &[Vec<u8>],
    start: usize,
    end: usize,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for alternative in alternatives {
        let len = alternative.len();
        if len == 0 || len > end.saturating_sub(start) {
            continue;
        }
        let last = end - len;
        let mut j = start;
        while j <= last {
            if &subject[j..j + len] == alternative.as_slice() {
                let better = match best {
                    None => true,
                    Some((best_pos, best_len)) => j < best_pos || (j == best_pos && len > best_len),
                };
                if better {
                    best = Some((j, len));
                }
                // Only the leftmost occurrence of *this* alternative can matter.
                break;
            }
            j += 1;
        }
    }
    best
}

/// Every non-overlapping match within `subject[start..end)`, in order (the
/// `ac`/`bm` `*_find_all_non_overlapping` result `[global]` split consumes).
fn find_all(subject: &[u8], alternatives: &[Vec<u8>], start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    let mut pos = start;
    while pos <= end {
        match find_first(subject, alternatives, pos, end) {
            Some((match_pos, len)) => {
                matches.push((match_pos, len));
                pos = match_pos + len;
            }
            None => break,
        }
    }
    matches
}

// ---------------------------------------------------------------------------
// The BIFs
// ---------------------------------------------------------------------------

/// `at(Binary, Pos)` — the byte at the 0-based `Pos`.
fn at(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = bits::bytes(args[0])?;
    let index = usize::try_from(as_uint(args[1]).ok_or_else(badarg)?).map_err(|_| badarg())?;
    let byte = *bytes.get(index).ok_or_else(badarg)?;
    ctx.int(i64::from(byte))
}

/// `part(Binary, {Start, Len})`.
fn part_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = args[1];
    if !access::is_tuple(range) || access::tuple_size(range) != 2 {
        return Err(badarg());
    }
    part_slice(ctx, args[0], access::tuple_element(range, 0), access::tuple_element(range, 1))
}

/// `part(Binary, Start, Len)`.
fn part_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    part_slice(ctx, args[0], args[1], args[2])
}

/// The shared body of `part/2,3`: copy the validated sub-binary to the heap.
fn part_slice(ctx: &mut BifContext<'_>, subject: Eterm, epos: Eterm, elen: Eterm) -> Result<Eterm> {
    let bytes = bits::bytes(subject)?;
    let slice = checked_slice(&bytes, epos, elen)?;
    bits::make_binary(ctx.heap(), slice)
}

/// `copy(Binary)`.
fn copy_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_copy(ctx, args[0], 1)
}

/// `copy(Binary, N)`.
fn copy_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let count = as_uint(args[1]).ok_or_else(badarg)?;
    do_copy(ctx, args[0], count)
}

/// `do_binary_copy`: `N` concatenated copies (`N == 0` or an empty source is
/// `<<>>`).
fn do_copy(ctx: &mut BifContext<'_>, subject: Eterm, count: u64) -> Result<Eterm> {
    let source = bits::bytes(subject)?;
    if count == 0 || source.is_empty() {
        return bits::make_binary(ctx.heap(), &[]);
    }
    let total = (source.len() as u64).checked_mul(count).ok_or_else(system_limit)?;
    let total = usize::try_from(total).map_err(|_| system_limit())?;
    // Milestone-1 heaps are fixed-size: a result the destination cannot hold is
    // `system_limit` (the `convert::make_tuple` convention), never an unbounded
    // allocation.
    if total > ctx.heap().available().saturating_mul(8) {
        return Err(system_limit());
    }
    let mut out = Vec::with_capacity(total);
    for _ in 0..count {
        out.extend_from_slice(&source);
    }
    bits::make_binary(ctx.heap(), &out)
}

/// `list_to_bin(List)` — an iolist flattened into a binary.
fn list_to_bin_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // `list_to_bitstring` requires a list at the top level: a bare binary is
    // `badarg` (`list_to_bin(<<1,2>>)` ⇒ `badarg`).
    let bytes = collect_byte_unit(args[0])?;
    bits::make_binary(ctx.heap(), &bytes)
}

// ---------------------------------------------------------------------------
// The `erlang` iolist family (`iolist_to_binary/1`, `iolist_size/1`,
// `list_to_bitstring/1`) and `erlang:split_binary/2`
// ---------------------------------------------------------------------------

/// The unit of an iolist walk (`erl_iolist.c`'s `Unit`).
///
/// * `Byte` (`Unit = 8`) — `erlang:iolist_to_binary/1` and
///   `erlang:iolist_size/1`: every bitstring leaf must be **byte aligned** and the
///   result is a binary.
/// * `Bit` (`Unit = 1`) — `erlang:list_to_bitstring/1`: a leaf may be a bitstring
///   of *any* bit size and the pieces are concatenated bit by bit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Byte,
    Bit,
}

/// A growable MSB-first bit buffer.
///
/// The packed form is exactly what [`crate::bits::make_bitstring`] consumes: bit
/// `i` is bit `7 - (i % 8)` of `bytes[i / 8]`, the bits after `len` in the final
/// byte stay zero, and `bytes.len() == len.div_ceil(8)`.
struct BitBuf {
    bytes: Vec<u8>,
    len: usize,
}

impl BitBuf {
    fn new() -> Self {
        Self { bytes: Vec::new(), len: 0 }
    }

    /// Appends one bit.
    fn push(&mut self, bit: bool) {
        if self.len % 8 == 0 {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> (self.len % 8);
        }
        self.len += 1;
    }

    /// Appends a whole byte (8 bits).
    fn push_byte(&mut self, byte: u8) {
        if self.len % 8 == 0 {
            self.bytes.push(byte);
            self.len += 8;
        } else {
            for shift in (0..8).rev() {
                self.push((byte >> shift) & 1 == 1);
            }
        }
    }

    /// Appends the `size_bits` bits of `bytes` starting at bit `offset`.
    ///
    /// `bytes` is the storage **byte** slice and `offset` the bit offset into it
    /// (the shape [`beam_codec::term::bitstring_bytes`] returns), so bit `i` of the
    /// appended run is bit `7 - ((offset + i) % 8)` of `bytes[(offset + i) / 8]`.
    fn push_bitstring(&mut self, bytes: &[u8], offset: usize, size_bits: usize) {
        if offset % 8 == 0 && size_bits % 8 == 0 && self.len % 8 == 0 {
            let first = offset / 8;
            self.bytes.extend_from_slice(&bytes[first..first + size_bits / 8]);
            self.len += size_bits;
        } else {
            for i in 0..size_bits {
                let bit = offset + i;
                self.push((bytes[bit / 8] >> (7 - (bit % 8))) & 1 == 1);
            }
        }
    }
}

/// Flattens an iolist into `buf`, rejecting an invalid element with `badarg`.
///
/// The walk mirrors `erl_iolist.c`'s `generic_iolist_size`/`generic_iolist_copy`:
/// a list element is a byte (`0..=255`), a bitstring, or a nested list; a cons
/// **tail** may also be `[]`, a cons, or a bitstring (`[1|<<2>>]` is a valid
/// iolist), and anything else is `badarg`. Under [`Unit::Byte`] a bitstring leaf
/// whose bit size is not a whole number of bytes is `badarg`
/// (`iolist_to_binary([<<1:3>>])`); under [`Unit::Bit`] any bit size is accepted.
///
/// The walk uses an explicit work stack, so a deeply nested list cannot overflow
/// the Rust stack.
fn collect_iolist_units(root: Eterm, unit: Unit, buf: &mut BitBuf) -> Result<()> {
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
            buf.push_byte(byte as u8);
            continue;
        }
        if bits::is_bitstring(term) {
            if unit == Unit::Byte && bits::bit_size(term)? % 8 != 0 {
                return Err(badarg());
            }
            let (bytes, offset, size_bits) = codec_term::bitstring_bytes(term)?;
            buf.push_bitstring(bytes, offset, size_bits);
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
    Ok(())
}

/// Flattens a [`Unit::Byte`] iolist whose top level must be a list or `[]` — the
/// shared body of `binary:list_to_bin/1` and `erlang:iolist_to_binary/1`.
///
/// Every element of a `Byte`-unit iolist is a whole number of bytes, so the packed
/// buffer's bytes are the result's bytes verbatim.
fn collect_byte_unit(root: Eterm) -> Result<Vec<u8>> {
    if !access::is_nil(root) && !access::is_list(root) {
        return Err(badarg());
    }
    let mut buf = BitBuf::new();
    collect_iolist_units(root, Unit::Byte, &mut buf)?;
    Ok(buf.bytes)
}

/// `iolist_to_binary(IoListOrBinary)` (`erl_iolist.c:916`).
///
/// Unlike `erlang:list_to_binary/1`, a **bare** byte-aligned binary is a valid
/// argument (`iolist_to_binary(<<1,2,3>>)` ⇒ `<<1,2,3>>`); a bare *non-byte-aligned*
/// bitstring is `badarg`.
fn iolist_to_binary_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let root = args[0];
    if bits::is_bitstring(root) {
        if bits::bit_size(root)? % 8 != 0 {
            return Err(badarg());
        }
        return Ok(root);
    }
    let bytes = collect_byte_unit(root)?;
    bits::make_binary(ctx.heap(), &bytes)
}

/// `iolist_size(IoListOrBinary)` (`erl_iolist.c:1084`) — the byte size of the
/// binary `iolist_to_binary/1` would build.
///
/// Both the argument rules and the `badarg` cases are `iolist_to_binary/1`'s (a
/// bare byte-aligned binary is accepted, a non-byte-aligned one is `badarg`). The
/// result is a plain **integer**: a size past the 60-bit small range becomes the
/// heap bignum the oracle's C `BUILD_INTEGER` produces, never a `system_limit`.
fn iolist_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let root = args[0];
    let size: u64 = if bits::is_bitstring(root) {
        let size_bits = bits::bit_size(root)?;
        if size_bits % 8 != 0 {
            return Err(badarg());
        }
        size_bits as u64 / 8
    } else {
        collect_byte_unit(root)?.len() as u64
    };
    byte_count_term(ctx, size)
}

/// The integer term for a non-negative byte count: a small int while it fits,
/// otherwise a heap bignum — both built by the shared `arith` promotion helpers
/// ([`arith::int_from_i64`] folds, [`arith::int_from_mag`] takes the magnitude
/// limbs little-endian), never by a local bignum construction.
fn byte_count_term(ctx: &mut BifContext<'_>, size: u64) -> Result<Eterm> {
    match i64::try_from(size) {
        Ok(small) => arith::int_from_i64(ctx, small),
        // A byte count above `i64::MAX`: one little-endian magnitude limb.
        Err(_) => arith::int_from_mag(ctx, false, &[size]),
    }
}

/// `list_to_bitstring(List)` (`erl_iolist.c:930`) — the `Unit = 1` variant of
/// `list_to_binary/1`: a leaf may be a bitstring of any bit size and the pieces are
/// concatenated bit by bit (`list_to_bitstring([<<1:3>>, <<1:5>>])` ⇒ `<<"!">>`,
/// `list_to_bitstring([<<1:3>>, 255])` ⇒ `<<63,7:3>>`).
///
/// The top level must be a list or `[]` (`list_to_bitstring(<<1:3>>)` ⇒ `badarg`),
/// and an invalid leaf is `badarg` exactly as for the `Byte` unit.
fn list_to_bitstring_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let root = args[0];
    if !access::is_nil(root) && !access::is_list(root) {
        return Err(badarg());
    }
    let mut buf = BitBuf::new();
    collect_iolist_units(root, Unit::Bit, &mut buf)?;
    bits::make_bitstring(ctx.heap(), &buf.bytes, buf.len)
}

/// `split_binary(Binary, Position)` (`binary.c:476`) — `{Before, After}` where
/// `Before` holds the first `Position` bytes and `After` the remainder.
///
/// `Position` is a whole **byte** count and must satisfy
/// `Position =< bit_size(Binary) / 8` (the C `term_to_Uint` + `size / 8` bound), so
/// `Before` is always byte aligned while `After` may keep a trailing partial byte
/// (`split_binary(<<1:9>>, 1)` ⇒ `{<<0>>, <<1:1>>}`). A non-bitstring subject, a
/// non-integer/negative `Position` and an out-of-range `Position` are all `badarg`.
fn split_binary_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !bits::is_bitstring(args[0]) {
        return Err(badarg());
    }
    // `term_to_Uint`: a non-integer (a float, an atom, a binary, …) is `badarg`;
    // a negative integer fails the conversion too.
    let Some(split_at) = as_uint(args[1]) else {
        return Err(badarg());
    };
    let size_bits = bits::bit_size(args[0])?;
    if split_at > (size_bits / 8) as u64 {
        return Err(badarg());
    }
    let split_bits = (split_at as usize) * 8;
    let (bytes, offset, _) = codec_term::bitstring_bytes(args[0])?;
    let mut before = BitBuf::new();
    before.push_bitstring(bytes, offset, split_bits);
    let mut after = BitBuf::new();
    after.push_bitstring(bytes, offset + split_bits, size_bits - split_bits);
    let before = bits::make_bitstring(ctx.heap(), &before.bytes, before.len)?;
    let after = bits::make_bitstring(ctx.heap(), &after.bytes, after.len)?;
    ctx.tuple(&[before, after])
}

/// Builds the list `[B0, B1, …]` of the bytes in `bytes`.
fn bytes_to_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// `bin_to_list(Binary)`.
fn bin_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = bits::bytes(args[0])?;
    bytes_to_list(ctx, &bytes)
}

/// `bin_to_list(Binary, {Start, Len})`.
fn bin_to_list_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = args[1];
    if !access::is_tuple(range) || access::tuple_size(range) != 2 {
        return Err(badarg());
    }
    bin_to_list_slice(
        ctx,
        args[0],
        access::tuple_element(range, 0),
        access::tuple_element(range, 1),
    )
}

/// `bin_to_list(Binary, Start, Len)`.
fn bin_to_list_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    bin_to_list_slice(ctx, args[0], args[1], args[2])
}

/// The shared body of `bin_to_list/2,3` (`binary.erl` delegates to
/// `binary_to_list/1,3`, whose range rules are `erts_binary_part`'s).
fn bin_to_list_slice(
    ctx: &mut BifContext<'_>,
    subject: Eterm,
    epos: Eterm,
    elen: Eterm,
) -> Result<Eterm> {
    let bytes = bits::bytes(subject)?;
    let slice = checked_slice(&bytes, epos, elen)?;
    bytes_to_list(ctx, slice)
}

/// `decode_unsigned(Binary)`.
fn decode_unsigned_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    decode_unsigned(ctx, args[0], Endian::Big)
}

/// `decode_unsigned(Binary, Endian)`.
fn decode_unsigned_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let endian = endian_of(ctx, args[1])?;
    decode_unsigned(ctx, args[0], endian)
}

/// `do_decode_unsigned`: the big/little-endian unsigned value, with the leading
/// (respectively trailing) zero bytes stripped; an empty result is `0`.
///
/// The magnitude is assembled straight from the bytes (big-endian: fold the bytes
/// most-significant first into little-endian limbs; little-endian: the same, byte
/// order reversed), so *any* width is accepted and a value past the 60-bit small
/// range becomes the bignum the oracle builds (`<<255 × 8>>` ⇒
/// `18446744073709551615`, `<<1,0 × 8>>` ⇒ `18446744073709551616`).
fn decode_unsigned(ctx: &mut BifContext<'_>, subject: Eterm, endian: Endian) -> Result<Eterm> {
    let bytes = bits::bytes(subject)?;
    let mut significant: &[u8] = &bytes;
    match endian {
        Endian::Big => {
            let skip = significant.iter().position(|&b| b != 0).unwrap_or(significant.len());
            significant = &significant[skip..];
        }
        Endian::Little => {
            let mut n = significant.len();
            while n > 0 && significant[n - 1] == 0 {
                n -= 1;
            }
            significant = &significant[..n];
        }
    }
    if significant.is_empty() {
        return ctx.int(0);
    }
    let mut magnitude = vec![0u64; significant.len().div_ceil(8)];
    match endian {
        // Most significant byte first: the byte `i` from the end is the `i`-th
        // least-significant byte of the magnitude.
        Endian::Big => {
            for (i, &byte) in significant.iter().rev().enumerate() {
                magnitude[i / 8] |= u64::from(byte) << (8 * (i % 8));
            }
        }
        // Least significant byte first: byte `i` is the `i`-th least-significant.
        Endian::Little => {
            for (i, &byte) in significant.iter().enumerate() {
                magnitude[i / 8] |= u64::from(byte) << (8 * (i % 8));
            }
        }
    }
    arith::int_from_mag(ctx, false, &magnitude)
}

/// `encode_unsigned(Integer)`.
fn encode_unsigned_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    encode_unsigned(ctx, args[0], Endian::Big)
}

/// `encode_unsigned(Integer, Endian)`.
fn encode_unsigned_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let endian = endian_of(ctx, args[1])?;
    encode_unsigned(ctx, args[0], endian)
}

/// `do_encode_unsigned`: the minimal big/little-endian bytes of a non-negative
/// integer (`0` ⇒ `<<0>>`).
///
/// A **bignum** is accepted (its limbs are read with
/// [`beam_codec::term::big_parts`]) and emits its minimal byte image exactly as
/// the oracle does (`encode_unsigned(18446744073709551616)` ⇒
/// `<<1,0,0,0,0,0,0,0,0>>`). A negative value, a float and any other non-integer
/// are `badarg`.
fn encode_unsigned(ctx: &mut BifContext<'_>, value: Eterm, endian: Endian) -> Result<Eterm> {
    let big_endian: Vec<u8> = if access::is_small(value) {
        let signed = signed_value(value);
        if signed < 0 {
            return Err(badarg());
        }
        let unsigned = signed as u64;
        let width = byte_width(unsigned);
        unsigned.to_be_bytes()[8 - width..].to_vec()
    } else if access::is_big(value) {
        let (negative, digits) = codec_term::big_parts(value)?;
        if negative {
            return Err(badarg());
        }
        // Least significant limb first: walk them in reverse for big-endian bytes.
        let mut bytes = Vec::with_capacity(digits.len() * 8);
        for limb in digits.iter().rev() {
            bytes.extend_from_slice(&limb.to_be_bytes());
        }
        let skip = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
        let bytes = bytes[skip..].to_vec();
        if bytes.is_empty() {
            return Err(badarg());
        }
        bytes
    } else {
        return Err(badarg());
    };
    let out = match endian {
        Endian::Big => big_endian,
        Endian::Little => {
            let mut little = big_endian;
            little.reverse();
            little
        }
    };
    bits::make_binary(ctx.heap(), &out)
}

/// `get_need`: the byte width of `u`, at least 1 (`0` ⇒ 1).
fn byte_width(u: u64) -> usize {
    let bits = 64 - u.leading_zeros() as usize;
    bits.div_ceil(8).max(1)
}

/// `first(Binary)`.
fn first_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = bits::bytes(args[0])?;
    let first = *bytes.first().ok_or_else(badarg)?;
    ctx.int(i64::from(first))
}

/// `last(Binary)`.
fn last_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = bits::bytes(args[0])?;
    let last = *bytes.last().ok_or_else(badarg)?;
    ctx.int(i64::from(last))
}

/// `split(Subject, Pattern)`.
fn split_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_split(ctx, args[0], args[1], None)
}

/// `split(Subject, Pattern, Options)`.
fn split_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_split(ctx, args[0], args[1], Some(args[2]))
}

/// `binary_split`: on the first match without `global`, on every non-overlapping
/// match with it (`do_split_single_result` / `do_split_global_result`).
fn do_split(
    ctx: &mut BifContext<'_>,
    subject: Eterm,
    pattern: Eterm,
    opts: Option<Eterm>,
) -> Result<Eterm> {
    let bytes = bits::bytes(subject)?;
    let alternatives = parse_pattern(ctx, pattern)?;
    let options = split_options(ctx, opts, bytes.len())?;

    if !options.global {
        return match find_first(&bytes, &alternatives, options.start, options.end) {
            None => split_not_found(ctx, &bytes, options.trimming()),
            Some((pos, len)) => {
                if options.trimming() && bytes.len() == pos + len {
                    // The match ends the subject: the trailing empty piece is
                    // dropped (and the leading one too when the match was the whole
                    // subject).
                    if pos == 0 {
                        return ctx.list(&[]);
                    }
                    let head = bits::make_binary(ctx.heap(), &bytes[..pos])?;
                    return ctx.list(&[head]);
                }
                let mut pieces = Vec::with_capacity(2);
                if !(options.trim_all && pos == 0) {
                    pieces.push(bits::make_binary(ctx.heap(), &bytes[..pos])?);
                }
                pieces.push(bits::make_binary(ctx.heap(), &bytes[pos + len..])?);
                ctx.list(&pieces)
            }
        };
    }

    let matches = find_all(&bytes, &alternatives, options.start, options.end);
    if matches.is_empty() {
        return split_not_found(ctx, &bytes, options.trimming());
    }
    // `do_split_global_result`: walk the matches from the last to the first,
    // emitting the gap after each — `do_trim` starts as `TRIM|TRIM_ALL` and keeps
    // only its `TRIM_ALL` bit once a piece has been emitted.
    let mut do_trim = options.trimming();
    let mut pieces: Vec<(usize, usize)> = Vec::with_capacity(matches.len() + 1);
    let mut end_pos = bytes.len();
    for &(pos, len) in matches.iter().rev() {
        let piece_start = pos + len;
        if !(piece_start == end_pos && do_trim) {
            pieces.push((piece_start, end_pos));
            do_trim = options.trim_all;
        }
        end_pos = pos;
    }
    let first_start = matches.first().map_or(0, |&(pos, _)| pos);
    if !(first_start == 0 && do_trim) {
        pieces.push((0, first_start));
    }
    pieces.reverse();
    let mut elems = Vec::with_capacity(pieces.len());
    for &(start, end) in &pieces {
        elems.push(bits::make_binary(ctx.heap(), &bytes[start..end])?);
    }
    ctx.list(&elems)
}

/// `do_split_not_found_result`: `[Subject]`, except that `trim`/`trim_all` on an
/// empty subject yields `[]`.
fn split_not_found(ctx: &mut BifContext<'_>, bytes: &[u8], trimming: bool) -> Result<Eterm> {
    if trimming && bytes.is_empty() {
        return ctx.list(&[]);
    }
    let whole = bits::make_binary(ctx.heap(), bytes)?;
    ctx.list(&[whole])
}

/// `match(Subject, Pattern)`.
fn match_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_match(ctx, args[0], args[1], None)
}

/// `match(Subject, Pattern, Options)`.
fn match_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_match(ctx, args[0], args[1], Some(args[2]))
}

/// `binary_match`: `{Pos, Len}` of the first match, or the atom `nomatch`.
fn do_match(
    ctx: &mut BifContext<'_>,
    subject: Eterm,
    pattern: Eterm,
    opts: Option<Eterm>,
) -> Result<Eterm> {
    let bytes = bits::bytes(subject)?;
    let alternatives = parse_pattern(ctx, pattern)?;
    let (start, end) = match_options(ctx, opts, bytes.len())?;
    match find_first(&bytes, &alternatives, start, end) {
        Some((pos, len)) => {
            let pos = ctx.int(pos as i64)?;
            let len = ctx.int(len as i64)?;
            ctx.tuple(&[pos, len])
        }
        None => ctx.atom("nomatch"),
    }
}

/// `matches(Subject, Pattern)`.
fn matches_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_matches(ctx, args[0], args[1], None)
}

/// `matches(Subject, Pattern, Options)`.
fn matches_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    do_matches(ctx, args[0], args[1], Some(args[2]))
}

/// `binary_matches`: every non-overlapping match as `[{Pos, Len}, …]`, in
/// position order — `[]` when there is none (not `nomatch`). The pattern/option
/// rules are the same `parse_pattern`/`parse_match_opts_list` `match/2,3` uses.
fn do_matches(
    ctx: &mut BifContext<'_>,
    subject: Eterm,
    pattern: Eterm,
    opts: Option<Eterm>,
) -> Result<Eterm> {
    let bytes = bits::bytes(subject)?;
    let alternatives = parse_pattern(ctx, pattern)?;
    let (start, end) = match_options(ctx, opts, bytes.len())?;
    let matches = find_all(&bytes, &alternatives, start, end);
    let mut elems = Vec::with_capacity(matches.len());
    for (pos, len) in matches {
        let pos = ctx.int(pos as i64)?;
        let len = ctx.int(len as i64)?;
        elems.push(ctx.tuple(&[pos, len])?);
    }
    ctx.list(&elems)
}

/// `longest_common_prefix(Binaries)` — the byte length of the common prefix.
fn longest_common_prefix_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    common_edge(ctx, args[0], false)
}

/// `longest_common_suffix(Binaries)` — the byte length of the common suffix.
fn longest_common_suffix_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    common_edge(ctx, args[0], true)
}

/// `start_longest_common`: the byte length of the common prefix (`from_end`
/// false) or suffix (`from_end` true) of a non-empty list of byte-aligned
/// binaries. An empty list, a non-binary element and a non-list are `badarg`.
fn common_edge(ctx: &mut BifContext<'_>, list: Eterm, from_end: bool) -> Result<Eterm> {
    let items = proper_list(list).ok_or_else(badarg)?;
    if items.is_empty() {
        return Err(badarg());
    }
    let mut binaries = Vec::with_capacity(items.len());
    for item in items {
        binaries.push(bits::bytes(item)?);
    }
    let mut shortest = usize::MAX;
    for binary in &binaries {
        shortest = shortest.min(binary.len());
    }
    let first = &binaries[0];
    let mut common = 0;
    'edge: for i in 0..shortest {
        let byte = if from_end { first[first.len() - 1 - i] } else { first[i] };
        for binary in &binaries[1..] {
            let other = if from_end { binary[binary.len() - 1 - i] } else { binary[i] };
            if other != byte {
                break 'edge;
            }
        }
        common += 1;
    }
    ctx.int(common as i64)
}

/// Registers the `binary` module BIFs.
///
/// Also registers the four **`erlang`** iolist/bitstring BIFs that belong to the
/// same `erl_iolist.c`/`binary.c` machinery — `iolist_to_binary/1`,
/// `iolist_size/1`, `list_to_bitstring/1` and `split_binary/2`.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "iolist_to_binary", 1, iolist_to_binary_1);
    reg.register("erlang", "iolist_size", 1, iolist_size_1);
    reg.register("erlang", "list_to_bitstring", 1, list_to_bitstring_1);
    reg.register("erlang", "split_binary", 2, split_binary_2);
    reg.register("binary", "at", 2, at);
    reg.register("binary", "part", 2, part_2);
    reg.register("binary", "part", 3, part_3);
    reg.register("binary", "copy", 1, copy_1);
    reg.register("binary", "copy", 2, copy_2);
    reg.register("binary", "list_to_bin", 1, list_to_bin_1);
    reg.register("binary", "bin_to_list", 1, bin_to_list_1);
    reg.register("binary", "bin_to_list", 2, bin_to_list_2);
    reg.register("binary", "bin_to_list", 3, bin_to_list_3);
    reg.register("binary", "decode_unsigned", 1, decode_unsigned_1);
    reg.register("binary", "decode_unsigned", 2, decode_unsigned_2);
    reg.register("binary", "encode_unsigned", 1, encode_unsigned_1);
    reg.register("binary", "encode_unsigned", 2, encode_unsigned_2);
    reg.register("binary", "first", 1, first_1);
    reg.register("binary", "last", 1, last_1);
    reg.register("binary", "compile_pattern", 1, compile_pattern_1);
    reg.register("binary", "split", 2, split_2);
    reg.register("binary", "split", 3, split_3);
    reg.register("binary", "match", 2, match_2);
    reg.register("binary", "match", 3, match_3);
    reg.register("binary", "matches", 2, matches_2);
    reg.register("binary", "matches", 3, matches_3);
    reg.register("binary", "longest_common_prefix", 1, longest_common_prefix_1);
    reg.register("binary", "longest_common_suffix", 1, longest_common_suffix_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{AtomTable, NIL};

    /// The atom names the tests build terms from (the runtime seeds the same set:
    /// `big`/`little`/`scope`/`global`/`trim`/`trim_all`/`nomatch` are standard
    /// atoms a real ERTS node interns at boot, as are the compiled-pattern tags
    /// `bm`/`ac`).
    const SEED: [&str; 17] = [
        "true", "false", "ok", "big", "little", "native", "scope", "global", "trim", "trim_all",
        "nomatch", "a", "abc", "b", "foo", "bm", "ac",
    ];

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(SEED);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// The on-heap binary `<<bytes>>`.
    fn bin(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        bits::make_binary(ctx.heap(), bytes).expect("the binary fits")
    }

    /// The bytes of a binary result.
    fn bytes(term: Eterm) -> Vec<u8> {
        bits::bytes(term).expect("a byte-aligned binary")
    }

    /// The small integers of a proper list result.
    fn ints(term: Eterm) -> Vec<i64> {
        let mut values = Vec::new();
        let mut cur = term;
        while access::is_list(cur) {
            let head = access::car(cur);
            assert!(access::is_small(head), "list element must be a small int");
            values.push(signed_value(head));
            cur = access::cdr(cur);
        }
        assert!(access::is_nil(cur), "the result must be a proper list");
        values
    }

    /// The small-int terms of `values`.
    fn smalls(ctx: &BifContext<'_>, values: &[i64]) -> Vec<Eterm> {
        values.iter().map(|&v| ctx.int(v).expect("a small int")).collect()
    }

    /// The list `[B0, B1, …]` of byte terms.
    fn byte_list(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
        let elems = smalls(ctx, values);
        ctx.list(&elems).expect("the list fits")
    }

    /// The `{Start, Len}` 2-tuple.
    fn range(ctx: &mut BifContext<'_>, start: i64, len: i64) -> Eterm {
        let elems = smalls(ctx, &[start, len]);
        ctx.tuple(&elems).expect("the tuple fits")
    }

    /// The `{scope, {Start, Len}}` option.
    fn scope(ctx: &mut BifContext<'_>, start: i64, len: i64) -> Eterm {
        let inner = range(ctx, start, len);
        let tag = ctx.atom("scope").expect("seeded");
        ctx.tuple(&[tag, inner]).expect("the tuple fits")
    }

    #[test]
    fn at_indexes_bytes_or_badarg() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[1, 2, 3]);
            let zero = ctx.int(0).expect("fits");
            assert_eq!(at(ctx, &[subject, zero]).unwrap(), ctx.int(1).unwrap());
            let two = ctx.int(2).expect("fits");
            assert_eq!(at(ctx, &[subject, two]).unwrap(), ctx.int(3).unwrap());

            // Out of range (`Pos == byte_size` and beyond), negative, float and
            // non-integer indices are `badarg`.
            for bad in [3, 4, 999_999_999, -1] {
                let pos = ctx.int(bad).expect("fits");
                assert!(
                    matches!(at(ctx, &[subject, pos]), Err(BeamError::BadArg)),
                    "at/2 with Pos {bad}"
                );
            }
            let float = ctx.make_float(1.0).unwrap();
            assert!(matches!(at(ctx, &[subject, float]), Err(BeamError::BadArg)));
            let atom = ctx.atom("a").unwrap();
            assert!(matches!(at(ctx, &[subject, atom]), Err(BeamError::BadArg)));

            // An empty binary and a non-binary subject are `badarg`.
            let empty = bin(ctx, &[]);
            let zero = ctx.int(0).expect("fits");
            assert!(matches!(at(ctx, &[empty, zero]), Err(BeamError::BadArg)));
            assert!(matches!(at(ctx, &[atom, zero]), Err(BeamError::BadArg)));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(at(ctx, &[bits, zero]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn part_2_and_3_slice_and_reverse() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[1, 2, 3]);

            let r = range(ctx, 0, 2);
            assert_eq!(bytes(part_2(ctx, &[subject, r]).unwrap()), vec![1, 2]);
            let r = range(ctx, 1, 2);
            assert_eq!(bytes(part_2(ctx, &[subject, r]).unwrap()), vec![2, 3]);

            // Empty slices at 0 and at `byte_size`.
            let r = range(ctx, 0, 0);
            assert!(bytes(part_2(ctx, &[subject, r]).unwrap()).is_empty());
            let r = range(ctx, 3, 0);
            assert!(bytes(part_2(ctx, &[subject, r]).unwrap()).is_empty());

            // `part/3` is the same helper.
            let (one, two, three, neg1, neg2, neg3) = (
                ctx.int(1).unwrap(),
                ctx.int(2).unwrap(),
                ctx.int(3).unwrap(),
                ctx.int(-1).unwrap(),
                ctx.int(-2).unwrap(),
                ctx.int(-3).unwrap(),
            );
            assert_eq!(bytes(part_3(ctx, &[subject, one, two]).unwrap()), vec![2, 3]);
            assert!(bytes(part_3(ctx, &[subject, three, ctx.int(0).unwrap()]).unwrap()).is_empty());

            // A negative Len counts backwards from Start.
            assert_eq!(bytes(part_3(ctx, &[subject, one, neg1]).unwrap()), vec![1]);
            assert_eq!(bytes(part_3(ctx, &[subject, two, neg2]).unwrap()), vec![1, 2]);
            let five = ctx.int(5).unwrap();
            assert!(matches!(part_3(ctx, &[subject, five, neg3]), Err(BeamError::BadArg)));
            assert!(matches!(part_3(ctx, &[subject, one, neg3]), Err(BeamError::BadArg)));

            // Ranges that leave the binary, a negative Start and non-integers.
            let r = range(ctx, 1, 3);
            assert!(matches!(part_2(ctx, &[subject, r]), Err(BeamError::BadArg)));
            let r = range(ctx, 2, 2);
            assert!(matches!(part_2(ctx, &[subject, r]), Err(BeamError::BadArg)));
            let r = range(ctx, 4, 0);
            assert!(matches!(part_2(ctx, &[subject, r]), Err(BeamError::BadArg)));
            let r = range(ctx, -1, 2);
            assert!(matches!(part_2(ctx, &[subject, r]), Err(BeamError::BadArg)));
            let r = range(ctx, 0, -1);
            assert!(matches!(part_2(ctx, &[subject, r]), Err(BeamError::BadArg)));
            assert!(matches!(part_3(ctx, &[subject, neg1, two]), Err(BeamError::BadArg)));
            let float = ctx.make_float(1.0).unwrap();
            assert!(matches!(part_3(ctx, &[subject, float, one]), Err(BeamError::BadArg)));

            // The second argument of `part/2` must be a 2-tuple.
            let three_tuple = ctx
                .tuple(&smalls(ctx, &[1, 2, 3]))
                .expect("fits");
            assert!(matches!(part_2(ctx, &[subject, three_tuple]), Err(BeamError::BadArg)));
            let one_tuple = ctx.tuple(&smalls(ctx, &[0])).expect("fits");
            assert!(matches!(part_2(ctx, &[subject, one_tuple]), Err(BeamError::BadArg)));
            assert!(matches!(part_2(ctx, &[subject, NIL]), Err(BeamError::BadArg)));

            // A non-binary subject (and a non-byte-aligned bitstring) is `badarg`.
            let atom = ctx.atom("abc").unwrap();
            let r = range(ctx, 0, 1);
            assert!(matches!(part_2(ctx, &[atom, r]), Err(BeamError::BadArg)));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(part_2(ctx, &[bits, r]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn copy_repeats_or_empties() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[1, 2]);
            assert_eq!(bytes(copy_1(ctx, &[subject]).unwrap()), vec![1, 2]);

            let (zero, one, three, neg) = (
                ctx.int(0).unwrap(),
                ctx.int(1).unwrap(),
                ctx.int(3).unwrap(),
                ctx.int(-1).unwrap(),
            );
            assert!(bytes(copy_2(ctx, &[subject, zero]).unwrap()).is_empty());
            assert_eq!(bytes(copy_2(ctx, &[subject, one]).unwrap()), vec![1, 2]);
            assert_eq!(
                bytes(copy_2(ctx, &[subject, three]).unwrap()),
                vec![1, 2, 1, 2, 1, 2]
            );

            // An empty source copies to `<<>>` whatever the count.
            let empty = bin(ctx, &[]);
            assert!(bytes(copy_2(ctx, &[empty, three]).unwrap()).is_empty());

            // A negative or non-integer count, a non-byte-aligned source and a
            // non-binary source are `badarg`.
            assert!(matches!(copy_2(ctx, &[subject, neg]), Err(BeamError::BadArg)));
            let float = ctx.make_float(1.0).unwrap();
            assert!(matches!(copy_2(ctx, &[subject, float]), Err(BeamError::BadArg)));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(copy_2(ctx, &[bits, three]), Err(BeamError::BadArg)));
            let atom = ctx.atom("abc").unwrap();
            assert!(matches!(copy_2(ctx, &[atom, three]), Err(BeamError::BadArg)));

            let three_bytes = bin(ctx, &[1, 2, 3]);
            assert_eq!(bytes(copy_1(ctx, &[three_bytes]).unwrap()), vec![1, 2, 3]);
        });
    }

    #[test]
    fn list_to_bin_accepts_iolists_and_rejects_bad_leaves() {
        with_ctx(|ctx| {
            // A flat byte list.
            let flat = byte_list(ctx, &[1, 2, 3]);
            assert_eq!(bytes(list_to_bin_1(ctx, &[flat]).unwrap()), vec![1, 2, 3]);

            // `[]` is the empty binary.
            let empty = ctx.list(&[]).unwrap();
            assert!(bytes(list_to_bin_1(ctx, &[empty]).unwrap()).is_empty());

            // A binary leaf and a byte leaf.
            let head = bin(ctx, &[1, 2]);
            let three = ctx.int(3).unwrap();
            let mixed = ctx.list(&[head, three]).unwrap();
            assert_eq!(bytes(list_to_bin_1(ctx, &[mixed]).unwrap()), vec![1, 2, 3]);

            // A deeply nested iolist.
            let inner = byte_list(ctx, &[3]);
            let nested_inner = ctx.list(&[inner]).unwrap();
            let two = ctx.int(2).unwrap();
            let nested = ctx.list(&[two, nested_inner]).unwrap();
            let one = ctx.int(1).unwrap();
            let deep = ctx.list(&[one, nested]).unwrap();
            assert_eq!(bytes(list_to_bin_1(ctx, &[deep]).unwrap()), vec![1, 2, 3]);

            // The boundary bytes 0 and 255.
            let zero = byte_list(ctx, &[0]);
            assert_eq!(bytes(list_to_bin_1(ctx, &[zero]).unwrap()), vec![0]);
            let max = byte_list(ctx, &[255]);
            assert_eq!(bytes(list_to_bin_1(ctx, &[max]).unwrap()), vec![255]);

            // Rejected: an out-of-range byte, a negative byte, a non-byte term, an
            // improper tail, a non-list top level and a non-byte-aligned leaf.
            for bad in [byte_list(ctx, &[256]), byte_list(ctx, &[-1])] {
                assert!(matches!(list_to_bin_1(ctx, &[bad]), Err(BeamError::BadArg)));
            }
            let atom = ctx.atom("a").unwrap();
            let with_atom = ctx.list(&[atom]).unwrap();
            assert!(matches!(list_to_bin_1(ctx, &[with_atom]), Err(BeamError::BadArg)));
            let tuple = ctx.tuple(&smalls(ctx, &[1])).expect("fits");
            let with_tuple = ctx.list(&[tuple]).unwrap();
            assert!(matches!(list_to_bin_1(ctx, &[with_tuple]), Err(BeamError::BadArg)));

            let one = ctx.int(1).unwrap();
            let two = ctx.int(2).unwrap();
            let improper = ctx.cons(one, two).unwrap();
            assert!(matches!(list_to_bin_1(ctx, &[improper]), Err(BeamError::BadArg)));
            let inner_improper = ctx.cons(one, two).unwrap();
            let nested_improper = ctx.list(&[inner_improper]).unwrap();
            assert!(matches!(list_to_bin_1(ctx, &[nested_improper]), Err(BeamError::BadArg)));

            assert!(matches!(list_to_bin_1(ctx, &[atom]), Err(BeamError::BadArg)));
            let bare = bin(ctx, &[1, 2]);
            assert!(matches!(list_to_bin_1(ctx, &[bare]), Err(BeamError::BadArg)));

            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            let with_bits = ctx.list(&[bits]).unwrap();
            assert!(matches!(list_to_bin_1(ctx, &[with_bits]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn bin_to_list_arity_1_2_3() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[1, 2, 3]);
            assert_eq!(ints(bin_to_list_1(ctx, &[subject]).unwrap()), vec![1, 2, 3]);
            let empty = bin(ctx, &[]);
            assert!(ints(bin_to_list_1(ctx, &[empty]).unwrap()).is_empty());

            let r = range(ctx, 1, 2);
            assert_eq!(ints(bin_to_list_2(ctx, &[subject, r]).unwrap()), vec![2, 3]);
            let r = range(ctx, 1, 3);
            assert!(matches!(bin_to_list_2(ctx, &[subject, r]), Err(BeamError::BadArg)));
            let atom = ctx.atom("abc").unwrap();
            assert!(matches!(bin_to_list_2(ctx, &[subject, atom]), Err(BeamError::BadArg)));

            let (zero, one, two, three, neg1) = (
                ctx.int(0).unwrap(),
                ctx.int(1).unwrap(),
                ctx.int(2).unwrap(),
                ctx.int(3).unwrap(),
                ctx.int(-1).unwrap(),
            );
            assert_eq!(ints(bin_to_list_3(ctx, &[subject, one, two]).unwrap()), vec![2, 3]);
            assert!(ints(bin_to_list_3(ctx, &[subject, three, zero]).unwrap()).is_empty());
            assert_eq!(ints(bin_to_list_3(ctx, &[subject, zero, three]).unwrap()), vec![1, 2, 3]);
            // A negative Len counts backwards from Start.
            assert_eq!(ints(bin_to_list_3(ctx, &[subject, two, neg1]).unwrap()), vec![2]);
            let four = ctx.int(4).unwrap();
            assert!(matches!(
                bin_to_list_3(ctx, &[subject, zero, four]),
                Err(BeamError::BadArg)
            ));
            let neg1b = ctx.int(-1).unwrap();
            assert!(matches!(
                bin_to_list_3(ctx, &[subject, neg1b, two]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                bin_to_list_3(ctx, &[subject, neg1b, neg1]),
                Err(BeamError::BadArg)
            ));
            let float = ctx.make_float(1.0).unwrap();
            assert!(matches!(
                bin_to_list_3(ctx, &[subject, float, two]),
                Err(BeamError::BadArg)
            ));

            // A non-binary subject and a non-byte-aligned bitstring are `badarg`.
            let r = range(ctx, 0, 1);
            assert!(matches!(bin_to_list_2(ctx, &[atom, r]), Err(BeamError::BadArg)));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(bin_to_list_1(ctx, &[bits]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn decode_and_encode_unsigned_both_endians() {
        with_ctx(|ctx| {
            let little = ctx.atom("little").unwrap();
            let big = ctx.atom("big").unwrap();
            let native = ctx.atom("native").unwrap();
            let foo = ctx.atom("foo").unwrap();

            // `decode_unsigned/1` is the big-endian form.
            for (data, expected) in [
                (vec![], 0i64),
                (vec![0], 0),
                (vec![0, 0], 0),
                (vec![255], 255),
                (vec![1, 0], 256),
                (vec![0, 0, 1, 0], 256),
                (vec![1, 0, 0], 65536),
                (vec![255, 255], 65535),
            ] {
                let subject = bin(ctx, &data);
                let expect = ctx.int(expected).unwrap();
                assert_eq!(
                    decode_unsigned_1(ctx, &[subject]).unwrap(),
                    expect,
                    "decode_unsigned(<<{data:?}>>)"
                );
            }

            // Little-endian reverses the byte order.
            let subject = bin(ctx, &[1, 0]);
            assert_eq!(
                decode_unsigned_2(ctx, &[subject, little]).unwrap(),
                ctx.int(1).unwrap()
            );
            let subject = bin(ctx, &[0, 0, 0, 1]);
            assert_eq!(
                decode_unsigned_2(ctx, &[subject, little]).unwrap(),
                ctx.int(16_777_216).unwrap()
            );
            let subject = bin(ctx, &[1, 0]);
            assert_eq!(
                decode_unsigned_2(ctx, &[subject, big]).unwrap(),
                ctx.int(256).unwrap()
            );

            // `native` is `badarg` in the oracle, as is any other atom.
            let subject = bin(ctx, &[1, 0]);
            assert!(matches!(
                decode_unsigned_2(ctx, &[subject, native]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                decode_unsigned_2(ctx, &[subject, foo]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                decode_unsigned_2(ctx, &[subject, ctx.int(1).unwrap()]),
                Err(BeamError::BadArg)
            ));
            // A non-byte-aligned bitstring and a non-binary are `badarg`.
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(decode_unsigned_1(ctx, &[bits]), Err(BeamError::BadArg)));
            assert!(matches!(decode_unsigned_1(ctx, &[foo]), Err(BeamError::BadArg)));

            // `encode_unsigned/1` is the big-endian form.
            for (value, expected) in [
                (0i64, vec![0u8]),
                (1, vec![1]),
                (255, vec![255]),
                (256, vec![1, 0]),
                (65535, vec![255, 255]),
                (65536, vec![1, 0, 0]),
            ] {
                let arg = ctx.int(value).unwrap();
                assert_eq!(
                    bytes(encode_unsigned_1(ctx, &[arg]).unwrap()),
                    expected,
                    "encode_unsigned({value})"
                );
            }

            let arg = ctx.int(256).unwrap();
            assert_eq!(
                bytes(encode_unsigned_2(ctx, &[arg, little]).unwrap()),
                vec![0, 1]
            );
            assert_eq!(bytes(encode_unsigned_2(ctx, &[arg, big]).unwrap()), vec![1, 0]);
            let big_value = ctx.int(65536).unwrap();
            assert_eq!(
                bytes(encode_unsigned_2(ctx, &[big_value, little]).unwrap()),
                vec![0, 0, 1]
            );

            // A negative value, a float, a non-number and a bad endian atom are
            // all `badarg`.
            let neg = ctx.int(-1).unwrap();
            assert!(matches!(encode_unsigned_1(ctx, &[neg]), Err(BeamError::BadArg)));
            let float = ctx.make_float(1.0).unwrap();
            assert!(matches!(encode_unsigned_1(ctx, &[float]), Err(BeamError::BadArg)));
            assert!(matches!(encode_unsigned_1(ctx, &[foo]), Err(BeamError::BadArg)));
            let one = ctx.int(1).unwrap();
            assert!(matches!(
                encode_unsigned_2(ctx, &[one, native]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(encode_unsigned_2(ctx, &[one, foo]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn first_and_last_bytes() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[1, 2, 3]);
            assert_eq!(first_1(ctx, &[subject]).unwrap(), ctx.int(1).unwrap());
            assert_eq!(last_1(ctx, &[subject]).unwrap(), ctx.int(3).unwrap());

            let single = bin(ctx, &[7]);
            assert_eq!(first_1(ctx, &[single]).unwrap(), ctx.int(7).unwrap());
            assert_eq!(last_1(ctx, &[single]).unwrap(), ctx.int(7).unwrap());

            // An empty binary and a non-binary are `badarg`.
            let empty = bin(ctx, &[]);
            assert!(matches!(first_1(ctx, &[empty]), Err(BeamError::BadArg)));
            assert!(matches!(last_1(ctx, &[empty]), Err(BeamError::BadArg)));
            let atom = ctx.atom("abc").unwrap();
            assert!(matches!(first_1(ctx, &[atom]), Err(BeamError::BadArg)));
            assert!(matches!(last_1(ctx, &[atom]), Err(BeamError::BadArg)));

            // A non-byte-aligned bitstring is `badarg`; a byte-aligned one is not.
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(first_1(ctx, &[bits]), Err(BeamError::BadArg)));
            assert!(matches!(last_1(ctx, &[bits]), Err(BeamError::BadArg)));
            let aligned = bits::make_bitstring(ctx.heap(), &[0xFF], 8).expect("fits");
            assert_eq!(first_1(ctx, &[aligned]).unwrap(), ctx.int(255).unwrap());
        });
    }

    #[test]
    fn split_first_and_global_and_trim() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[b'a', b'X', b'b', b'X', b'c']);
            let x = bin(ctx, &[b'X']);
            let nomatch = bin(ctx, &[b'Z']);

            // `split/2` splits on the first match only.
            let parts = proper_list(split_2(ctx, &[subject, x]).unwrap()).expect("a proper list");
            assert_eq!(parts.len(), 2);
            assert_eq!(bytes(parts[0]), b"a".to_vec());
            assert_eq!(bytes(parts[1]), b"bXc".to_vec());

            // No match ⇒ `[Subject]`; the empty subject with no trim ⇒ `[<<>>]`.
            let parts = proper_list(split_2(ctx, &[subject, nomatch]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 1);
            assert_eq!(bytes(parts[0]), b"aXbXc".to_vec());
            let empty = bin(ctx, &[]);
            let parts = proper_list(split_2(ctx, &[empty, x]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 1);
            assert!(bytes(parts[0]).is_empty());

            // `[global]` splits on every match and keeps the empty end pieces.
            let global = ctx.atom("global").unwrap();
            let opts = ctx.list(&[global]).unwrap();
            let parts = proper_list(split_3(ctx, &[subject, x, opts]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 3);
            assert_eq!(bytes(parts[0]), b"a".to_vec());
            assert_eq!(bytes(parts[1]), b"b".to_vec());
            assert_eq!(bytes(parts[2]), b"c".to_vec());

            let trailing = bin(ctx, &[b'a', b'X', b'b', b'X', b'c', b'X']);
            let parts = proper_list(split_3(ctx, &[trailing, x, opts]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 4);
            assert!(bytes(parts[3]).is_empty());

            // `trim` drops the trailing empty piece, `trim_all` both ends.
            let trim = ctx.atom("trim").unwrap();
            let trim_all = ctx.atom("trim_all").unwrap();
            let trim_opts = ctx.list(&[global, trim]).unwrap();
            let trim_all_opts = ctx.list(&[global, trim_all]).unwrap();
            let parts = proper_list(split_3(ctx, &[trailing, x, trim_opts]).unwrap()).expect("p");
            assert_eq!(parts.len(), 3);
            assert!(bytes(parts[2]) == b"c".to_vec());

            let padded = bin(ctx, &[b'X', b'a', b'b', b'c', b'X']);
            let parts = proper_list(split_3(ctx, &[padded, x, trim_opts]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 2);
            assert!(bytes(parts[0]).is_empty());
            assert_eq!(bytes(parts[1]), b"abc".to_vec());
            let parts =
                proper_list(split_3(ctx, &[padded, x, trim_all_opts]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 1);
            assert_eq!(bytes(parts[0]), b"abc".to_vec());

            // The non-global forms of the same trim rules.
            let leading = bin(ctx, &[b'X', b'a', b'b', b'c']);
            let parts = proper_list(split_2(ctx, &[leading, x]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 2);
            let parts =
                proper_list(split_3(ctx, &[leading, x, trim_all_opts]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 1);
            let both = bin(ctx, &[b'X']);
            assert!(proper_list(split_3(ctx, &[both, x, trim_all_opts]).unwrap())
                .expect("proper")
                .is_empty());
            let ending = bin(ctx, &[b'a', b'b', b'c', b'X']);
            let parts = proper_list(split_3(ctx, &[ending, x, trim_opts]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 1);
            assert_eq!(bytes(parts[0]), b"abc".to_vec());
            // `trim` on an empty subject with no match is `[]`.
            let parts = proper_list(split_3(ctx, &[empty, nomatch, trim_opts]).unwrap())
                .expect("proper");
            assert!(parts.is_empty());

            // A `{scope, …}` option restricts the search to that byte range.
            let scoped = bin(ctx, &[b'a', b'X', b'c']);
            let sc = scope(ctx, 2, 1);
            let scoped_opts = ctx.list(&[sc]).unwrap();
            let parts = proper_list(split_3(ctx, &[scoped, x, scoped_opts]).unwrap()).expect("p");
            assert_eq!(parts.len(), 1);
            assert_eq!(bytes(parts[0]), b"aXc".to_vec());
            let sc = scope(ctx, 0, 3);
            let scoped_opts = ctx.list(&[sc, global]).unwrap();
            let parts = proper_list(split_3(ctx, &[scoped, x, scoped_opts]).unwrap()).expect("p");
            assert_eq!(parts.len(), 2);
            assert_eq!(bytes(parts[0]), b"a".to_vec());
            assert_eq!(bytes(parts[1]), b"c".to_vec());

            // An empty pattern, a bad option, an out-of-range scope, a
            // non-byte-aligned subject and a non-binary pattern are `badarg`.
            let empty_pattern = bin(ctx, &[]);
            assert!(matches!(
                split_2(ctx, &[subject, empty_pattern]),
                Err(BeamError::BadArg)
            ));
            let bad_opt = ctx.atom("bad").unwrap_or_else(|_| ctx.atom("foo").unwrap());
            let bad_opts = ctx.list(&[bad_opt]).unwrap();
            assert!(matches!(
                split_3(ctx, &[subject, x, bad_opts]),
                Err(BeamError::BadArg)
            ));
            let sc = scope(ctx, 0, 100);
            let scoped_opts = ctx.list(&[sc]).unwrap();
            assert!(matches!(
                split_3(ctx, &[subject, x, scoped_opts]),
                Err(BeamError::BadArg)
            ));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(split_2(ctx, &[bits, x]), Err(BeamError::BadArg)));
            let atom = ctx.atom("abc").unwrap();
            assert!(matches!(split_2(ctx, &[subject, atom]), Err(BeamError::BadArg)));

            // A list pattern picks the alternative with the leftmost-longest match.
            let aba = bin(ctx, &[b'a', b'b', b'a']);
            let ab = bin(ctx, &[b'a', b'b']);
            let alt = ctx.list(&[ab, aba]).unwrap();
            let abab = bin(ctx, &[b'a', b'b', b'a', b'b']);
            let parts = proper_list(split_2(ctx, &[abab, alt]).unwrap()).expect("proper");
            assert_eq!(parts.len(), 2);
            assert!(bytes(parts[0]).is_empty());
            assert_eq!(bytes(parts[1]), b"b".to_vec());
        });
    }

    /// The `[{Pos, Len}, …]` result of `matches/2,3` as `(Pos, Len)` pairs.
    fn positions(term: Eterm) -> Vec<(i64, i64)> {
        proper_list(term)
            .expect("matches returns a proper list")
            .into_iter()
            .map(|pair| {
                assert!(access::is_tuple(pair) && access::tuple_size(pair) == 2);
                (
                    signed_value(access::tuple_element(pair, 0)),
                    signed_value(access::tuple_element(pair, 1)),
                )
            })
            .collect()
    }

    #[test]
    fn matches_returns_every_non_overlapping_match() {
        with_ctx(|ctx| {
            let subject = bin(ctx, &[b'a', b'b', b'c', b'a', b'b', b'c', b'a', b'b']);
            let ab = bin(ctx, &[b'a', b'b']);
            assert_eq!(
                positions(matches_2(ctx, &[subject, ab]).unwrap()),
                vec![(0, 2), (3, 2), (6, 2)]
            );

            // No match is `[]` (not the `nomatch` atom `match/2` returns).
            let zz = bin(ctx, &[b'z', b'z']);
            assert!(access::is_nil(matches_2(ctx, &[subject, zz]).unwrap()));

            // Adjacent non-overlapping occurrences, never overlapping ones.
            let aaaa = bin(ctx, &[b'a', b'a', b'a', b'a']);
            let aa = bin(ctx, &[b'a', b'a']);
            assert_eq!(positions(matches_2(ctx, &[aaaa, aa]).unwrap()), vec![(0, 2), (2, 2)]);

            // An alternative list picks the leftmost-longest match at each step.
            let aaa = bin(ctx, &[b'a', b'a', b'a']);
            let alts = ctx.list(&[aa, aaa]).unwrap();
            assert_eq!(positions(matches_2(ctx, &[aaaa, alts]).unwrap()), vec![(0, 3)]);

            // `{scope, …}` restricts the searched range (only whole matches).
            let sc = scope(ctx, 1, 5);
            let scoped = ctx.list(&[sc]).unwrap();
            assert_eq!(positions(matches_3(ctx, &[subject, ab, scoped]).unwrap()), vec![(3, 2)]);

            // An empty pattern, an empty alternative list, a bad option and a
            // non-list option list are `badarg`.
            let empty = bin(ctx, &[]);
            assert!(matches!(matches_2(ctx, &[subject, empty]), Err(BeamError::BadArg)));
            let empty_list = ctx.list(&[]).unwrap();
            assert!(matches!(matches_2(ctx, &[subject, empty_list]), Err(BeamError::BadArg)));
            let bad = ctx.list(&[ctx.atom("a").unwrap()]).unwrap();
            assert!(matches!(matches_3(ctx, &[subject, ab, bad]), Err(BeamError::BadArg)));
            let non_list_opts = ctx.atom("foo").unwrap();
            assert!(matches!(
                matches_3(ctx, &[subject, ab, non_list_opts]),
                Err(BeamError::BadArg)
            ));

            // A non-byte-aligned subject is `badarg`; an empty subject has none.
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(matches_2(ctx, &[bits, ab]), Err(BeamError::BadArg)));
            let empty_subject = bin(ctx, &[]);
            assert!(access::is_nil(matches_2(ctx, &[empty_subject, ab]).unwrap()));
        });
    }

    #[test]
    fn longest_common_suffix_mirrors_the_prefix() {
        with_ctx(|ctx| {
            let cases: [(&[&[u8]], i64, i64); 6] = [
                (&[b"abc", b"abd"], 2, 0),
                (&[b"abc", b"abc"], 3, 3),
                (&[b"abc", b"xbc"], 0, 2),
                (&[b"abcd", b"ab"], 2, 0),
                (&[b"", b""], 0, 0),
                (&[b"abc"], 3, 3),
            ];
            for (inputs, prefix, suffix) in cases {
                let mut elems = Vec::with_capacity(inputs.len());
                for input in inputs {
                    elems.push(bin(ctx, input));
                }
                let list = ctx.list(&elems).unwrap();
                assert_eq!(
                    longest_common_prefix_1(ctx, &[list]).unwrap(),
                    ctx.int(prefix).unwrap(),
                    "prefix of {inputs:?}"
                );
                assert_eq!(
                    longest_common_suffix_1(ctx, &[list]).unwrap(),
                    ctx.int(suffix).unwrap(),
                    "suffix of {inputs:?}"
                );
            }

            // An empty list, a non-binary element, a non-list and a
            // non-byte-aligned element are all `badarg`.
            let empty = ctx.list(&[]).unwrap();
            assert!(matches!(
                longest_common_suffix_1(ctx, &[empty]),
                Err(BeamError::BadArg)
            ));
            let atom = ctx.atom("abc").unwrap();
            let with_atom = ctx.list(&[atom]).unwrap();
            assert!(matches!(
                longest_common_suffix_1(ctx, &[with_atom]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(longest_common_suffix_1(ctx, &[atom]), Err(BeamError::BadArg)));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            let with_bits = ctx.list(&[bits, bits]).unwrap();
            assert!(matches!(
                longest_common_suffix_1(ctx, &[with_bits]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn match_finds_leftmost_longest() {
        with_ctx(|ctx| {
            let abcdef = bin(ctx, &[b'a', b'b', b'c', b'd', b'e', b'f']);
            let cd = bin(ctx, &[b'c', b'd']);
            let xy = bin(ctx, &[b'x', b'y']);
            let nomatch = ctx.atom("nomatch").unwrap();

            // A hit is `{Pos, Len}`.
            let found = match_2(ctx, &[abcdef, cd]).unwrap();
            assert!(access::is_tuple(found));
            assert_eq!(access::tuple_size(found), 2);
            assert_eq!(signed_value(access::tuple_element(found, 0)), 2);
            assert_eq!(signed_value(access::tuple_element(found, 1)), 2);
            // A miss is the atom `nomatch`.
            assert_eq!(match_2(ctx, &[abcdef, xy]).unwrap(), nomatch);

            // A list pattern is the alternative set; the leftmost start wins and
            // ties are broken by the longest alternative.
            let alts = ctx.list(&[xy, cd]).unwrap();
            let found = match_2(ctx, &[abcdef, alts]).unwrap();
            assert_eq!(signed_value(access::tuple_element(found, 0)), 2);
            assert_eq!(signed_value(access::tuple_element(found, 1)), 2);

            let abab = bin(ctx, &[b'a', b'b', b'a', b'b']);
            let a = bin(ctx, &[b'a']);
            let ab = bin(ctx, &[b'a', b'b']);
            let aba = bin(ctx, &[b'a', b'b', b'a']);
            for pattern in [
                ctx.list(&[ab, aba]).unwrap(),
                ctx.list(&[aba, ab]).unwrap(),
                ctx.list(&[a, ab]).unwrap(),
            ] {
                let found = match_2(ctx, &[abab, pattern]).unwrap();
                assert_eq!(signed_value(access::tuple_element(found, 0)), 0);
            }
            let just_b = bin(ctx, &[b'b']);
            let bab = bin(ctx, &[b'b', b'a', b'b']);
            let b_alt = ctx.list(&[just_b, bab]).unwrap();
            let found = match_2(ctx, &[abab, b_alt]).unwrap();
            assert_eq!(signed_value(access::tuple_element(found, 0)), 1);
            assert_eq!(signed_value(access::tuple_element(found, 1)), 3);

            // An empty pattern is `badarg` (`binary:match(<<"abcdef">>, <<>>)` ⇒
            // `badarg`); an empty *subject* with a non-empty pattern is `nomatch`.
            let empty = bin(ctx, &[]);
            assert!(matches!(
                match_2(ctx, &[abcdef, empty]),
                Err(BeamError::BadArg)
            ));

            // `{scope, …}` restricts the searched range (the whole pattern must fit
            // inside it).
            let e = bin(ctx, &[b'e']);
            let sc = scope(ctx, 0, 4);
            let scoped = ctx.list(&[sc]).unwrap();
            assert_eq!(match_3(ctx, &[abcdef, e, scoped]).unwrap(), nomatch);
            let d = bin(ctx, &[b'd']);
            let sc = scope(ctx, 3, 1);
            let scoped = ctx.list(&[sc]).unwrap();
            let found = match_3(ctx, &[abcdef, d, scoped]).unwrap();
            assert_eq!(signed_value(access::tuple_element(found, 0)), 3);
            assert_eq!(signed_value(access::tuple_element(found, 1)), 1);
            let bc = bin(ctx, &[b'b', b'c']);
            let sc = scope(ctx, 0, 2);
            let scoped = ctx.list(&[sc]).unwrap();
            assert_eq!(match_3(ctx, &[abcdef, bc, scoped]).unwrap(), nomatch);
            let sc = scope(ctx, 1, 2);
            let scoped = ctx.list(&[sc]).unwrap();
            let found = match_3(ctx, &[abcdef, bc, scoped]).unwrap();
            assert_eq!(signed_value(access::tuple_element(found, 0)), 1);
            assert_eq!(signed_value(access::tuple_element(found, 1)), 2);

            // An out-of-range scope, an empty pattern and a bad pattern are
            // `badarg`.
            let sc = scope(ctx, 0, 100);
            let scoped = ctx.list(&[sc]).unwrap();
            assert!(matches!(
                match_3(ctx, &[abcdef, cd, scoped]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(match_2(ctx, &[abcdef, empty]), Err(BeamError::BadArg)));
            let empty_list = ctx.list(&[]).unwrap();
            assert!(matches!(match_2(ctx, &[abcdef, empty_list]), Err(BeamError::BadArg)));
            let atom = ctx.atom("a").unwrap();
            let non_binaries = ctx.list(&[atom, atom]).unwrap();
            assert!(matches!(match_2(ctx, &[abcdef, non_binaries]), Err(BeamError::BadArg)));
            assert!(matches!(match_2(ctx, &[abcdef, atom]), Err(BeamError::BadArg)));

            // An empty subject never matches; a non-byte-aligned subject is `badarg`.
            assert_eq!(match_2(ctx, &[empty, a]).unwrap(), nomatch);
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(match_2(ctx, &[bits, a]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn longest_common_prefix_of_list() {
        with_ctx(|ctx| {
            let cases: [(&[&[u8]], i64); 5] = [
                (&[b"abc", b"abd"], 2),
                (&[b"abc"], 3),
                (&[b"", b""], 0),
                (&[b"abc", b"xyz"], 0),
                (&[b"abcd", b"ab"], 2),
            ];
            for (inputs, expected) in cases {
                let mut elems = Vec::with_capacity(inputs.len());
                for input in inputs {
                    elems.push(bin(ctx, input));
                }
                let list = ctx.list(&elems).unwrap();
                assert_eq!(
                    longest_common_prefix_1(ctx, &[list]).unwrap(),
                    ctx.int(expected).unwrap(),
                    "longest_common_prefix of {inputs:?}"
                );
            }

            // An empty list, a non-binary element, a non-list argument and a
            // non-byte-aligned element are all `badarg`.
            let empty = ctx.list(&[]).unwrap();
            assert!(matches!(
                longest_common_prefix_1(ctx, &[empty]),
                Err(BeamError::BadArg)
            ));
            let atom = ctx.atom("abc").unwrap();
            let with_atom = ctx.list(&[atom]).unwrap();
            assert!(matches!(
                longest_common_prefix_1(ctx, &[with_atom]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                longest_common_prefix_1(ctx, &[atom]),
                Err(BeamError::BadArg)
            ));
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            let with_bits = ctx.list(&[bits, bits]).unwrap();
            assert!(matches!(
                longest_common_prefix_1(ctx, &[with_bits]),
                Err(BeamError::BadArg)
            ));
        });
    }

    // -----------------------------------------------------------------------
    // Compiled patterns (`binary:compile_pattern/1`) — pinned to the oracle
    // (OTP 29.1.1 / erts-17.1)
    // -----------------------------------------------------------------------

    /// The tag atom of a compiled `{Tag, Ref}` handle.
    fn compiled_tag(ctx: &BifContext<'_>, handle: Eterm) -> String {
        assert!(access::is_tuple(handle) && access::tuple_size(handle) == 2);
        let tag = access::tuple_element(handle, 0);
        assert!(access::is_atom(tag), "the tag must be an atom");
        ctx.atoms().name(atom_val(tag)).expect("a seeded tag atom").to_owned()
    }

    /// The rendered form of a BIF result. Rendering is the structural comparison:
    /// a `{Pos, Len}` tuple or a result list is a *fresh* heap block, so term
    /// identity would never compare two equal results.
    fn render(ctx: &mut BifContext<'_>, term: Eterm) -> String {
        crate::print::format_term(ctx, term, false).expect("the result renders")
    }

    /// The `{Tag, Ref}` tuple a forged compiled pattern would be.
    fn handle(ctx: &mut BifContext<'_>, tag: &str, reference: Eterm) -> Eterm {
        let tag = ctx.atom(tag).expect("a seeded tag atom");
        ctx.tuple(&[tag, reference]).expect("the tuple fits")
    }

    /// The compiled `ac` handle for the alternative binaries `parts`.
    fn compile_ac(ctx: &mut BifContext<'_>, parts: &[&[u8]]) -> Eterm {
        let elems: Vec<Eterm> = parts.iter().map(|p| bin(ctx, p)).collect();
        let list = ctx.list(&elems).expect("the list fits");
        compile_pattern_1(ctx, &[list]).expect("a valid pattern")
    }

    #[test]
    fn compile_pattern_tags_one_string_bm_and_several_ac() {
        with_ctx(|ctx| {
            // A binary, whatever its length, is a single search string ⇒ `bm`
            // (the C `words == 1` ⇒ `am_bm`).
            for pat in [&b"a"[..], &b"ab"[..], &b"abcdef"[..]] {
                let search = bin(ctx, pat);
                let compiled = compile_pattern_1(ctx, &[search]).unwrap();
                assert_eq!(compiled_tag(ctx, compiled), "bm", "pattern {pat:?}");
            }

            // A **one-element list** is still a single search string ⇒ `bm`
            // (regression: `binary:compile_pattern([<<"ab">>])` ⇒ `{bm, _}`).
            let one = bin(ctx, b"ab");
            let one_list = ctx.list(&[one]).unwrap();
            let compiled = compile_pattern_1(ctx, &[one_list]).unwrap();
            assert_eq!(compiled_tag(ctx, compiled), "bm");

            // Two or more search strings ⇒ `ac` (the Aho-Corasick trie), even for
            // duplicates or prefix-overlapping alternatives.
            let cases: [&[&[u8]]; 4] =
                [&[&b"a"[..], &b"b"[..]], &[&b"ab"[..], &b"cd"[..]], &[&b"a"[..], &b"a"[..]], &[&b"a"[..], &b"ab"[..]]];
            for parts in cases {
                let compiled = compile_ac(ctx, parts);
                assert_eq!(compiled_tag(ctx, compiled), "ac", "alternatives {parts:?}");
            }
        });
    }

    #[test]
    fn compile_pattern_rejects_every_bad_argument() {
        with_ctx(|ctx| {
            // An empty binary leaves the C's `characters == 0` ⇒ `badarg`.
            let empty = bin(ctx, &[]);
            assert!(matches!(compile_pattern_1(ctx, &[empty]), Err(BeamError::BadArg)));
            // The empty list.
            let nil = ctx.list(&[]).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[nil]), Err(BeamError::BadArg)));
            // A list holding an empty binary (a zero-length search string).
            let with_empty = ctx.list(&[empty]).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[with_empty]), Err(BeamError::BadArg)));

            // A non-byte-aligned binary (`TAIL_BITS != 0`), alone and as an element.
            let bits = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert!(matches!(compile_pattern_1(ctx, &[bits]), Err(BeamError::BadArg)));
            let with_bits = ctx.list(&[bits]).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[with_bits]), Err(BeamError::BadArg)));

            // A non-binary element (an atom), and an improper tail.
            let atom = ctx.atom("a").unwrap();
            let with_atom = ctx.list(&[atom]).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[with_atom]), Err(BeamError::BadArg)));
            let a = bin(ctx, b"a");
            let improper = ctx.cons(a, a).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[improper]), Err(BeamError::BadArg)));

            // A non-list, non-binary argument (an atom, a tuple, a map …) never
            // grows `characters`, so it is `badarg`.
            assert!(matches!(compile_pattern_1(ctx, &[atom]), Err(BeamError::BadArg)));
            let tuple = ctx.tuple(&[a, a]).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[tuple]), Err(BeamError::BadArg)));
            // … including a compiled handle itself (a tuple is not re-compilable).
            let compiled = compile_pattern_1(ctx, &[a]).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[compiled]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn compile_pattern_rejects_characters_like_the_oracle() {
        with_ctx(|ctx| {
            // erts-17.1 accepts neither a bare character nor a list of characters,
            // despite what older OTP docs suggest: `binary:compile_pattern(65)` and
            // `binary:compile_pattern([65, 66])` are both `error:badarg` (an
            // element must be a *binary*, so a small integer — in `0..=255` or
            // not — fails the `is_bitstring` check).
            let byte = ctx.int(65).unwrap();
            assert!(matches!(compile_pattern_1(ctx, &[byte]), Err(BeamError::BadArg)));
            let chars = byte_list(ctx, &[65, 66]);
            assert!(matches!(compile_pattern_1(ctx, &[chars]), Err(BeamError::BadArg)));
            // An out-of-range element is rejected the same way, not differently.
            let out_of_range = byte_list(ctx, &[256]);
            assert!(matches!(compile_pattern_1(ctx, &[out_of_range]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn compile_pattern_interns_its_tag_atom_on_demand() {
        // The oracle's `bm`/`ac` are pre-interned from `atom.names`, but the Rust
        // runtime's boot seed need not hold them: the BIF resolves the tag through
        // `BifContext::intern_atom`, so an absent name is interned via the
        // `AtomInterner` seam instead of failing with `TermNotFound`.
        let mut heap = Heap::new(4096).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["ab", "cd"]); // deliberately without `bm`/`ac`

        let mut interner = Interner { table: std::cell::RefCell::new(AtomTable::new()) };
        let (bm_tag, ac_tag) = {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_atom_interner(&mut interner);
            let ab = bin(&mut ctx, b"ab");
            let cd = bin(&mut ctx, b"cd");

            // One search string ⇒ `bm`, interned on demand.
            let bm = compile_pattern_1(&mut ctx, &[ab]).expect("interns `bm` on demand");
            let bm_tag = access::tuple_element(bm, 0);
            assert!(access::is_atom(bm_tag));
            // Several search strings ⇒ `ac`, likewise.
            let list = ctx.list(&[ab, cd]).unwrap();
            let ac = compile_pattern_1(&mut ctx, &[list]).expect("interns `ac` on demand");
            (bm_tag, access::tuple_element(ac, 0))
        };

        // `ctx` is gone, so the interner can be inspected: the tags landed under
        // exactly the oracle's names.
        let names = interner.table.borrow();
        assert_eq!(names.name(atom_val(bm_tag)), Some("bm"));
        assert_eq!(names.name(atom_val(ac_tag)), Some("ac"));
        drop(names);

        // Without an interner an unresolvable tag atom is the typed
        // `TermNotFound`, never a fabricated index (the `atoms` table above has
        // neither name and this context has no seam).
        let mut bare = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let ab = bin(&mut bare, b"ab");
        assert!(matches!(
            compile_pattern_1(&mut bare, &[ab]),
            Err(BeamError::TermNotFound)
        ));
    }

    /// An `AtomInterner` over its own table (the `convert.rs` mock): the table
    /// *is* the shared atom table in a real runtime.
    struct Interner {
        table: std::cell::RefCell<AtomTable>,
    }

    impl crate::context::AtomInterner for Interner {
        fn intern(&mut self, name: &str) -> beam_term::Atom {
            self.table.borrow_mut().intern(name)
        }
    }

    #[test]
    fn compiled_pattern_searches_identically_to_the_raw_pattern() {
        with_ctx(|ctx| {
            let subject = bin(ctx, b"xabycdab");

            // `bm` (a single search string).
            let raw = bin(ctx, b"ab");
            let bm = compile_pattern_1(ctx, &[raw]).unwrap();
            let compiled_match = match_2(ctx, &[subject, bm]).unwrap();
            assert_eq!(render(ctx, compiled_match), "{1,2}");
            let raw_match = match_2(ctx, &[subject, raw]).unwrap();
            assert_eq!(render(ctx, compiled_match), render(ctx, raw_match));
            let compiled_matches = matches_2(ctx, &[subject, bm]).unwrap();
            let raw_matches = matches_2(ctx, &[subject, raw]).unwrap();
            assert_eq!(render(ctx, compiled_matches), render(ctx, raw_matches));
            let compiled_split = split_2(ctx, &[subject, bm]).unwrap();
            let raw_split = split_2(ctx, &[subject, raw]).unwrap();
            assert_eq!(render(ctx, compiled_split), render(ctx, raw_split));

            // `ac` (several search strings) takes the same leftmost-longest pick.
            let ab = bin(ctx, b"ab");
            let cd = bin(ctx, b"cd");
            let raw_alts = ctx.list(&[ab, cd]).unwrap();
            let ac = compile_pattern_1(ctx, &[raw_alts]).unwrap();
            let compiled_ac_match = match_2(ctx, &[subject, ac]).unwrap();
            let raw_ac_match = match_2(ctx, &[subject, raw_alts]).unwrap();
            assert_eq!(render(ctx, compiled_ac_match), render(ctx, raw_ac_match));
            let compiled_ac_matches = matches_2(ctx, &[subject, ac]).unwrap();
            let raw_ac_matches = matches_2(ctx, &[subject, raw_alts]).unwrap();
            assert_eq!(render(ctx, compiled_ac_matches), render(ctx, raw_ac_matches));

            // `match/3` and `split/3` options are unchanged, and a compiled `bm`
            // handle is accepted wherever a raw pattern is.
            let sc = scope(ctx, 1, 6);
            let scoped = ctx.list(&[sc]).unwrap();
            let compiled_scoped = match_3(ctx, &[subject, bm, scoped]).unwrap();
            let raw_scoped = match_3(ctx, &[subject, raw, scoped]).unwrap();
            assert_eq!(render(ctx, compiled_scoped), render(ctx, raw_scoped));
            let global_atom = ctx.atom("global").unwrap();
            let global = ctx.list(&[global_atom]).unwrap();
            let compiled_global = split_3(ctx, &[subject, bm, global]).unwrap();
            let raw_global = split_3(ctx, &[subject, raw, global]).unwrap();
            assert_eq!(render(ctx, compiled_global), render(ctx, raw_global));

            // Oracle-pinned: `binary:split(<<"aba">>,
            // binary:compile_pattern([<<"a">>]), [global, trim_all])` ⇒ `[<<"b">>]`.
            let a = bin(ctx, b"a");
            let aba = bin(ctx, b"aba");
            let one = ctx.list(&[a]).unwrap();
            let trim = compile_pattern_1(ctx, &[one]).unwrap();
            assert_eq!(compiled_tag(ctx, trim), "bm");
            let trim_all = ctx.atom("trim_all").unwrap();
            let opts = ctx.list(&[global_atom, trim_all]).unwrap();
            let trimmed = split_3(ctx, &[aba, trim, opts]).unwrap();
            assert_eq!(render(ctx, trimmed), "[<<98>>]");

            // A miss: `nomatch` for `match/2`, `[]` for `matches/2`.
            let zz = bin(ctx, b"zz");
            let miss = compile_pattern_1(ctx, &[zz]).unwrap();
            let missed = match_2(ctx, &[subject, miss]).unwrap();
            assert_eq!(render(ctx, missed), "nomatch");
            let none = matches_2(ctx, &[subject, miss]).unwrap();
            assert_eq!(render(ctx, none), "[]");
        });
    }

    #[test]
    fn forged_and_mistagged_compiled_patterns_are_badarg() {
        with_ctx(|ctx| {
            let subject = bin(ctx, b"xabx");
            let raw = bin(ctx, b"ab");
            let bm = compile_pattern_1(ctx, &[raw]).unwrap();
            let bm_ref = access::tuple_element(bm, 1);
            let valid = match_2(ctx, &[subject, bm]).unwrap();
            assert_eq!(render(ctx, valid), "{1,2}");

            // A `Ref` the registry has never handed out (a forged handle) is
            // `badarg` — the oracle's magic-ref check rejects it too.
            let fresh = access::make_ref_thing(ctx.heap(), 7).unwrap();
            let forged = handle(ctx, "bm", fresh);
            assert!(matches!(match_2(ctx, &[subject, forged]), Err(BeamError::BadArg)));
            assert!(matches!(matches_2(ctx, &[subject, forged]), Err(BeamError::BadArg)));
            assert!(matches!(split_2(ctx, &[subject, forged]), Err(BeamError::BadArg)));

            // A `bm` handle under an `ac` tag — and vice versa — is `badarg` (the
            // C magic-ref-destructor check).
            let mistagged_bm = handle(ctx, "ac", bm_ref);
            assert!(matches!(match_2(ctx, &[subject, mistagged_bm]), Err(BeamError::BadArg)));
            let ac = compile_ac(ctx, &[&b"ab"[..], &b"cd"[..]]);
            let ac_ref = access::tuple_element(ac, 1);
            let mistagged_ac = handle(ctx, "bm", ac_ref);
            assert!(matches!(match_2(ctx, &[subject, mistagged_ac]), Err(BeamError::BadArg)));

            // Any other tuple is `badarg`: an unknown tag, a non-ref second
            // element, the wrong arity, a non-atom tag.
            let one = ctx.int(1).unwrap();
            let bm_atom = ctx.atom("bm").unwrap();
            let unknown_tag = handle(ctx, "foo", bm_ref);
            let non_ref = handle(ctx, "bm", one);
            let binary_ref = handle(ctx, "bm", raw);
            let pid_ref = handle(ctx, "bm", subject);
            let arity_one = ctx.tuple(&[bm_atom]).unwrap();
            let arity_three = ctx.tuple(&[bm_atom, bm_ref, one]).unwrap();
            let non_atom_tag = ctx.tuple(&[one, bm_ref]).unwrap();
            for bad in [
                unknown_tag,
                non_ref,
                binary_ref,
                pid_ref,
                arity_one,
                arity_three,
                non_atom_tag,
            ] {
                assert!(
                    matches!(match_2(ctx, &[subject, bad]), Err(BeamError::BadArg)),
                    "a malformed compiled handle must be badarg"
                );
            }
        });
    }

    #[test]
    fn register_installs_every_binary_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("at", 2),
            ("part", 2),
            ("part", 3),
            ("copy", 1),
            ("copy", 2),
            ("list_to_bin", 1),
            ("bin_to_list", 1),
            ("bin_to_list", 2),
            ("bin_to_list", 3),
            ("decode_unsigned", 1),
            ("decode_unsigned", 2),
            ("encode_unsigned", 1),
            ("encode_unsigned", 2),
            ("first", 1),
            ("last", 1),
            ("compile_pattern", 1),
            ("split", 2),
            ("split", 3),
            ("match", 2),
            ("match", 3),
            ("matches", 2),
            ("matches", 3),
            ("longest_common_prefix", 1),
            ("longest_common_suffix", 1),
        ] {
            assert!(reg.contains("binary", name, arity), "missing binary:{name}/{arity}");
        }
        // The `erlang` iolist/bitstring BIFs registered alongside the `binary`
        // module (`bif.tab`: `bif erlang:iolist_to_binary/1`, `iolist_size/1`,
        // `list_to_bitstring/1`, `split_binary/2`).
        for (name, arity) in [
            ("iolist_to_binary", 1),
            ("iolist_size", 1),
            ("list_to_bitstring", 1),
            ("split_binary", 2),
        ] {
            assert!(reg.contains("erlang", name, arity), "missing erlang:{name}/{arity}");
        }
        assert_eq!(reg.len(), 28);
    }
    #[test]
    fn iolist_size_promotes_past_the_small_int_range() {
        with_ctx(|ctx| {
            // The oracle's ordinary results are unchanged small ints.
            // `iolist_size([<<"ab">>,<<"cd">>])` ⇒ `4`, `iolist_size([1,2|<<3>>])` ⇒ `3`.
            let ab = bin(ctx, b"ab");
            let cd = bin(ctx, b"cd");
            let nested = ctx.list(&[ab, cd]).expect("the list fits");
            assert!(access::is_small(iolist_size_1(ctx, &[nested]).unwrap()));
            assert_eq!(signed_value(iolist_size_1(ctx, &[nested]).unwrap()), 4);
            let one_two = byte_list(ctx, &[1, 2]);
            let three = bin(ctx, &[3]);
            let improper = ctx.cons(one_two, three).expect("the cons fits");
            assert_eq!(signed_value(iolist_size_1(ctx, &[improper]).unwrap()), 3);

            // A size beyond the 60-bit small range is a heap **bignum**, not
            // `system_limit` — the shared promotion path (`arith`). A >2^59-byte
            // iolist is not constructible in a test, so the helper is exercised
            // directly: the values are exact and oracle-verified as integers.
            let big = byte_count_term(ctx, u64::MAX).unwrap();
            assert!(!access::is_small(big), "a >60-bit size must be a bignum");
            assert_eq!(crate::print::format_term(ctx, big, false).unwrap(), "18446744073709551615");
            // One past the small-int maximum (`2^59 - 1`) stays representable.
            let edge = byte_count_term(ctx, 1u64 << 59).unwrap();
            assert_eq!(crate::print::format_term(ctx, edge, false).unwrap(), "576460752303423488");
            // The small-range bound itself (`2^59 - 1`) is still a small int.
            let last_small = byte_count_term(ctx, (1u64 << 59) - 1).unwrap();
            assert!(access::is_small(last_small));
            assert_eq!(signed_value(last_small), (1i64 << 59) - 1);
        });
    }
}
