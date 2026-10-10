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

//! `erts_internal`/`erts_debug` BIFs — the term/map helpers, the timers' unit
//! queries and the internal integer parsers.
//!
//! Ground truth (read-only):
//! `../../erts/emulator/beam/erl_map.c` (`erts_internal_term_type_1:3336-3417`,
//! `erts_internal_map_to_tuple_keys_1:3318-3328`,
//! `erts_internal_map_hashmap_children_1:3425-3461`,
//! `erts_internal_map_next_3:3636-3920`), `beam_debug.c:64-83`
//! (`erts_debug_same_2`, `erts_debug_flat_size_1`), `big.c:4112-4355`
//! (`digits_per_small_lookup`, `chars_to_integer`,
//! `erts_internal_binary_to_integer_2`, `erts_internal_list_to_integer_2`),
//! `erl_time_sup.c:2537-2540,2607-2610` (`erts_internal_time_unit_0`,
//! `erts_internal_perf_counter_unit_0`) and `bif.tab:182-187,446-447,694,779-780`.
//!
//! # Roster
//!
//! `erts_debug:same/2`, `erts_debug:flat_size/1`,
//! `erts_internal:term_type/1`, `erts_internal:map_to_tuple_keys/1`,
//! `erts_internal:map_hashmap_children/1`, `erts_internal:map_next/3`,
//! `erts_internal:time_unit/0`, `erts_internal:perf_counter_unit/0`,
//! `erts_internal:binary_to_integer/2`, `erts_internal:list_to_integer/2`.
//!
//! # `erts_debug:same/2` is raw word identity
//!
//! `beam_debug.c:64-68` is literally `return (BIF_ARG_1 == BIF_ARG_2) ? am_true :
//! am_false;` — a comparison of the two `Eterm` words, not of their structure. Two
//! structurally equal boxed terms are `same/2` only when they are *the same object*:
//! the loader's literal-area deduplication makes `same([1],[1])` and
//! `same(#{},#{})` true for literals of one module, while
//! `same(maps:put(a,1,#{}),maps:put(a,1,#{}))` is false (two freshly built maps).
//! Whether two equal-looking terms share a word is the *term/literal
//! representation's* business (the loader owns literal dedup), never this BIF's.
//!
//! # `erts_debug:flat_size/1` is representation-dependent
//!
//! `beam_debug.c:70-83` is `size_object(term)` promoted to a bignum past the
//! small-int range; the algorithm
//! ([`beam_term::size::size_object`], the port of `copy.c:88-263`) is shared, but
//! the *number* depends on the physical representation of the argument. Three node
//! kinds differ between this runtime and the C, so a term graph containing any of
//! them has **no** comparable oracle value and `flat_size/1` reports the typed
//! [`crate::error::unsupported_bif`] instead of a number that would be wrong:
//!
//! 1. **an internal reference** — the M1 `access::make_ref_thing` block is 4 words
//!    (`[header, node, serial, id]`) where C's `ERL_REF_SIZE` is 3;
//! 2. **any fun** — [`beam_term::fun`] inlines the descriptor, so the block is
//!    `1 + desc_words(kind) + num_free` where C's is `ERL_FUN_SIZE + num_free` = 2;
//! 3. **a bitstring longer than C's on-heap limit** (`bit_len > 512`, i.e. > 64
//!    bytes) — this runtime may keep a long binary in an on-heap `ErlHeapBits`
//!    (which counts `2 + ceil(bits/64)`) where C switches to the 8-word
//!    `[BinRef][ErlSubBits]` pair.
//!
//! An external pid/port/ref (whose `node` slot is likewise inlined by this runtime)
//! and any boxed subtag this module does not positively recognize as
//! representation-identical are treated the same way: conservatively divergent,
//! never guessed. Everything else — immediates, cons cells, tuples and native
//! records, floats, bignums, flat maps, **hash maps** (their HAMT node interiors are
//! the ERTS layout, `copy.c:192-214`, which
//! [`beam_term::layout`](beam_term) sizes exactly), on-heap bitstrings up to 512
//! bits and the 8-word refc pair itself — is counted exactly as the C does, because
//! it is the same algorithm over the same frozen layouts (note
//! `heap_bits_size(bits) == 2 + ceil(bits/64)`, identical to
//! [`beam_term::bitstring::heap_bits_size_words`]). Oracle-pinned:
//! `erts_debug:flat_size(maps:from_list([{N,N*10} || N <- lists:seq(1,33)]))` ⇒ `123`,
//! the count this runtime's own 33-pair HAMT produces. A hash map's *trie shape* —
//! and therefore its count — is driven by `erts_map_hash`, which for an atom key
//! reads the atom's runtime index, so an atom-keyed HAMT can legitimately count
//! differently here than in the oracle; that is a differing *input map*, not a
//! differing representation, and the number is this runtime's true flat size.
//!
//! # The internal integer parsers *return* their failure atoms
//!
//! `erts_internal:binary_to_integer/2` and `erts_internal:list_to_integer/2` are
//! the engines behind the public `erlang:*` wrappers. `big.c` ends every failure
//! path in `BIF_RET(am_badarg)` / `BIF_RET(am_big)` / `BIF_RET(am_no_integer)` /
//! `BIF_RET(am_not_a_list)`, so these two BIFs never raise: they return the atoms
//! `badarg`, `big`, `no_integer` / `not_a_list`, a small integer, or the
//! `{Integer, Rest}` tuple. Verified live on erts-17.1:
//! `try erts_internal:binary_to_integer(<<>>,10) catch C:R -> {C,R} end` ⇒
//! `badarg` (a *returned* atom; the public `erlang:binary_to_integer(<<>>,10)`
//! wrapper is what raises `error:badarg`). This module is faithful to that.
//!
//! # Gaps
//!
//! * `erts_internal:term_to_string/2` is **not registered**: `bif.c:4154-4194`
//!   formats through the internal `%.*T` printer, whose output diverges from this
//!   crate's `src/print.rs` renderer in several observable ways (a float as
//!   `1.500000e+00`, a fun as `#Fun<erl_eval.42.130099583>`, the `"... "`
//!   truncation tail), so no faithful implementation exists here.
//! * `erts_debug:size/1` (the *shared* sizer, `size_shared`), `get_internal_state/1`,
//!   `set_internal_state/2` and `disassemble/1` are not registered (they need the
//!   shared-term walk, the emulator's internal state and the BEAM code loader).
//! * Typed gaps *inside* a registered BIF (each returns
//!   [`crate::error::unsupported_bif`], never a wrong value):
//!   `map_hashmap_children/1` on a hash-map node (the raw slot walk,
//!   `erl_map.c:3425-3461`, needs a node-interior accessor the LCA does not expose —
//!   see its own doc); `flat_size/1` for the divergent representations above.
//!   `term_type/1` distinguishes the on-heap (`heap_binary`) from the off-heap
//!   (`sub_binary`) bitstring by the layout rule itself; the C `refc_binary` name is
//!   unreachable because a bare `BinRef` header is not `is_bitstring`, so such a term
//!   falls into the unrecognized-tag gap (this runtime builds no standalone `BinRef`
//!   term anyway). `map_next/3` walks both map representations; only its continuation
//!   *path encoding* and per-call chunk size are this implementation's own (see
//!   [`map_next_3`]).
use beam_common::{BeamError, Result};
use beam_term::prelude::{boxed_val, make_small, signed_value};
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, structured_error, unsupported_bif};
use crate::registry::BifRegistry;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `unsigned_val(x)` for a small integer: the 60-bit pattern as an unsigned value
/// (`erl_term.h`). `signed_value` sign-extends, and the low 60 bits of a
/// two's-complement 60-bit value are the same either way.
fn unsigned_val(x: Eterm) -> u64 {
    signed_value(x) as u64
}

/// `x` is the small integer matching the ASCII `byte` — the C `CAR ==
/// make_small(ch)` sign test of `erts_internal_list_to_integer_2` (a list *byte*
/// only matches via a small integer, never an atom).
fn is_small_char(x: Eterm, byte: u8) -> bool {
    access::is_small(x) && signed_value(x) == byte as i64
}

/// `term` is exactly the atom `name`.
fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// Returns the atom `name` (`ctx.intern_atom`, so the runtime's `AtomInterner` is
/// consulted for a name this crate has not interned).
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Result<Eterm> {
    ctx.intern_atom(name)
}

/// `c2int_is_invalid_char(c, base)` (`big.c:4065-4076`): `false` when `c` is a digit
/// of `base`.
///
/// The letter arms are the C's `ch < 'A' + base - 10` / `ch < 'a' + base - 10`: the
/// highest valid letter digit is `'A' + (base - 11)` (e.g. `'F'` for base 16), so the
/// exclusive upper bound is `'A' + base - 10` (e.g. `'G'`).
fn c2int_is_invalid_char(c: u8, base: u32) -> bool {
    let c = c as u32;
    if base <= 10 {
        !(c >= '0' as u32 && c < '0' as u32 + base)
    } else {
        !((c >= '0' as u32 && c <= '9' as u32)
            || (c >= 'A' as u32 && c < 'A' as u32 + base - 10)
            || (c >= 'a' as u32 && c < 'a' as u32 + base - 10))
    }
}

/// `c2int_digit_from_base(c)` (`big.c`): the digit value of a character already
/// known to be valid for the base.
fn c2int_digit_from_base(c: u8) -> u32 {
    let c = c as u32;
    if c <= '9' as u32 {
        c - '0' as u32
    } else if c <= 'Z' as u32 {
        10 + (c - 'A' as u32)
    } else {
        10 + (c - 'a' as u32)
    }
}

/// The 64-bit `digits_per_small_lookup` row of `big.c:4115-4127` (bases 2..=36):
/// the largest digit count that still fits a 60-bit small integer.
const DIGITS_PER_SMALL: [u8; 35] = [
    59, 37, 29, 25, 22, 21, 19, 18, 17, 17, 16, 15, 15, 15, 14, 14, 14, 13, 13, 13, 13, 13, 12, 12,
    12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 11,
];

/// `get_digits_per_small(base)`: the table row above; `base` is `2..=36`.
fn digits_per_small(base: u32) -> usize {
    DIGITS_PER_SMALL[(base - 2) as usize] as usize
}

/// The `2..=36` base argument of the two integer parsers, or `None` (the caller
/// returns the `badarg` atom). Mirrors `base = unsigned_val(BIF_ARG_2)` followed
/// by the `base < 2 || base > 36` test, so a non-small and a negative base are both
/// rejected.
fn parse_base(ctx: &BifContext<'_>, term: Eterm) -> Option<u32> {
    if !access::is_small(term) {
        return None;
    }
    let base = unsigned_val(term);
    let _ = ctx;
    if (2..=36).contains(&base) {
        Some(base as u32)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// erts_debug
// ---------------------------------------------------------------------------

/// `erts_debug:same/2` — raw `Eterm` word equality (`beam_debug.c:64-68`).
fn same_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(args[0] == args[1])
}

/// Whether the term graph rooted at `term` contains a node whose word count under
/// **this** runtime's representation cannot be compared with the C's `size_object`
/// (see the module docs). Conservative: an unrecognized boxed subtag counts as
/// divergent.
fn has_representation_divergence(term: Eterm) -> bool {
    // A generous node budget: terms are acyclic, so this only bounds pathological
    // graphs, and exceeding it is reported as a divergence (a typed gap) rather than
    // a partial answer.
    const MAX_NODES: usize = 1 << 20;
    let mut work: Vec<Eterm> = vec![term];
    let mut visited = 0usize;
    while let Some(current) = work.pop() {
        visited += 1;
        if visited > MAX_NODES {
            return true;
        }
        // Immediates: identical word count (zero) in both representations.
        if access::is_small(current)
            || access::is_atom(current)
            || access::is_nil(current)
            || access::is_internal_pid(current)
            || access::is_internal_port(current)
        {
            continue;
        }
        if access::is_list(current) {
            work.push(access::car(current));
            work.push(access::cdr(current));
            continue;
        }
        if access::is_float(current) || access::is_big(current) {
            continue;
        }
        // The three documented divergences, plus the unverifiable external terms.
        if access::is_internal_ref(current)
            || access::is_function(current)
            || access::is_external_pid(current)
            || access::is_external_port(current)
            || access::is_external_ref(current)
        {
            return true;
        }
        if access::is_bitstring(current) {
            match access::bitstring_bit_size(current) {
                Ok(bits) if bits <= 512 => continue,
                // A long bitstring: this runtime may hold it on the heap where C uses
                // the 8-word refc pair (and a corrupt size has no oracle value either).
                _ => return true,
            }
        }
        if access::is_tuple(current) {
            for i in 0..access::tuple_size(current) {
                work.push(access::tuple_element(current, i));
            }
            continue;
        }
        if access::is_record(current) {
            match beam_codec::term::record_values(current) {
                Ok(values) => {
                    work.extend_from_slice(values);
                    continue;
                }
                Err(_) => return true,
            }
        }
        if access::is_map(current) {
            // Both representations: a flat map (`map_pairs_storage_order` reads its
            // keys tuple) and a HAMT (`size_object` sizes the frozen ERTS node
            // interiors, so only the keys/values can carry a divergence).
            match access::map_pairs_storage_order(current) {
                Ok(pairs) => {
                    for (key, value) in pairs {
                        work.push(key);
                        work.push(value);
                    }
                    continue;
                }
                Err(_) => return true,
            }
        }
        // Any subtag not positively recognized above: its word count is not
        // verifiable through the safe API.
        return true;
    }
    false
}

/// `erts_debug:flat_size/1` — `size_object/1` promoted past the small-int range
/// (`beam_debug.c:70-83`), with a typed gap for the divergent representations.
fn flat_size_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if has_representation_divergence(args[0]) {
        return Err(unsupported_bif());
    }
    let size = beam_term::size::size_object(args[0]);
    let size = i64::try_from(size).map_err(|_| BeamError::SystemLimit)?;
    arith::int_from_i64(ctx, size)
}

// ---------------------------------------------------------------------------
// erts_internal: the term/map helpers
// ---------------------------------------------------------------------------

/// `erts_internal:term_type/1` — the atom naming the argument's physical tag
/// (`erl_map.c:3336-3417`). An unrecognized tag is the typed gap, never an invented
/// name.
fn term_type_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    let name = if access::is_small(term) {
        "fixnum"
    } else if access::is_atom(term) {
        "atom"
    } else if access::is_nil(term) {
        "nil"
    } else if access::is_internal_pid(term) {
        "pid"
    } else if access::is_internal_port(term) {
        "port"
    } else if access::is_list(term) {
        // `TAG_PRIMARY_LIST`: cons cells, proper and improper.
        "list"
    } else if access::is_tuple(term) {
        "tuple"
    } else if access::is_record(term) {
        // `ARITYVAL_SUBTAG` and `RECORD_SUBTAG` both report `tuple`.
        "tuple"
    } else if access::is_float(term) {
        "hfloat"
    } else if access::is_big(term) {
        "bignum"
    } else if access::is_flatmap(term) {
        "flatmap"
    } else if access::is_map(term) {
        // A HAMT node (an inner node-bitmap node is `hashmap_node` in C, but is not
        // reachable from a top-level argument in this runtime).
        "hashmap"
    } else if access::is_bitstring(term) {
        // The C reports the header subtag: `HEAP_BITS_SUBTAG` ⇒ `heap_binary`,
        // `SUB_BITS_SUBTAG` ⇒ `sub_binary`. Both satisfy `is_bitstring` and their
        // layouts differ in exactly one way — an `ErlHeapBits` stores its data inline
        // at word 2, an `ErlSubBits` points at an off-heap payload — so the data base
        // pointer identifies the representation (no `unsafe` needed:
        // `beam_codec::term::bitstring_parts` is the safe reader and `boxed_val` is a
        // safe const accessor).
        match beam_codec::term::bitstring_parts(term) {
            Ok((base, _, _)) if base == unsafe_base_of(term) => "heap_binary",
            Ok(_) => "sub_binary",
            Err(err) => return Err(err),
        }
    } else if access::is_function(term) {
        match beam_codec::term::fun_kind_term(term) {
            Ok(beam_codec::term::FunKind::Local) => "fun",
            Ok(_) => "export",
            Err(err) => return Err(err),
        }
    } else if access::is_internal_ref(term) {
        "reference"
    } else if access::is_external_ref(term) {
        "external_reference"
    } else if access::is_external_pid(term) {
        "external_pid"
    } else if access::is_external_port(term) {
        "external_port"
    } else {
        return Err(unsupported_bif());
    };
    atom(ctx, name)
}

/// The address an `ErlHeapBits`'s inline data starts at — word 2 of the term's own
/// block (`beam_term::bitstring::bitstring_parts`: base `= boxed_val(t) + 2`).
fn unsafe_base_of(term: Eterm) -> *const u8 {
    // `boxed_val` is a safe const accessor here and for `is_bitstring`-verified terms.
    boxed_val(term).wrapping_add(2).cast::<u8>()
}

/// `erts_internal:map_to_tuple_keys/1` — a flat map's key tuple
/// (`erl_map.c:3318-3328`). The C returns the map's own key-tuple object; this
/// builds a fresh tuple that is `=:=`-equal to it.
///
/// A **hash map is `badarg`** — not `{badmap, _}`: the C reaches a map's keys only
/// through the flat-map head's `keys` word, so `erts_internal_map_to_tuple_keys_1`
/// errors explicitly for a HAMT (`erl_map.c:3322-3323`). Oracle-pinned:
/// `erts_internal:map_to_tuple_keys(maps:from_list([{N,N} || N <- lists:seq(1,33)]))`
/// ⇒ `error:badarg` (and `#{}`⇒`{}`, `[1,2]`⇒`error:{badmap,[1,2]}`).
fn map_to_tuple_keys_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if access::is_flatmap(term) {
        let (keys, _values) = access::flatmap_pairs(term)?;
        return ctx.tuple(&keys);
    }
    if access::is_map(term) {
        return Err(badarg());
    }
    Err(structured_error(ctx, "badmap", &[term])?)
}

/// `erts_internal:map_hashmap_children/1` — the slot values of one hashmap node
/// (`erl_map.c:3425-3461`). Only the two non-node cases are reproducible here.
///
/// The C returns the node's own **in-heap slot words** — every leaf cons cell, every
/// sub-node term and every collision tuple of the node reached by the argument — in
/// reverse slot order. Reaching them needs a node-interior accessor: a head-array
/// node is `[hdr][size][16 slots]`, a bitmap node `[hdr][(size)][n slots]` and a
/// collision node a plain arityval tuple, and the slot words are pointers into the
/// existing map. `beam_term::map`'s public surface is pairs-level only
/// (`map_pairs`/`map_pairs_storage_order`/`map_size`/`map_get`) and this crate
/// confines `unsafe` to `src/access.rs`, so a node argument stays the typed gap.
fn map_hashmap_children_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if !access::is_map(term) {
        return Err(structured_error(ctx, "badmap", &[term])?);
    }
    if access::is_flatmap(term) {
        // `HAMT_SUBTAG_HEAD_FLATMAP` ⇒ the C raises `badarg`.
        return Err(badarg());
    }
    Err(unsupported_bif())
}

/// The HAMT positional walk's per-call chunk cap, `MAP_SMALL_MAP_LIMIT`
/// (`erl_map.h:96`) — the oracle's deterministic *first*-call ceiling
/// (`erl_map.c:3799-3801`). [`crate::maps`]'s `next/1` uses the same cap and the
/// same storage-order offset encoding, so the two implementations of this BIF agree.
const HAMT_CHUNK: usize = 32;

/// `erts_internal:map_next/3` — one step of `maps:next/1`'s iteration
/// (`erl_map.c:3636-4122`).
///
/// Three shapes, decided by the arguments exactly as the C does:
///
/// * **ordered iterator** (`Mode == iterator` and a list/NIL `Path`): `Path` is the
///   key list `maps:iterator(M, ordered)` built, and the result is
///   `{K, maps:get(K, Map), …}` over it, one triple per key, terminating in `none`.
/// * **iterator** (any other `Path` with `Mode == iterator`): the look-ahead chain
///   `{K0,V0,{K1,V1,…}}`, ending in `none` — or in the `[NextPath | Map]`
///   continuation a caller (`maps:next/1`) resumes from.
/// * **list mode** (`Mode` a list or NIL, `Path` an integer): the `{K,V}` pairs are
///   prepended onto `Mode` and the result is `[NextPath, Map | Accumulated]` while a
///   chunk remains (`maps.erl:526-542` accumulates it) or the plain list once the map
///   is exhausted.
///
/// Both positional walks read the map's pairs in the order
/// [`crate::access::map_pairs_storage_order`] reports — a flat map's key-tuple order,
/// a HAMT's slot-traversal order, i.e. the oracle's `maps:to_list/1` order. A flat
/// map (≤ [`HAMT_CHUNK`] pairs) always emits its whole chain, ignoring the offset
/// beyond the bound check; a HAMT is chunked.
///
/// # Deviation: the `Path` is an opaque storage-order offset and the chunking is fixed
///
/// The C's `Path` is an internal trie-nibble encoding (`erl_map.c:4058-4111`, a
/// bignum past 15 levels, with "offset from end" semantics in list mode) and, after
/// the first call, its element count is `min(remaining reductions / 4, size)`
/// (`erl_map.c:3783-3792`) — a function of the *process's* reduction budget, so two
/// identical calls in one process can return different amounts. Neither is
/// reproducible here: no node-interior accessor exists (see
/// [`map_hashmap_children_1`]) and a [`BifContext`] carries no reduction counter.
/// This implementation therefore emits the oracle's *deterministic* first-call cap
/// ([`HAMT_CHUNK`]) and then one further [`HAMT_CHUNK`] per call, using its own
/// offset (an integer, so `maps.erl`'s `is_integer(Path)` guard accepts it) as the
/// continuation. The **pair sequence**, the **order** and the terminal `none` are
/// the oracle's; only the intermediate chunk boundaries and the continuation values
/// differ, and only for a map with more than 2×[`HAMT_CHUNK`] pairs (below that the
/// chunking coincides). `crate::maps`'s `next/1` uses the same encoding, so
/// `maps:next/1` and `maps:to_list/1` agree end-to-end.
fn map_next_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (path, map, mode) = (args[0], args[1], args[2]);
    // (1) A non-map is a plain `badarg` here — *not* `{badmap, _}` (`erl_map.c:3644`).
    if !access::is_map(map) {
        return Err(badarg());
    }
    // (2) `iterator` or a list/NIL accumulator; anything else is `badarg`.
    let iterator = is_atom_named(ctx, mode, "iterator");
    if !iterator && !access::is_nil(mode) && !access::is_list(mode) {
        return Err(badarg());
    }

    // (3) Ordered iterator: the path is the key list to visit.
    if iterator && (access::is_nil(path) || access::is_list(path)) {
        return ordered_next(ctx, path, map);
    }

    // (4) The positional walks. The offset must be a small integer no greater than
    // the pair count — a negative small integer has a huge unsigned value and is
    // therefore a `badarg`, exactly as the C's `n < unsigned_val(Path)` test.
    let pairs = access::map_pairs_storage_order(map)?;
    let n = pairs.len();
    if !access::is_small(path) || (n as u64) < unsigned_val(path) {
        return Err(badarg());
    }
    let offset = unsigned_val(path) as usize;

    if access::is_flatmap(map) {
        // A flat map's chain is emitted whole, whatever the offset.
        if iterator {
            let none = atom(ctx, "none")?;
            return chain(ctx, &pairs, 0, n, none);
        }
        let mut result = mode;
        for i in (0..n).rev() {
            let pair = ctx.tuple(&[pairs[i].0, pairs[i].1])?;
            result = ctx.cons(pair, result)?;
        }
        return Ok(result);
    }
    if iterator {
        let end = core::cmp::min(offset + HAMT_CHUNK, n);
        let terminal = if end < n {
            ctx.cons(ctx.int(end as i64)?, map)?
        } else {
            atom(ctx, "none")?
        };
        return chain(ctx, &pairs, offset, end, terminal);
    }

    // List mode: the chunk *ending* at `to`. The C traverses the trie backwards
    // (`erl_map.c:3975-4054`) and `CONS`es each pair onto the accumulator, so the
    // first call yields the map's *trailing* chunk and later calls prepend earlier
    // ones — the accumulated list ends up in storage order. Path `0` is that first
    // call; any other path resumes one chunk further down.
    let (from, to) = if offset == 0 {
        (n.saturating_sub(HAMT_CHUNK), n)
    } else {
        (offset.saturating_sub(HAMT_CHUNK), offset)
    };
    let mut result = mode;
    for i in (from..to).rev() {
        let pair = ctx.tuple(&[pairs[i].0, pairs[i].1])?;
        result = ctx.cons(pair, result)?;
    }
    if from > 0 {
        let next = ctx.int(from as i64)?;
        result = ctx.cons(map, result)?;
        result = ctx.cons(next, result)?;
    }
    Ok(result)
}

/// Folds the pairs `from..end` (ascending) into the iterator-mode look-ahead chain
/// `{K0,V0,{K1,V1,…}}`, ending in `terminal`.
fn chain(
    ctx: &mut BifContext<'_>,
    pairs: &[(Eterm, Eterm)],
    from: usize,
    end: usize,
    terminal: Eterm,
) -> Result<Eterm> {
    let mut result = terminal;
    for i in (from..end).rev() {
        result = ctx.tuple(&[pairs[i].0, pairs[i].1, result])?;
    }
    Ok(result)
}

/// The ordered-iterator walk (`erl_map.c:3656-3707`): for every key of `keys_path`,
/// a `{Key, Value, …}` triple whose value is `maps:get(Key, Map)` — any map kind,
/// flat or hash — terminating in `none`. A `NIL` path yields `none` itself.
///
/// # Errors
///
/// `badarg` for a key the map does not hold (the C's `ordered_badarg`).
fn ordered_next(ctx: &mut BifContext<'_>, keys_path: Eterm, map: Eterm) -> Result<Eterm> {
    let mut keys: Vec<Eterm> = Vec::new();
    let mut current = keys_path;
    while access::is_list(current) {
        keys.push(access::car(current));
        current = access::cdr(current);
    }
    // `current` is `NIL` here: the caller admitted only a proper list or `NIL`.
    // Look every value up before building, so no `ctx.atoms()` borrow is held across
    // a `ctx.tuple` allocation.
    let mut values: Vec<Eterm> = Vec::with_capacity(keys.len());
    for &key in &keys {
        match access::map_get(map, key, ctx.atoms())? {
            Some(value) => values.push(value),
            None => return Err(badarg()),
        }
    }
    let mut result = atom(ctx, "none")?;
    for i in (0..keys.len()).rev() {
        result = ctx.tuple(&[keys[i], values[i], result])?;
    }
    Ok(result)
}
/// `erts_internal:time_unit/0` — the runtime's native time unit in units per second
/// (`erl_time_sup.c:2537-2540`, `ERTS_MONOTONIC_TIME_UNIT`). This runtime fixes the
/// native unit to **nanoseconds** (see `src/time.rs`), as on this Linux host;
/// oracle-pinned: `erts_internal:time_unit()` ⇒ `1000000000`.
fn time_unit_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.int(1_000_000_000)
}

/// `erts_internal:perf_counter_unit/0` — `erts_sys_perf_counter_unit()`
/// (`erl_time_sup.c:2607-2610`), nanoseconds on Linux; oracle-pinned ⇒ `1000000000`.
fn perf_counter_unit_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.int(1_000_000_000)
}

/// `erts_internal:binary_to_integer/2` — `chars_to_integer`
/// (`big.c:4162-4229`). Always returns an integer or one of the atoms `big` /
/// `badarg` (never a raise — see the module docs).
fn binary_to_integer_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let Some(base) = parse_base(ctx, args[1]) else {
        return atom(ctx, "badarg");
    };
    // A byte-aligned binary; anything else (including a sub-bit binary this crate
    // cannot read) is the `badarg` atom.
    if !access::is_binary(args[0]) {
        return atom(ctx, "badarg");
    }
    let bytes = match access::binary_bytes(args[0]) {
        Ok(bytes) => bytes,
        Err(_) => return atom(ctx, "badarg"),
    };

    let mut digits = &bytes[..];
    if digits.is_empty() {
        return atom(ctx, "badarg");
    }
    let mut neg = false;
    if digits[0] == b'-' {
        neg = true;
        digits = &digits[1..];
    } else if digits[0] == b'+' {
        digits = &digits[1..];
    }
    if digits.is_empty() {
        return atom(ctx, "badarg");
    }
    // Trim leading zeroes; all zeros is `0`.
    let mut start = 0usize;
    while start < digits.len() && digits[start] == b'0' {
        start += 1;
        if start == digits.len() {
            return Ok(make_small(0));
        }
    }
    let digits = &digits[start..];
    if digits.len() > digits_per_small(base) {
        return atom(ctx, "big");
    }
    let mut acc: i64 = 0;
    if base <= 10 {
        for &b in digits {
            let digit = (b as u32).wrapping_sub('0' as u32);
            if digit >= base {
                return atom(ctx, "badarg");
            }
            acc = acc
                .wrapping_mul(base as i64)
                .wrapping_add(i64::from(digit));
        }
    } else {
        for &b in digits {
            if c2int_is_invalid_char(b, base) {
                return atom(ctx, "badarg");
            }
            acc = acc
                .wrapping_mul(base as i64)
                .wrapping_add(i64::from(c2int_digit_from_base(b)));
        }
    }
    if neg {
        acc = -acc;
    }
    if !(-(1i64 << 59)..(1i64 << 59)).contains(&acc) {
        // A 60-bit bound is guaranteed by the digit-count check above; this keeps the
        // small-int construction total rather than silently truncating.
        return Err(BeamError::SystemLimit);
    }
    Ok(make_small(acc))
}

/// `erts_internal:list_to_integer/2` (`big.c:4258-4355`). Returns a small integer,
/// the `{Integer, Rest}` tuple, or one of the atoms `big` / `no_integer` /
/// `not_a_list` / `badarg` (never a raise — see the module docs).
fn list_to_integer_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let mut list = args[0];
    if access::is_nil(list) {
        return atom(ctx, "no_integer");
    }
    if !access::is_list(list) {
        return atom(ctx, "not_a_list");
    }
    let Some(base) = parse_base(ctx, args[1]) else {
        return atom(ctx, "badarg");
    };

    // A leading `-`/`+` (a small integer only — an atom or float never matches).
    let mut neg = false;
    if is_small_char(access::car(list), b'-') {
        neg = true;
        list = access::cdr(list);
    } else if is_small_char(access::car(list), b'+') {
        list = access::cdr(list);
    }

    // Zero-skipping phase with its own counter `n` (the C `n` of lines 4292-4323).
    let mut zeros: i64 = 0;
    while access::is_list(list) {
        let head = access::car(list);
        if !access::is_small(head) {
            break;
        }
        let c = (unsigned_val(head) & 0xFF) as u8;
        if c != b'0' {
            if c2int_is_invalid_char(c, base) {
                if zeros == 0 {
                    return atom(ctx, "no_integer");
                }
                // `{0, list}` — the *current* cell, which begins the invalid digit.
                return ctx.tuple(&[make_small(0), list]);
            }
            break;
        }
        zeros += 1;
        list = access::cdr(list);
    }

    if !access::is_list(list) {
        if zeros == 0 {
            return atom(ctx, "no_integer");
        }
        return ctx.tuple(&[make_small(0), list]);
    }

    // Accumulate phase: the counter restarts (`n = 0`, line 4325).
    let max_digits = digits_per_small(base);
    let mut n = 0usize;
    let mut ui: u64 = 0;
    while n <= max_digits {
        let head = access::car(list);
        if !access::is_small(head) {
            break;
        }
        let c = (unsigned_val(head) & 0xFF) as u8;
        if c2int_is_invalid_char(c, base) {
            break;
        }
        ui = ui
            .wrapping_mul(base as u64)
            .wrapping_add(u64::from(c2int_digit_from_base(c)));
        n += 1;
        list = access::cdr(list);
        if !access::is_list(list) {
            break;
        }
    }

    if n == 0 {
        return atom(ctx, "no_integer");
    }
    if n > max_digits {
        return atom(ctx, "big");
    }
    let value = if neg { -(ui as i64) } else { ui as i64 };
    let int = ctx.int(value)?;
    ctx.tuple(&[int, list])
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Registers the `erts_internal`/`erts_debug` BIFs (the roster in the module docs).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erts_debug", "same", 2, same_2);
    reg.register("erts_debug", "flat_size", 1, flat_size_1);
    reg.register("erts_internal", "term_type", 1, term_type_1);
    reg.register("erts_internal", "map_to_tuple_keys", 1, map_to_tuple_keys_1);
    reg.register("erts_internal", "map_hashmap_children", 1, map_hashmap_children_1);
    reg.register("erts_internal", "map_next", 3, map_next_3);
    reg.register("erts_internal", "time_unit", 0, time_unit_0);
    reg.register("erts_internal", "perf_counter_unit", 0, perf_counter_unit_0);
    reg.register("erts_internal", "binary_to_integer", 2, binary_to_integer_2);
    reg.register("erts_internal", "list_to_integer", 2, list_to_integer_2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true", "false", "ok", "badarg", "big", "none", "no_integer", "not_a_list", "iterator",
            "a", "b", "badmap", "x", "fixnum", "atom", "nil", "pid", "port", "list", "tuple",
            "hfloat", "bignum", "flatmap", "hashmap", "heap_binary", "sub_binary", "fun", "export",
            "reference", "external_reference", "external_pid", "external_port",
        ]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(8192).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    #[test]
    fn register_installs_every_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (module, name, arity) in [
            ("erts_debug", "same", 2),
            ("erts_debug", "flat_size", 1),
            ("erts_internal", "term_type", 1),
            ("erts_internal", "map_to_tuple_keys", 1),
            ("erts_internal", "map_hashmap_children", 1),
            ("erts_internal", "map_next", 3),
            ("erts_internal", "time_unit", 0),
            ("erts_internal", "perf_counter_unit", 0),
            ("erts_internal", "binary_to_integer", 2),
            ("erts_internal", "list_to_integer", 2),
        ] {
            assert!(
                reg.contains(module, name, arity),
                "missing {module}:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 10);
    }

    #[test]
    fn same_is_word_identity() {
        with_ctx(|ctx| {
            let one = make_small(1);
            let expected_true = ctx.boolean(true).unwrap();
            let expected_false = ctx.boolean(false).unwrap();
            let float_one = ctx.make_float(1.0).unwrap();
            assert_eq!(same_2(ctx, &[one, one]).unwrap(), expected_true);
            assert_eq!(same_2(ctx, &[one, float_one]).unwrap(), expected_false);
        });
    }

    #[test]
    fn term_type_names_every_immediate() {
        with_ctx(|ctx| {
            let cases: [(&str, Eterm); 5] = [
                ("fixnum", make_small(1)),
                ("atom", ctx.atom("a").unwrap()),
                ("nil", beam_term::NIL),
                ("pid", beam_term::prelude::make_internal_pid(beam_term::prelude::make_pid_data(0, 1))),
                (
                    "port",
                    beam_term::prelude::make_internal_port(beam_term::prelude::make_port_data(1)),
                ),
            ];
            for (name, term) in cases {
                let expected = ctx.intern_atom(name).unwrap();
                let got = term_type_1(ctx, &[term]).unwrap();
                assert_eq!(got, expected, "{name}");
            }
        });
    }

    #[test]
    fn map_to_tuple_keys_builds_the_key_tuple() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let map = access::make_flatmap(ctx.heap(), &[(a, make_small(1)), (b, make_small(2))])
                .unwrap();
            let keys = map_to_tuple_keys_1(ctx, &[map]).unwrap();
            assert!(access::is_tuple(keys));
            assert_eq!(access::tuple_size(keys), 2);
            assert_eq!(access::tuple_element(keys, 0), a);
            assert_eq!(access::tuple_element(keys, 1), b);

        });
    }

    #[test]
    fn map_next_flat_iterates_in_storage_order() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let map = access::make_flatmap(ctx.heap(), &[(a, make_small(1)), (b, make_small(2))])
                .unwrap();
            let iter = ctx.atom("iterator").unwrap();
            let step = map_next_3(ctx, &[make_small(0), map, iter]).unwrap();
            assert!(access::is_tuple(step));
            assert_eq!(access::tuple_element(step, 0), a);
            assert_eq!(access::tuple_element(step, 1), make_small(1));
        });
    }

    #[test]
    fn time_units_are_nanoseconds() {
        with_ctx(|ctx| {
            assert_eq!(time_unit_0(ctx, &[]).unwrap(), make_small(1_000_000_000));
            assert_eq!(
                perf_counter_unit_0(ctx, &[]).unwrap(),
                make_small(1_000_000_000)
            );
        });
    }

    #[test]
    fn binary_to_integer_parses_and_returns_failure_atoms() {
        with_ctx(|ctx| {
            let ten = make_small(10);
            let bin = access::make_heap_bitstring(ctx.heap(), b"123", 24).unwrap();
            assert_eq!(binary_to_integer_2(ctx, &[bin, ten]).unwrap(), make_small(123));
            let bad = access::make_heap_bitstring(ctx.heap(), b"1a", 16).unwrap();
            let badarg = ctx.intern_atom("badarg").unwrap();
            let got = binary_to_integer_2(ctx, &[bad, ten]).unwrap();
            assert_eq!(got, badarg);
        });
    }

    #[test]
    fn list_to_integer_stops_at_the_first_invalid_digit() {
        with_ctx(|ctx| {
            let ten = make_small(10);
            let x = ctx.atom("x").unwrap();
            let list = ctx.list(&[make_small(49), make_small(50), x]).unwrap();
            let expected_rest = ctx.list(&[x]).unwrap();
            let result = list_to_integer_2(ctx, &[list, ten]).unwrap();
            assert_eq!(access::tuple_element(result, 0), make_small(12));
            assert!(beam_term::compare::eq(
                access::tuple_element(result, 1),
                expected_rest,
                ctx.atoms()
            ));
        });
    }

    #[test]
    fn flat_size_counts_and_gaps() {
        with_ctx(|ctx| {
            assert_eq!(flat_size_1(ctx, &[make_small(1)]).unwrap(), make_small(0));
            let t = ctx.tuple(&[make_small(1), make_small(2), make_small(3)]).unwrap();
            assert_eq!(flat_size_1(ctx, &[t]).unwrap(), make_small(4));
            let empty = access::make_flatmap(ctx.heap(), &[]).unwrap();
            assert_eq!(flat_size_1(ctx, &[empty]).unwrap(), make_small(3));
            // A fun is a typed gap (its inlined descriptor diverges from C's).
            let ref_thing = access::make_ref_thing(ctx.heap(), 1).unwrap();
            assert!(matches!(flat_size_1(ctx, &[ref_thing]), Err(BeamError::Raise(_))));
        });
    }
}
