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

//! The `~w`/`~p`/`~W`/`~P` term printer used by `io:format`/`io_lib:format`.
//!
//! A safe helper module for [`crate::io`] (it registers no BIFs). It renders the
//! Milestone-1 term set the way `io_lib:write/…` and the `~p` pretty printer do,
//! and pins every case against the OTP oracle (erts-17.1): small integers, atoms
//! (bare vs quoted), `NIL`, floats, proper/improper lists, tuples, maps, binaries,
//! bignums, internal pids, internal ports and internal refs.
//!
//! This module owns the **leaf renderer** ([`term_chunk`]/[`render_chunk`], the
//! `io_lib:write1/4` tree) and the shared `io_lib_format` chunk helpers; the
//! `io_lib_pretty` `~p`/`~P` line re-flow lives in the private [`pretty`] submodule
//! and is re-exported here as [`pretty_chunk`], its `cind/7` probe in
//! `pretty::cind`.
//!
//! [`format_term`] is the depth-unlimited entry point `~w`/`~p` use;
//! [`format_term_depth`] is the depth-limited one `~W`/`~P` use. The depth
//! semantics reproduce `io_lib:write1/4` (`lib/stdlib/src/io_lib.erl`): depth `0`
//! renders `...`, depth `1` elides the *contents* of a container (`{...}`,
//! `[...]`, `<<...>>`, `#{}`), and every nesting step consumes one level; a
//! negative depth is unlimited (`~w` passes `-1`).
//!
//! [`crate::io`] renders every directive to a [`Chunk`] and materializes those for
//! `io_lib:format` (the deep list) or flattens them for `io:format` (the text).
//! [`format_term`] is a test-only text shortcut over the same renderer.
//!
//! # Nested chunks
//!
//! Rendering builds a tree of [`Chunk`]s mirroring the **exact nested-list
//! structure** the oracle's `io_lib`/`io_lib_pretty` produce (`write1/4`,
//! `write_tuple/6`, `write_tail/4`, `write_map/4`, `write_binary/2`): a container
//! is `[open, [e1, sep, e2, …], close]`, each element its own sub-list, each
//! separator an individual integer. [`format_term`]/[`format_term_depth`] flatten
//! the tree to text; `crate::io` materializes it as the deep list `io_lib:format`
//! returns.
//!
//! # The `~p` printable-string rule
//!
//! With `printable = true` (the `~p`/`~P` directives) a **non-empty proper list
//! whose every element is a printable latin1 character** (`16#20..=16#7E`,
//! `16#A0..=16#FF`, or one of `\t \n \r \v \b \f \e`) renders as a `"…"` string,
//! escaping `"`, `\` and those control characters (and leaving `16#A0..=16#FF`
//! verbatim); `~w` (`printable = false`) never applies the rule. So `~p` of
//! `"abc"` is `"abc"` while `~w` of `"abc"` is `[97,98,99]`, and `~p` of `[]` is
//! `[]` (the rule requires a *non-empty* list). The same rule is applied to
//! **binaries**, which then render as `<<"…">>`.
//!
//! # Maps
//!
//! A map renders `#{k => v,…}` (note ` => ` between a key and its value and a
//! bare `,` between pairs — the oracle's rendering). By default the pairs are
//! printed in the map's **storage order**
//! ([`beam_term::map::map_pairs_storage_order`]) — the order `maps:keys/1`
//! yields: a flat map (≤32 pairs) stores its keys in the oracle's *flatmap key
//! order* (`erts_cmp_flatmap_keys`, `erl_utils.h:267`: two atom keys by their raw
//! term word, i.e. atom index / intern order; anything else by the exact term
//! order), and a hash map (>32 pairs) stores them in its HAMT slot traversal
//! order. Both match the oracle's `~w`/`~p` byte for byte.
//!
//! The `~k`/`~K` **map-order modifiers** select a different order for the
//! directive that follows them (`io_lib_format:modifiers/3`): `~k` sets the
//! order to `ordered` (ascending term order), and `~K` takes the order from the
//! next argument — the atoms `ordered`/`reversed`/`undefined` (the default
//! storage order) select a built-in order, and a 2-arity fun is the order
//! *comparator* the oracle would call (a typed [`unsupported_bif`] here, as no
//! `FunCaller` seam is available). The order applies to **every** map at any
//! depth of the directive's term; [`MapOrder`] carries it and
//! [`resolve_map_pairs`] interprets it exactly when a map is rendered.
//!
//! # `~p`/`~P` re-flow (`io_lib_pretty`)
//!
//! The oracle runs `io_lib_pretty`, which breaks a container across lines when its
//! single-line rendering is at least `line_length - column` characters (80 columns
//! and column 1 by default). [`pretty_chunk`] reproduces that break for a
//! proper/improper list, a tagged/untagged tuple and a map each of whose elements
//! renders single-line (the break separator `,`, the improper tail after `|`, a
//! `tag_len + 2` indent after a tuple tag, a two-space indent after `#{`), and for a
//! **non-printable byte-aligned binary**, whose `integer_to_list/1` byte elements
//! `pp_binary/7` packs `N = max(8, min(Ll - Col, M - 4))` per line. For the forms it
//! does not model — a nested container, an element too long to fit on a line of its
//! own, an all-printable binary (the oracle's string leaf), a non-byte-aligned
//! bitstring, a long tuple tag, a depth-limited `~P` — it raises `error:badarg`
//! rather than emit a line the oracle would have broken differently. See the
//! `crate::io` module docs for the precise rule, the term classes and the narrowest
//! repro.
//!
//! # Floats
//!
//! A float renders as `float_to_list(F, [short])` — the oracle's plain float
//! leaf (`io_lib:write1/4` calls `io_lib_format:fwrite_g/1`, which is
//! `float_to_list(F, [short])`), reproduced by the same
//! [`crate::numeric::format_short`] port the `float_to_list/2` BIF uses, so
//! `1.0e5` ⇒ `"1.0e5"`, `0.1` ⇒ `"0.1"`, `1.0` ⇒ `"1.0"`. The domain is finite
//! floats — OTP represents no `inf`/`NaN` (see [`format_float`]).
//!
//! # Atoms
//!
//! An atom is rendered by the oracle's `io_lib:write_atom_as_latin1/1` (the
//! latin1 clause of `write1/4`, which `~w`, `~W`, `~p` and `~P` all take): bare
//! when `io_lib:quote_atom/2` is false — the name is not an
//! `erl_scan:reserved_word/1`, its first character is a lowercase letter
//! (`a..z`, `ß..ÿ` except `÷`) and every character matches `name_char/1` (`a..z`,
//! `ß..ÿ` except `÷`, `A..Z`, `À..Þ` except `×`, `0..9`, `_`, `@`) — and
//! `'…'`-quoted with `io_lib:string_char(unicode_as_latin1, …)` escaping
//! otherwise. So `héllo` is **bare** (`io_lib:format("~w",['héllo'])` ⇒
//! `[104,233,108,108,111]`), `and`/`Hello`/`_foo`/`a b`/`a÷b` are quoted, a
//! control character is an octal/`\n`/`\t`/… escape, `16#7F` is `\d`, and a
//! character above `16#FF` is `\x{H}`. See [`format_atom`].
//!
//! # Deviations from the oracle (documented)
//!
//! * **Non-byte-aligned bitstrings** render as the oracle's `<<X:L>>` (whole leading
//!   bytes, then the trailing `L` bits as `Value:L`), e.g. `<<1:1>>`, `<<23:5>>`,
//!   `<<0,1:1>>`. The `~p` printable-string rule never applies to them.
//! * **Internal refs** render as `#Ref<Channel.Node.Serial.Id>` with
//!   `Channel`/`Node`/`Serial` always `0` in Milestone-1 (the frozen
//!   `[header, node, serial, id]` thing of [`crate::access::make_ref_thing`]), so
//!   only `Id` varies. The oracle's numbers are the same shape.
//! * **Internal ports** render as `#Port<0.N>` (see [`format_port`]), the oracle's
//!   `#Port<Channel.Num>` with the local node's channel `0`.
//! * **External identifiers** (a pid, port or ref naming another node) and a
//!   **native record** are outside the Milestone-1 printable set. The oracle renders
//!   both, but for an external identifier the rendered channel number is
//!   `atom_val(x->sysname)` — the node-name atom's index in the **running VM's** atom
//!   table, runtime state the frozen external-identifier term does not carry (measured:
//!   the same wire image printed `#Port<10643.7>` and `#Port<10639.7>` in two runs), so
//!   no byte-faithful text exists; each is rejected with the typed
//!   `BeamError::UnsupportedBif` (a capability gap) rather than guessed. A native
//!   record's *single-line* form (`#rec:r{a = 1,b = 2}`) is derivable from
//!   [`beam_term::record`], but the printer cannot emit it as a leaf: `~p`/`~P` route a
//!   record through the same [`render_chunk`] and would then print the flat form where
//!   the oracle's `io_lib_pretty` **re-flows** a long record (`pp_record`/`rec_indent` in
//!   `lib/stdlib/src/io_lib_pretty.erl:249-376`), a *silently wrong* result — so the
//!   whole class stays a typed gap until that template is ported.
//!
//! # Funs
//!
//! A **fun**'s leaf is `erlang:fun_to_list/1` (`lib/stdlib/src/io_lib.erl:914` for
//! `~w`/`~p`, and `io_lib_pretty:print_length/7` at `:932-934` splices the same
//! text), so an `EXPORT_EXT` fun renders `fun Module:Function/Arity` and a
//! `NEW_FUN_EXT` fun renders `#Fun<Module.OldIndex.OldUniq>` — both byte-faithfully,
//! via [`crate::fun_info::fun_text_str`], at any nesting depth. Only a runtime-made
//! `Local` (`make_fun3`) fun stays the typed capability gap
//! ([`BeamError::UnsupportedBif`](beam_common::BeamError), matching
//! `erlang:fun_to_list/1`): its inlined descriptor is only `(uniq, index)`, so the
//! module/name a `#Fun<…>` needs is not in the term and any rendering would be a
//! *silently wrong* result.

use beam_codec::term::big_parts;
use beam_common::Result;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

use crate::access::{
    bitstring_bit_size, bitstring_bits, car, cdr, float_value, fun_arity, is_atom, is_big,
    is_external_pid, is_external_ref, is_float, is_function, is_internal_pid, is_internal_port,
    is_internal_ref, is_list, is_map, is_nil, is_port, is_small, is_tuple, tuple_element,
    tuple_size,
};
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};

mod pretty;
pub(crate) use pretty::{
    pretty_chunk, pretty_chunk_limited, pretty_chunk_order, pretty_write_limited, tsub,
};

/// The depth value that disables depth limiting — `io_lib` passes `-1` for `~w`.
pub(crate) const UNLIMITED_DEPTH: i64 = -1;

/// The map printing order a `~k`/`~K` modifier selected
/// (`io_lib_format:modifiers/3` → `Spec#{maps_order => …}`).
///
/// Threaded through the whole rendering of one directive's term, so it applies to
/// every map at any depth. The order is interpreted **only when a map is
/// rendered** ([`resolve_map_pairs`]), exactly as the oracle's `maps:iterator/2`
/// is only reached by `write_map`/`print_length_map`.
#[derive(Clone, Copy)]
pub(crate) enum MapOrder {
    /// The default: the map's stored pair order (the `undefined` order).
    Storage,
    /// `ordered` (the `~k` modifier): ascending term order.
    Ordered,
    /// `reversed`: descending term order.
    Reversed,
    /// A raw `~K` order term, interpreted at map-render time.
    Explicit(Eterm),
}

/// Resolves the pairs of a map `term` in the order a `~k`/`~K` modifier selected
/// — the oracle's `maps:iterator(Map, MapsOrder)` + `maps:next/1` walk.
///
/// * [`MapOrder::Storage`] — the map's stored pair order (`maps:to_list/1`'s
///   order, the `undefined` order);
/// * [`MapOrder::Ordered`] — ascending **term** order (`map_pairs`);
/// * [`MapOrder::Reversed`] — descending term order (`map_pairs` reversed);
/// * [`MapOrder::Explicit`] — a `~K` order term: the atoms `ordered`/`reversed`/
///   `undefined` select as above (`undefined` is the default iterator, which walks
///   the stored order), a **2-arity fun** is the typed [`unsupported_bif`] (the
///   printer has no `FunCaller` seam to invoke it, and a fabricated order would be a
///   wrong result), and anything else (any other atom, a non-atom, or a fun of another
///   arity) is `badarg`, as the oracle's `maps:iterator/2` argument check.
///
/// # Errors
///
/// As [`beam_term::map`] (`badarg` for a non-map), plus the `badarg`/
/// `unsupported_bif` cases above.
pub(crate) fn resolve_map_pairs(
    term: Eterm,
    order: MapOrder,
    atoms: &AtomTable,
) -> Result<Vec<(Eterm, Eterm)>> {
    match order {
        MapOrder::Storage => Ok(beam_term::map::map_pairs_storage_order(term)?),
        MapOrder::Ordered => Ok(beam_term::map::map_pairs(term, atoms)?),
        MapOrder::Reversed => {
            let mut pairs = beam_term::map::map_pairs(term, atoms)?;
            pairs.reverse();
            Ok(pairs)
        }
        MapOrder::Explicit(order_term) => {
            // A 2-arity fun is the order *comparator* the oracle would call
            // (`maps:iterator(Map, CmpFun)`) — the printer has no `FunCaller` seam,
            // so it is an honest typed gap rather than a fabricated order.
            if is_function(order_term) {
                return match fun_arity(order_term) {
                    Some(2) => Err(unsupported_bif()),
                    _ => Err(badarg()),
                };
            }
            if !is_atom(order_term) {
                return Err(badarg());
            }
            let resolved = match atoms.name(atom_val(order_term)).ok_or_else(badarg)? {
                "ordered" => MapOrder::Ordered,
                "reversed" => MapOrder::Reversed,
                // `maps:iterator(Map, undefined)` is `maps:iterator(Map, fun(A,B) ->
                // A =< B end)` — the default iterator, whose `maps:next/1` walk visits
                // the map's *stored* pair order, exactly `MapOrder::Storage`. Verified
                // against the oracle: `~Kp undefined` / `~Kw undefined` are byte-equal
                // to the default `~p`/`~w` text for both a flat (≤32-key) and a HAMT
                // (>32-key) map. Any other atom is `badarg`, as `maps:iterator/2`
                // raises for an unrecognised order term (`~Kp bogus`, `~Kp {x}` and
                // `~Kp 5` are all `error:badarg`).
                "undefined" => MapOrder::Storage,
                _ => return Err(badarg()),
            };
            resolve_map_pairs(term, resolved, atoms)
        }
    }
}
/// A fragment of a rendering: the exact nested character-list structure the
/// oracle's `io_lib`/`io_lib_pretty` build.
///
/// An `io_lib` "chars" value is a list whose elements are either single
/// characters (small integers), *raw* integers, or further lists: `Char` is one
/// such element, `Int` a raw-integer element and `List` a sub-list element.
/// `io:format` flattens the tree to text, while `io_lib:format` materializes it
/// as the corresponding nested term (see `crate::io`).
#[derive(Clone)]
pub(crate) enum Chunk {
    /// One character element — a single small integer in the output list.
    Char(char),
    /// An integer element that is not a Rust `char`: the oracle's `io_lib` keeps
    /// the integer **verbatim** while its unicode device refuses it. Only the
    /// `~tc` directive and the radix **prefix** build one — see [`crate::io`] and
    /// [`flatten_output`].
    Int(Eterm),
    /// A sub-list element.
    List(Vec<Chunk>),
    /// A **binary** element: the one element kind a deep `io_lib:format` result can
    /// carry that is not a character list. Only the `~ts`-under-`{chars_limit, N}`
    /// arm builds one (`io_lib_format:cdata_to_chars/3` hands a
    /// `string:next_grapheme/1` error payload's binary through verbatim), so
    /// `io_lib:format("~10.4ts", [[[<<200,201>>],97]], [{chars_limit,4}])` is
    /// `[[[32],<<200,201>>,97]]` — a real binary term inside the list.
    ///
    /// Its [`Chunk::flat_len`] is `io_lib:chars_length(Bin)` = `byte_size(Bin)`,
    /// and it is the only variant that materializes as a **binary** rather than as
    /// a list of small integers (see `crate::io::chunk_to_term`).
    Bin(Vec<u8>),
}

impl Chunk {
    /// A flat character list (`"abc"` ⇒ the one element `[a, b, c]`).
    pub(crate) fn text(text: &str) -> Chunk {
        Chunk::List(text.chars().map(Chunk::Char).collect())
    }

    /// Appends the flattened text to `out`.
    pub(crate) fn flatten_into(&self, out: &mut String) {
        match self {
            Chunk::Char(c) => out.push(*c),
            // A raw integer element. Every production path that can build one goes
            // through [`flatten_output`] (which rejects a non-scalar code point
            // first, as the oracle's unicode device does); `flatten` is reached
            // from a *term* rendering, which never builds an `Int`. The
            // "unrepresentable" marker keeps this total without a panic.
            Chunk::Int(term) => out.push(raw_scalar(*term).unwrap_or(char::REPLACEMENT_CHARACTER)),
            Chunk::List(items) => {
                for item in items {
                    item.flatten_into(out);
                }
            }
            // A binary element of a deep list: `io_lib` splices its bytes into the
            // surrounding character list (`iolist_to_binary/1` leaves them as they
            // are), so they are appended one latin1 character each — the same
            // mapping the latin1 `~s` arm uses for a binary's bytes.
            Chunk::Bin(bytes) => out.extend(bytes.iter().map(|&b| b as char)),
        }
    }

    /// The flat character count (`io_lib:chars_length/1`).
    pub(crate) fn flat_len(&self) -> usize {
        match self {
            // `io_lib:chars_length/1` counts a raw integer element as one character.
            Chunk::Char(_) | Chunk::Int(_) => 1,
            Chunk::List(items) => items.iter().map(Chunk::flat_len).sum(),
            // `io_lib:chars_length(Bin)` is `iolist_size(Bin)` = `byte_size(Bin)`.
            Chunk::Bin(bytes) => bytes.len(),
        }
    }

    /// The element list of a `Chunk::List`.
    pub(crate) fn into_items(self) -> Vec<Chunk> {
        match self {
            Chunk::List(items) => items,
            char => vec![char],
        }
    }
}

/// Flattens a chunk sequence to its text.
pub(crate) fn flatten(chunks: &[Chunk]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        chunk.flatten_into(&mut out);
    }
    out
}

/// The `char` a raw integer element denotes, `None` when it is not a Unicode
/// scalar value (negative, past `16#10FFFF`, or a surrogate).
///
/// A bignum is never a scalar — its magnitude is at least `2^59`.
fn raw_scalar(term: Eterm) -> Option<char> {
    if !is_small(term) {
        return None;
    }
    u32::try_from(signed_value(term))
        .ok()
        .and_then(char::from_u32)
}

/// Flattens a chunk sequence to the text an `io:format` device receives,
/// rejecting a raw integer element that is not a Unicode scalar value with
/// `badarg`.
///
/// `io:format` writes the iolist to a device, and the oracle's device is a
/// **unicode** one: it encodes every character as UTF-8 and refuses a code point
/// outside the scalar range. So `io_lib:format("~tc",[1114112])` ⇒ `[1114112]`
/// (the integer is carried verbatim) while `io:format("~tc",[1114112])` ⇒
/// `error:badarg` — and likewise for a surrogate (`55296`), a negative value and
/// a bignum. The `&str`-typed [`crate::context::OutputSink`] cannot carry such a
/// code point either, so the error is raised here rather than fabricating text.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-scalar
/// [`Chunk::Int`].
pub(crate) fn flatten_output(chunks: &[Chunk]) -> Result<String> {
    let mut out = String::new();
    for chunk in chunks {
        flatten_output_into(chunk, &mut out)?;
    }
    Ok(out)
}

/// [`flatten_output`] for one chunk.
fn flatten_output_into(chunk: &Chunk, out: &mut String) -> Result<()> {
    match chunk {
        Chunk::Char(c) => {
            out.push(*c);
            Ok(())
        }
        Chunk::Int(term) => match raw_scalar(*term) {
            Some(c) => {
                out.push(c);
                Ok(())
            }
            None => Err(badarg()),
        },
        Chunk::List(items) => {
            for item in items {
                flatten_output_into(item, out)?;
            }
            Ok(())
        }
        // Unreachable from `io:format` (the `~ts`-under-`{chars_limit, N}` arm is
        // only reachable through `io_lib:format/3`, whose result is materialized as
        // a term, not flattened to text). Handled as [`Chunk::flatten_into`] does,
        // byte by byte, so the walk stays total without inventing a raise.
        Chunk::Bin(bytes) => {
            out.extend(bytes.iter().map(|&b| b as char));
            Ok(())
        }
    }
}

/// Renders `term` the way the `~w` (`printable = false`) or `~p`
/// (`printable = true`) directive does, with no depth limit — the **text** form.
///
/// Test-only: `crate::io` renders its directives to [`Chunk`]s and flattens those
/// itself, so this text shortcut is used by the assertions of this module and of
/// `crate::lists` alone.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
/// Milestone-1 printable set (see the module docs), or an atom whose index is not
/// interned in `ctx`'s atom table.
#[cfg(test)]
pub(crate) fn format_term(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
) -> Result<String> {
    format_term_depth(ctx, term, printable, UNLIMITED_DEPTH)
}

/// Renders `term` with an `io_lib:write/2` **depth** limit (`~W`/`~P`); see
/// [`format_term`].
///
/// A negative `depth` is unlimited (see [`UNLIMITED_DEPTH`]); `0` renders `...`
/// and `1` elides a container's contents.
///
/// # Errors
///
/// As [`format_term`].
#[cfg(test)]
pub(crate) fn format_term_depth(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<String> {
    Ok(flatten(&[term_chunk(ctx, term, printable, depth)?]))
}

/// Renders `term` as one element chunk (`io_lib:write1/4`, or
/// `io_lib_pretty:write/1` when `printable`, the `~p` string rule).
///
/// # Errors
///
/// As [`format_term`].
pub(crate) fn term_chunk(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<Chunk> {
    render_chunk(ctx.atoms(), term, printable, depth, MapOrder::Storage)
}

/// As [`term_chunk`], but with the map order a `~k`/`~K` modifier selected
/// ([`MapOrder`]) applied to every map at any depth.
///
/// # Errors
///
/// As [`term_chunk`].
pub(crate) fn term_chunk_order(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    order: MapOrder,
) -> Result<Chunk> {
    render_chunk(ctx.atoms(), term, printable, depth, order)
}/// Collects `term` into its element vector iff it is a proper list.
///
/// `NIL` yields `Some(vec![])`; a cons chain ending in anything but `NIL`, or a
/// non-list term, yields `None`. Shared with [`crate::io`] and [`crate::ets`] so
/// the "is this a proper list?" rule lives in exactly one place.
pub(crate) fn proper_list(term: Eterm) -> Option<Vec<Eterm>> {
    let mut elems = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        elems.push(car(cur));
        cur = cdr(cur);
    }
    if is_nil(cur) {
        Some(elems)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The `~p` printable-character rule, shared with the `pretty` sub-module
// ---------------------------------------------------------------------------

/// `io_lib:printable_character(C, latin1)`: `16#20..=16#7E`, `16#A0..=16#FF`, or
/// one of `\t \n \r \v \b \f \e` (`io_lib.erl:1514-1523`).
fn latin1_printable_char(c: u8) -> bool {
    (0x20..=0x7E).contains(&c)
        || (0xA0..=0xFF).contains(&c)
        || matches!(c, 0x09 | 0x0A | 0x0D | 0x0B | 0x08 | 0x0C | 0x1B)
}

/// [`latin1_printable_char`] for a small integer's value: the value must be a byte
/// (`0..=255`) that is a printable latin1 character. A value outside the byte
/// range is never printable.
fn char_printable(value: i64) -> bool {
    (0..=255).contains(&value) && latin1_printable_char(value as u8)
}

/// Renders `term` as one element chunk (`io_lib:write1/4` for `~w`, or
/// `io_lib_pretty:write/1` for `~p`).
fn render_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    order: MapOrder,
) -> Result<Chunk> {
    if depth == 0 {
        return Ok(Chunk::text("..."));
    }
    if is_small(term) {
        return Ok(Chunk::text(&signed_value(term).to_string()));
    }
    if is_big(term) {
        return Ok(Chunk::text(&format_big(term)?));
    }
    if is_nil(term) {
        return Ok(Chunk::text("[]"));
    }
    if is_list(term) {
        if depth == 1 {
            return Ok(Chunk::text("[...]"));
        }
        if printable {
            if let Some(bytes) = printable_string(term) {
                return Ok(Chunk::text(&quoted_string(&bytes)));
            }
        }
        return render_list_chunk(atoms, term, printable, depth, order);
    }
    if is_tuple(term) {
        let arity = tuple_size(term);
        if arity == 0 {
            return Ok(Chunk::text("{}"));
        }
        if depth == 1 {
            return Ok(Chunk::text("{...}"));
        }
        let mut body: Vec<Chunk> = Vec::with_capacity(arity);
        body.push(render_chunk(
            atoms,
            tuple_element(term, 0),
            printable,
            depth - 1,
            order,
        )?);
        render_tuple_rest_chunk(atoms, term, 1, depth - 1, printable, order, &mut body)?;
        return Ok(Chunk::List(vec![
            Chunk::Char('{'),
            Chunk::List(body),
            Chunk::Char('}'),
        ]));
    }
    if is_atom(term) {
        let name = atoms.name(atom_val(term)).ok_or_else(badarg)?;
        return Ok(Chunk::text(&format_atom(name)));
    }
    if is_float(term) {
        return Ok(Chunk::text(&format_float(float_value(term))?));
    }
    if bits::is_binary(term) {
        return render_binary_chunk(term, printable, depth);
    }
    if bits::is_bitstring(term) {
        return render_bitstring_chunk(term, printable, depth);
    }
    if is_map(term) {
        return render_map_chunk(atoms, term, printable, depth, order);
    }
    if is_internal_pid(term) {
        // The `%T` text `erlang:pid_to_list/1` renders (the oracle's `write1/4`
        // calls exactly that), from its one home in `crate::term_conv`.
        return Ok(Chunk::text(&crate::term_conv::pid_text_str(term)?));
    }
    if is_internal_ref(term) {
        return Ok(Chunk::text(&format_ref(term)?));
    }
    if is_port(term) {
        return Ok(Chunk::text(&format_port(term)?));
    }
    // An **external** pid/ref is a typed gap, exactly like an external port (see
    // [`format_port`]): the oracle renders it through `pid_channel_no` /
    // `ref_channel_no`, which for a foreign node is `external_channel_no(x)` =
    // `dist_entry_channel_no(external_dist_entry(x))` = `atom_val(x->sysname)` — the
    // index of the node-name atom in the **running VM's** atom table
    // (`erl_node_container_utils.h:109-116`). That index is runtime state the frozen
    // external-identifier term does not carry (measured: the same `V4_PORT_EXT` image
    // naming `a@b` printed `#Port<10643.7>` and `#Port<10639.7>` in two runs), so a
    // fabricated channel would be a *silently wrong* result.
    if is_external_pid(term) || is_external_ref(term) {
        return Err(unsupported_bif());
    }
    // A **fun**'s leaf is `erlang:fun_to_list/1` (`io_lib:write1/4` → `fun_to_list`,
    // and `io_lib_pretty:print_length/7` splices the same `io_lib:write/1` text —
    // `lib/stdlib/src/io_lib_pretty.erl:932-934`), i.e. the text
    // `crate::fun_info::fun_text_str` renders: `fun Module:Function/Arity` for an
    // `EXPORT_EXT` fun, `#Fun<Module.OldIndex.OldUniq>` for a `NEW_FUN_EXT` fun.
    // Only a runtime-made `Local` (`make_fun3`) fun stays a typed capability gap —
    // its inlined descriptor carries no module/name, so a `#Fun<…>` here would be a
    // *silently wrong* result (see the module docs).
    if is_function(term) {
        return Ok(Chunk::text(&crate::fun_info::fun_text_str(term, atoms)?));
    }
    Err(badarg())
}

/// The `%T` text of an internal port — `#Port<channel.number>`
/// (`../../erts/emulator/beam/erl_printf_term.c:521-530`, the leaf
/// `erlang:port_to_list/1` renders and `io_lib:write1/4` / `io_lib_pretty`'s
/// `print_length/7` both splice in), from its one home in `crate::term_conv`'s
/// [`port_text_str`](crate::term_conv::port_text_str). An **internal** port renders
/// `#Port<0.N>` — the local node's channel number and the port-table number —
/// byte-identical to the oracle, where `io_lib:format("~p", [hd(erlang:ports())])`
/// is `"#Port<0.0>"`.
///
/// An **external** port is the typed [`unsupported_bif`]; see the external
/// pid/ref comment in [`render_chunk`] for why no faithful channel exists.
///
/// # Errors
///
/// [`BeamError::UnsupportedBif`](beam_common::BeamError) for an external port (see
/// above); a term that is not a port at all never reaches this function.
fn format_port(term: Eterm) -> Result<String> {
    if is_internal_port(term) {
        return crate::term_conv::port_text_str(term);
    }
    Err(unsupported_bif())
}/// Renders a list (`[e1,e2]` / `[e1|tail]`), the caller having already handled
/// depth `0`/`1` and the `~p` printable-string rule (`io_lib:write1/4`).
fn render_list_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    order: MapOrder,
) -> Result<Chunk> {
    let mut body: Vec<Chunk> = Vec::new();
    body.push(render_chunk(atoms, car(term), printable, depth - 1, order)?);
    render_tail_chunk(atoms, cdr(term), printable, depth - 1, order, &mut body)?;
    Ok(Chunk::List(vec![
        Chunk::Char('['),
        Chunk::List(body),
        Chunk::Char(']'),
    ]))
}

/// Appends a list tail (`""`, `,e…` or `|tail`, `io_lib:write_tail/4`); the
/// closing `]` is the caller's.
///
/// The tail is walked with a **loop**, never by self-recursion: `write_tail/4`
/// consumes one depth level per element, so a self-recursive walk grew the native
/// stack once per list element and a long flat list (tens of thousands of elements)
/// overflowed it. The pushed chunks — the `,` before each element, the terminating
/// `|`+tail, or the depth-limited `|...` — are exactly the recursive walk's.
fn render_tail_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    order: MapOrder,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    let mut term = term;
    let mut depth = depth;
    loop {
        if is_nil(term) {
            return Ok(());
        }
        if depth == 1 {
            // `[$| | "..."]` — the four characters are spliced individually.
            out.extend(['|', '.', '.', '.'].map(Chunk::Char));
            return Ok(());
        }
        if !is_list(term) {
            out.push(Chunk::Char('|'));
            out.push(render_chunk(atoms, term, printable, depth - 1, order)?);
            return Ok(());
        }
        out.push(Chunk::Char(','));
        out.push(render_chunk(atoms, car(term), printable, depth - 1, order)?);
        term = cdr(term);
        depth -= 1;
    }
}

/// Appends the remaining tuple elements (`,e…` / `,...`), `io_lib:write_tuple/6`.
///
/// Iterative for the same reason as [`render_tail_chunk`]: one depth level is
/// consumed per element, so a per-element recursion overflowed the native stack on a
/// long tuple.
fn render_tuple_rest_chunk(
    atoms: &AtomTable,
    term: Eterm,
    index: usize,
    depth: i64,
    printable: bool,
    order: MapOrder,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    let len = tuple_size(term);
    let mut index = index;
    let mut depth = depth;
    while index < len {
        if depth == 1 {
            out.extend([',', '.', '.', '.'].map(Chunk::Char));
            return Ok(());
        }
        out.push(Chunk::Char(','));
        out.push(render_chunk(
            atoms,
            tuple_element(term, index),
            printable,
            depth - 1,
            order,
        )?);
        index += 1;
        depth -= 1;
    }
    Ok(())
}
/// Renders a map (`#{k => v,…}`), `io_lib:write_map/4` /
/// `io_lib_pretty:write/1` (both build `[#, {, [pair, …, pair], }]`).
///
/// The pairs come from [`resolve_map_pairs`]: the map's storage order by default
/// (flat-map key order for a flat map, HAMT slot order for a hash map) — the order
/// `maps:keys/1` yields and the oracle prints — or the `ordered`/`reversed`/`~K`
/// order `order` selected. So no re-sort is needed and a >32-pair hash map renders
/// like the oracle.
fn render_map_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    order: MapOrder,
) -> Result<Chunk> {
    if depth == 1 {
        return Ok(Chunk::text("#{}"));
    }
    let pairs: Vec<(Eterm, Eterm)> = resolve_map_pairs(term, order, atoms)?;
    if pairs.is_empty() {
        return Ok(Chunk::text("#{}"));
    }
    let inner = depth - 1;
    let mut body: Vec<Chunk> = Vec::new();
    body.push(map_pair_chunk(atoms, pairs[0], printable, inner, order)?);
    render_map_body_chunk(atoms, &pairs, 1, inner, inner, printable, order, &mut body)?;
    Ok(Chunk::List(vec![
        Chunk::Char('#'),
        Chunk::Char('{'),
        Chunk::List(body),
        Chunk::Char('}'),
    ]))
}

/// Appends the remaining map pairs (`,k => v…` / `,...`),
/// `io_lib:write_map_body/5`.
///
/// `depth` starts at the first pair's inner depth and decreases per pair; `inner`
/// is the (fixed) depth every key and value is rendered at.
///
/// Iterative for the same reason as [`render_tail_chunk`]: the walk consumes one
/// depth level per pair, so a per-pair recursion would grow the native stack once per
/// pair. Note the `depth == 1` check comes **before** the index bound, exactly as in
/// the recursive form (the depth-limited run emits its `,...` even when the pairs are
/// exhausted).
fn render_map_body_chunk(
    atoms: &AtomTable,
    pairs: &[(Eterm, Eterm)],
    index: usize,
    depth: i64,
    inner: i64,
    printable: bool,
    order: MapOrder,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    let mut index = index;
    let mut depth = depth;
    loop {
        if depth == 1 {
            out.extend([',', '.', '.', '.'].map(Chunk::Char));
            return Ok(());
        }
        if index >= pairs.len() {
            return Ok(());
        }
        out.push(Chunk::Char(','));
        out.push(map_pair_chunk(atoms, pairs[index], printable, inner, order)?);
        index += 1;
        depth -= 1;
    }
}

/// Renders one map pair (`io_lib:write_map_assoc/4`): `[Key, " => ", Value]` —
/// the four ` => ` characters are one sub-list element.
fn map_pair_chunk(
    atoms: &AtomTable,
    pair: (Eterm, Eterm),
    printable: bool,
    depth: i64,
    order: MapOrder,
) -> Result<Chunk> {
    Ok(Chunk::List(vec![
        render_chunk(atoms, pair.0, printable, depth, order)?,
        Chunk::text(" => "),
        render_chunk(atoms, pair.1, printable, depth, order)?,
    ]))
}
/// `io_lib_format:chars/2` — a deeply nested `lists:duplicate/2`.
///
/// `chars(C, N)` is `[]`/`[C]`/`[C,C]`/`[C,C,C]` for `N` in `0..=3`, else `[S|S]`
/// (`S` = `chars(C, N/2)`) for even `N` and `[C,S|S]` for odd `N`, so a long run
/// is a *tree* of sub-lists, not a flat list.
pub(crate) fn dup_chars(c: char, n: usize) -> Chunk {
    dup_element(Chunk::Char(c), n)
}

/// [`dup_chars`] for an arbitrary single element — a [`Chunk::Char`] or a raw
/// [`Chunk::Int`] (`io_lib_format:chars/2` duplicates whatever `char/5` holds,
/// and `~tc` may hold an integer that is not a Unicode scalar).
pub(crate) fn dup_element(elem: Chunk, n: usize) -> Chunk {
    match n {
        0 => Chunk::List(Vec::new()),
        1 => Chunk::List(vec![elem]),
        2 => Chunk::List(vec![elem.clone(), elem]),
        3 => Chunk::List(vec![elem.clone(), elem.clone(), elem]),
        n if n % 2 == 0 => {
            let half = dup_element(elem, n / 2);
            let mut items = vec![half.clone()];
            items.extend(half.into_items());
            Chunk::List(items)
        }
        n => {
            let half = dup_element(elem.clone(), n / 2);
            let mut items = vec![elem, half.clone()];
            items.extend(half.into_items());
            Chunk::List(items)
        }
    }
}

/// `io_lib_format:adjust/3` — `[Data|Pad]` (left) / `[Pad|Data]` (right), or the
/// data unchanged when the pad is the empty list.
pub(crate) fn adjust_chunk(data: Chunk, pad: Chunk, left: bool) -> Chunk {
    let pad_items = pad.into_items();
    if pad_items.is_empty() {
        return data;
    }
    let data_items = data.into_items();
    if left {
        let mut items = vec![Chunk::List(data_items)];
        items.extend(pad_items);
        Chunk::List(items)
    } else {
        let mut items = vec![Chunk::List(pad_items)];
        items.extend(data_items);
        Chunk::List(items)
    }
}

/// `io_lib_format:term/6` — the field-width / truncation rule as a chunk.
pub(crate) fn field_chunk(
    body: Chunk,
    width: Option<usize>,
    left: bool,
    precision: Option<usize>,
    pad: u8,
) -> Chunk {
    let (field, precision) = match (width, precision) {
        (None, None) => return body,
        (None, Some(p)) => (p, p),
        (Some(f), None) => (f, f),
        (Some(f), Some(p)) => (f, p),
    };
    let length = body.flat_len();
    let effective = length.min(precision.min(field));
    if length > effective {
        adjust_chunk(
            dup_chars('*', effective),
            dup_chars(pad as char, field - effective),
            left,
        )
    } else {
        adjust_chunk(body, dup_chars(pad as char, field - length), left)
    }
}

/// Renders a binary (`io_lib:write_binary/2` for `~w`,
/// `io_lib_pretty:write/1` for `~p`).
///
/// The `~w` form is `[<, <, [digits, 44, digits, …], >, >]` (each byte its own
/// sub-list, the separators individual integers; `[<, <, [], >, >]` for an empty
/// binary). The body is depth-limited the way `io_lib:write_binary/3`'s
/// `write_binary_body/4` limits it — one level per byte, then `"..."` while bytes
/// remain — so `~W` of `<<1,2,3>>` is `<<>>`, `<<...>>`, `<<1,...>>`, `<<1,2,...>>`,
/// `<<1,2,3>>` for depths `0..=4`. The `~p` form keeps the string leaf flat:
/// `<<>>` / `<<...>>` / `<<"…">>`.
fn render_binary_chunk(term: Eterm, printable: bool, depth: i64) -> Result<Chunk> {
    let bytes = bits::bytes(term)?;
    if printable {
        if bytes.is_empty() {
            return Ok(Chunk::text("<<>>"));
        }
        if depth == 1 {
            return Ok(Chunk::text("<<...>>"));
        }
        if printable_bytes(&bytes) {
            return Ok(Chunk::List(vec![
                Chunk::Char('<'),
                Chunk::Char('<'),
                Chunk::text(&quoted_string(&bytes)),
                Chunk::Char('>'),
                Chunk::Char('>'),
            ]));
        }
    } else if bytes.is_empty() {
        return Ok(Chunk::List(vec![
            Chunk::Char('<'),
            Chunk::Char('<'),
            Chunk::List(Vec::new()),
            Chunk::Char('>'),
            Chunk::Char('>'),
        ]));
    }
    // `io_lib:write_binary/3` (`io_lib.erl` `write_binary_body/4`): each byte is one
    // depth level, so at depth `D` (>= 1) the first `D - 1` bytes are shown and `...`
    // follows when bytes remain (`<<1,2,3>>` at depth 3 ⇒ `<<1,2,...>>`). A negative
    // depth is unlimited; depth `0` was handled by the caller.
    let budget = if depth < 0 {
        bytes.len()
    } else {
        (depth - 1).max(0) as usize
    };
    let shown = budget.min(bytes.len());
    let mut body: Vec<Chunk> = Vec::new();
    for (i, byte) in bytes[..shown].iter().enumerate() {
        if i != 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text(&byte.to_string()));
    }
    if shown < bytes.len() {
        if shown != 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text("..."));
    }
    Ok(Chunk::List(vec![
        Chunk::Char('<'),
        Chunk::Char('<'),
        Chunk::List(body),
        Chunk::Char('>'),
        Chunk::Char('>'),
    ]))
}

/// Renders a **non-byte-aligned** bitstring (`io_lib:write_binary/2` /
/// `io_lib_pretty:write/1`).
///
/// The oracle prints `<<` + the whole leading bytes as comma-separated decimals +
/// (when bits remain) a comma and the trailing bit field `Value:BitCount` + `>>`;
/// erts-17.1 renders e.g. `<<1:1>>` ⇒ `<<1:1>>`, `<<5:3,3:2>>` ⇒ `<<23:5>>`,
/// `<<1:9>>` ⇒ `<<0,1:1>>`, `<<255:12>>` ⇒ `<<15,15:4>>`. The nested-list
/// structure matches [`render_binary_chunk`]: the container is
/// `[<, <, body, >, >]`, each rendered number its own sub-list and every `,`/`:`
/// an individual integer.
///
/// A byte-aligned bitstring is never routed here (the binary branch of
/// [`render_chunk`] handles it), but the delegation keeps the helper total. The
/// `~p` printable-string rule does **not** apply — a non-byte-aligned bitstring is
/// never a string — yet `~p`/`~P` still collapse a depth-1 (`<<...>>`) render
/// exactly like a printable binary (the printable branch of
/// [`render_binary_chunk`]).
fn render_bitstring_chunk(term: Eterm, printable: bool, depth: i64) -> Result<Chunk> {
    let bit_len = bitstring_bit_size(term)?;
    if bit_len % 8 == 0 {
        return render_binary_chunk(term, printable, depth);
    }
    if printable && depth == 1 {
        // `io_lib_pretty` truncates a printable-bitstring render to the flat
        // `<<...>>` (mirrors the depth-1 case of `render_binary_chunk`).
        return Ok(Chunk::text("<<...>>"));
    }
    let data = bitstring_bits(term)?;
    let full_bytes = bit_len / 8;
    let trailing_bits = bit_len % 8;
    // `write_binary_body/4` consumes one depth level per body element: the
    // `full_bytes` whole bytes and then the trailing `Value:Bits` field. So at depth
    // `D` (>= 1) the first `D - 1` elements are shown and `...` follows while elements
    // remain (`<<255:12>>` at depth 2 ⇒ `<<15,...>>`); a negative depth is unlimited.
    let elements = full_bytes + 1;
    let budget = if depth < 0 {
        elements
    } else {
        (depth - 1).max(0) as usize
    };
    let shown = budget.min(full_bytes);
    let mut body: Vec<Chunk> = Vec::new();
    for (i, byte) in data[..shown].iter().enumerate() {
        if i != 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text(&byte.to_string()));
    }
    if budget > full_bytes {
        // The trailing field fits within the remaining depth.
        if full_bytes != 0 {
            body.push(Chunk::Char(','));
        }
        // `data[full_bytes]` holds the trailing `trailing_bits` bits in its most
        // significant positions (the packing is MSB-first), so shift them down.
        let trailing_value = data[full_bytes] >> (8 - trailing_bits);
        body.push(Chunk::text(&trailing_value.to_string()));
        body.push(Chunk::Char(':'));
        body.push(Chunk::text(&trailing_bits.to_string()));
    } else {
        if shown != 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text("..."));
    }
    Ok(Chunk::List(vec![
        Chunk::Char('<'),
        Chunk::Char('<'),
        Chunk::List(body),
        Chunk::Char('>'),
        Chunk::Char('>'),
    ]))
}
/// The byte values of `term` iff it is a non-empty proper list of printable latin1
/// characters (the `~p` printable-string rule; see [`char_printable`]).
fn printable_string(term: Eterm) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = car(cur);
        if !is_small(head) {
            return None;
        }
        let value = signed_value(head);
        if !char_printable(value) {
            return None;
        }
        bytes.push(value as u8);
        cur = cdr(cur);
    }
    if !is_nil(cur) || bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

/// Whether every byte is printable latin1 and the binary is non-empty (the `~p`
/// printable-binary rule, mirroring [`printable_string`]).
fn printable_bytes(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| latin1_printable_char(b))
}

/// Renders `bytes` as a double-quoted string, escaping `"`, `\` and the latin1
/// control characters the oracle's `io_lib:string_char/4` escapes (`\b \t \n \v \f
/// \r \e`). Bytes `16#A0..=16#FF` are emitted verbatim as their Unicode code point
/// (the enclosing [`Chunk`] is a Rust `String`, so the oracle's UTF-8 output — e.g.
/// `c2 a0` for `16#A0` — comes out byte-for-byte).
fn quoted_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &byte in bytes {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0B => out.push_str("\\v"),
            0x0C => out.push_str("\\f"),
            0x0D => out.push_str("\\r"),
            0x1B => out.push_str("\\e"),
            _ => out.push(char::from(byte)),
        }
    }
    out.push('"');
    out
}
/// Renders a bignum in decimal (`erts_big_to_string(wobj, 10, …)`).
///
/// The digits are read with the safe [`beam_codec::term::big_parts`] wrapper (the
/// crate's `access.rs` exposes no bignum digit accessor and is the crate's only
/// `#[allow(unsafe_code)]` module; `beam_codec` is already a declared dependency).
fn format_big(term: Eterm) -> Result<String> {
    let (negative, digits) = big_parts(term)?;
    Ok(format_big_decimal(negative, digits))
}

/// Converts a least-significant-first `u64` digit vector to decimal.
fn format_big_decimal(negative: bool, digits: &[u64]) -> String {
    let mut limbs: Vec<u64> = digits.to_vec();
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut reversed: Vec<char> = Vec::new();
    loop {
        // One base-2^64 long division by 10 (the remainder is the next decimal
        // digit, least significant first).
        let mut rem: u128 = 0;
        let mut nonzero = false;
        for i in (0..limbs.len()).rev() {
            let current = (rem << 64) | u128::from(limbs[i]);
            let quotient = (current / 10) as u64;
            limbs[i] = quotient;
            nonzero |= quotient != 0;
            rem = current % 10;
        }
        reversed.push(char::from(b'0' + rem as u8));
        if !nonzero {
            break;
        }
    }
    let mut out: String = reversed.iter().rev().collect();
    if negative {
        out.insert(0, '-');
    }
    out
}

/// Renders an atom name the way the oracle's atom leaf does — `~w`/`~p` take the
/// latin1 clause of `io_lib:write1/4`, i.e. `io_lib:write_atom_as_latin1/1`:
/// **bare** when [`is_bare_atom`] (the oracle's `io_lib:quote_atom/2` is `false`),
/// otherwise wrapped in `'…'` with every character escaped by [`escape_atom_char`]
/// (`io_lib:string_char(unicode_as_latin1, …)`).
///
/// So `héllo` renders bare (`io_lib:format("~w",['héllo'])` ⇒
/// `[104,233,108,108,111]`), while `_foo`/`Hello`/`a b`/`and`/a control character/
/// `16#7F`/`16#80..16#9F`/`a÷b` are quoted, and a character above `16#FF` becomes
/// the `\x{H}` escape inside the quotes (`Ā` ⇒ `'\x{100}'`).
fn format_atom(name: &str) -> String {
    if is_bare_atom(name) {
        return name.to_string();
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('\'');
    for c in name.chars() {
        escape_atom_char(c, &mut out);
    }
    out.push('\'');
    out
}

/// `io_lib:quote_atom/2` — whether the atom name needs **no** quotes.
///
/// The name must not be an Erlang reserved word ([`is_reserved_word`]), its
/// **first** character must be a lowercase letter (`a..z`, or `ß..ÿ` except `÷`)
/// and every character must satisfy the oracle's `name_char/1`: `a..z`, `ß..ÿ`
/// except `÷`, `A..Z`, `À..Þ` except `×`, `0..9`, `_` or `@` (`io_lib.erl`).
///
/// This is **not** the `~p` printable-latin1 rule ([`latin1_printable_char`] /
/// `io_lib:printable_latin1_list/1`), which is a different predicate: a space,
/// `16#A0`, a leading digit or a leading uppercase letter are *printable* yet
/// still force quotes here, while `héllo` (bare) and `a÷b` (quoted, `÷` is
/// excluded) are rendered exactly as the oracle renders them.
fn is_bare_atom(name: &str) -> bool {
    if is_reserved_word(name) {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if is_atom_name_initial(first) => chars.all(is_atom_name_char),
        _ => false,
    }
}

/// `quote_atom/2`'s first-character clause: `a..z`, or `ß..ÿ` except `÷`.
fn is_atom_name_initial(c: char) -> bool {
    matches!(c, 'a'..='z') || (('\u{00DF}'..='\u{00FF}').contains(&c) && c != '\u{00F7}')
}

/// The oracle's `name_char/1`: `a..z`, `ß..ÿ` except `÷`, `A..Z`, `À..Þ` except
/// `×`, `0..9`, `_` and `@`.
fn is_atom_name_char(c: char) -> bool {
    matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '@')
        || (('\u{00DF}'..='\u{00FF}').contains(&c) && c != '\u{00F7}')
        || (('\u{00C0}'..='\u{00DE}').contains(&c) && c != '\u{00D7}')
}

/// `erl_scan:reserved_word/1` — the fixed reserved words of `erl_scan.erl` plus
/// the `erl_features:keywords/0` set of the default feature configuration
/// (`maybe`/`else`; the `maybe_expr` feature is on by default in erts-17.1, so a
/// default oracle quotes both). A reserved word is always quoted, e.g.
/// `io_lib:format("~w",['and'])` ⇒ `"'and'"` while `'rec'`/`'query'` stay bare.
fn is_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "after"
            | "begin"
            | "case"
            | "try"
            | "cond"
            | "catch"
            | "andalso"
            | "orelse"
            | "end"
            | "fun"
            | "if"
            | "let"
            | "of"
            | "receive"
            | "when"
            | "bnot"
            | "not"
            | "div"
            | "rem"
            | "band"
            | "and"
            | "bor"
            | "bxor"
            | "bsl"
            | "bsr"
            | "or"
            | "xor"
            | "maybe"
            | "else"
    )
}

/// `io_lib:string_char(unicode_as_latin1, C, $\', Tail)` — one character of a
/// **quoted** atom, appended to `out`.
///
/// The oracle's clause order is reproduced exactly: the quote character and `\`
/// escape first, then `16#20..16#7E` verbatim, then `16#A0..16#FF` verbatim (the
/// `unicode_as_latin1` clause), then anything above `16#FF` as `\x{Hex}` with
/// **uppercase** hex digits (`integer_to_list(C, 16)`), then the `\n \r \t \v \b
/// \f \e` mnemonics, `16#7F` as `\d`, and every remaining character below `16#A0`
/// as three octal digits. So `'\x{0}'` ⇒ `'\000'`, `'\x{1B}'` ⇒ `'\e'`,
/// `'\x{7F}'` ⇒ `'\d'`, `'\x{80}'` ⇒ `'\200'`, `Ā` (U+0100) ⇒ `'\x{100}'`,
/// `a'b` ⇒ `a\'b`, and `a"b`/`'a\xA0b'` keep `"`/`16#A0` verbatim.
fn escape_atom_char(c: char, out: &mut String) {
    let code = c as u32;
    match c {
        '\'' => out.push_str("\\'"),
        '\\' => out.push_str("\\\\"),
        _ if (0x20..=0x7E).contains(&code) => out.push(c),
        _ if (0xA0..=0xFF).contains(&code) => out.push(c),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\u{0B}' => out.push_str("\\v"),
        '\u{08}' => out.push_str("\\b"),
        '\u{0C}' => out.push_str("\\f"),
        '\u{1B}' => out.push_str("\\e"),
        '\u{7F}' => out.push_str("\\d"),
        _ if code > 0xFF => {
            out.push_str("\\x{");
            out.push_str(&format!("{code:X}"));
            out.push('}');
        }
        // Every other character below `16#A0`: three octal digits.
        _ => {
            out.push('\\');
            for shift in [6, 3, 0] {
                out.push(char::from(b'0' + ((code >> shift) & 7) as u8));
            }
        }
    }
}

/// Renders a float exactly as the oracle's plain (non-`~e`/`~f`/`~g`) float leaf
/// does: `io_lib:write1/4`'s float clause is `io_lib_format:fwrite_g(Fl)`, and
/// `fwrite_g/1` is `float_to_list(Fl, [short])` (`io_lib_format.erl:758-760`).
/// So the rendering is [`crate::numeric::format_short`] — the same
/// compatibility-pinned port `float_to_list/2` uses, never a second renderer.
///
/// # Errors
///
/// As [`crate::numeric::format_short`]. The guard is unreachable for a real
/// `f64` (the shortest round-trip form is well under 255 characters).
fn format_float(value: f64) -> Result<String> {
    if !value.is_finite() {
        // Outside the oracle's domain: OTP cannot hold an `inf`/`NaN` float at
        // all (`binary_to_term/1` refuses such a `NEW_FLOAT_EXT`), so no float
        // the oracle can *print* reaches here. `format_short`'s shortest-digit
        // search only accepts a finite non-zero value, so keep the debug-style
        // spelling rather than panic; the BIF arithmetic that can fabricate one
        // is a separate, recorded gap.
        return Ok(format!("{value}"));
    }
    crate::numeric::format_short(value)
}

/// Renders an internal ref as `#Ref<0.0.0.Id>` — the `%T` text
/// `erlang:ref_to_list/1` renders, from its one home in
/// [`crate::term_conv::ref_text_str`] (Milestone-1 refs carry no channel, node or
/// serial — see the module docs).
fn format_ref(term: Eterm) -> Result<String> {
    crate::term_conv::ref_text_str(term)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    /// Runs `f` over a fresh context whose atom table is seeded with the names the
    /// tests build terms from.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        with_ctx_capacity(1024, f)
    }

    /// [`with_ctx`] over a heap of `capacity_words` term words.
    ///
    /// The long-container regression tests below build a flat container with tens of
    /// thousands of elements, which does not fit the 1024-word default heap (a
    /// [`Heap`] never reallocates; exhaustion is a typed `Alloc` error).
    fn with_ctx_capacity<R>(capacity_words: usize, f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(capacity_words).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "Hello", "a", "b", "c", "abc", "hi"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// [`with_ctx`] over an atom table additionally seeded with `extra` (the fun
    /// tests build `lists`/`reverse`/`nonode@nohost` terms the default seed lacks).
    fn with_ctx_seeded<R>(extra: &[&str], f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(1024).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "Hello", "a", "b", "c", "abc", "hi"]);
        atoms.seed(extra.iter().copied());
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// Runs `f` on a thread with a deliberately small (256 KiB) stack.
    ///
    /// The three long-container tests below render a flat list/tuple/map with tens of
    /// thousands of elements. The render helpers used to recurse **once per element**
    /// (`render_tail_chunk`/`render_tuple_rest_chunk`/`render_map_body_chunk`), so on
    /// this small stack the walk overflowed the native stack and Rust aborted the
    /// whole test process (`fatal runtime error: stack overflow`). Running the render
    /// here pins that per-element stack-overflow regression: the walk must be total.
    fn on_small_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(f)
            .expect("the regression thread spawns")
            .join()
            .expect("the render must not overflow the native stack")
    }

    /// Elements per long list/tuple and pairs per large map in the regression tests.
    ///
    /// 20_000 elements overflowed a 256 KiB stack through the *old* per-element
    /// recursion (measured: the render thread aborts with
    /// `fatal runtime error: stack overflow`), and leaves ~2× headroom over the
    /// ~8_000-element breaking point at this stack size.
    const LONG_CONTAINER_LEN: i64 = 20_000;
    const LARGE_MAP_PAIRS: i64 = 20_000;

    /// Renders `term` with `printable`, asserting success.
    fn show(ctx: &mut BifContext<'_>, term: Eterm, printable: bool) -> String {
        format_term(ctx, term, printable).expect("term is printable")
    }

    /// Renders `term` at `depth`.
    fn show_at(ctx: &mut BifContext<'_>, term: Eterm, printable: bool, depth: i64) -> String {
        format_term_depth(ctx, term, printable, depth).expect("term is printable")
    }

    /// A flat map whose keys are stored in the canonical flat-map key order
    /// ([`beam_term::compare::cmp_flatmap_keys`]).
    ///
    /// [`crate::access::make_flatmap`] preserves the pair order it is handed, so the
    /// writer canonicalises first — exactly the step `maps.rs::build_map` performs (and
    /// what `beam_term::map::write_map` sorts for). A real flat map always stores its
    /// keys in that order, which is why the printer renders the map's *storage* order
    /// without re-sorting.
    fn canonical_flatmap(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let mut pairs = pairs.to_vec();
        pairs.sort_by(|a, b| beam_term::compare::cmp_flatmap_keys(a.0, b.0, ctx.atoms()));
        crate::access::make_flatmap(ctx.heap(), &pairs).expect("the flat map fits")
    }

    #[test]
    fn small_ints_and_atoms() {
        with_ctx(|ctx| {
            let positive = ctx.int(7).expect("fits");
            assert_eq!(show(ctx, positive, false), "7");
            let negative = ctx.int(-5).expect("fits");
            assert_eq!(show(ctx, negative, false), "-5");

            let hello = ctx.atom("hello").expect("seeded");
            assert_eq!(show(ctx, hello, false), "hello");
            // `Hello` is not a bare name (`[a-z]…`), so it is quoted.
            let quoted = ctx.atom("Hello").expect("seeded");
            assert_eq!(show(ctx, quoted, false), "'Hello'");
        });
    }

    #[test]
    fn tuple_and_lists() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one]).expect("fits");
            assert_eq!(show(ctx, tuple, false), "{a,1}");

            let two = ctx.int(2).expect("fits");
            let tail = ctx.cons(two, NIL).expect("fits");
            let list = ctx.cons(one, tail).expect("fits");
            assert_eq!(show(ctx, list, false), "[1,2]");

            // `[]` is `NIL`, `{}` the empty tuple.
            assert_eq!(show(ctx, NIL, false), "[]");
            assert_eq!(show(ctx, NIL, true), "[]");
            let empty = ctx.tuple(&[]).expect("fits");
            assert_eq!(show(ctx, empty, false), "{}");
            assert_eq!(show_at(ctx, empty, false, 1), "{}");

            // An improper list renders `[head|tail]`.
            let b = ctx.atom("b").expect("seeded");
            let improper = ctx.cons(a, b).expect("fits");
            assert_eq!(show(ctx, improper, false), "[a|b]");
            assert_eq!(show(ctx, improper, true), "[a|b]");
        });
    }

    #[test]
    fn floats_render_as_float_to_list_short() {
        with_ctx(|ctx| {
            // The oracle (`erl -noshell -eval 'io:format("~w~n",[V])'`) renders
            // every one of these as `float_to_list(V, [short])`.
            for (value, expected) in [
                (0.0f64, "0.0"),
                (-0.0, "-0.0"),
                (1.0, "1.0"),
                (1.5, "1.5"),
                (0.1, "0.1"),
                (3.14159, "3.14159"),
                (-2.5, "-2.5"),
                (100.0, "100.0"),
                (100.5, "100.5"),
                // The magnitude rule: the decimal notation only survives while it
                // is no longer than the scientific one.
                (1.0e-4, "0.0001"),
                (1.0e-5, "1.0e-5"),
                (1.0e-6, "1.0e-6"),
                (1.0e5, "1.0e5"),
                (1.0e10, "1.0e10"),
                (1.0e16, "1.0e16"),
                (1.0e20, "1.0e20"),
                (1.0e100, "1.0e100"),
                (1.0e-300, "1.0e-300"),
                (1.0e9, "1.0e9"),
                (123456789.5, "123456789.5"),
                (1234567895.0, "1234567895.0"),
                (12345678900.0, "12345678900.0"),
                (1.2345e-5, "1.2345e-5"),
                (9.1e15, "9.1e15"),
                (1.5e-4, "1.5e-4"),
                (2.5e-7, "2.5e-7"),
                (3.141592653589793, "3.141592653589793"),
                (f64::MAX, "1.7976931348623157e308"),
                (5.0e-324, "5.0e-324"),
                (f64::MIN_POSITIVE, "2.2250738585072014e-308"),
            ] {
                let term = ctx.make_float(value).expect("fits");
                assert_eq!(show(ctx, term, false), expected, "~w of {value:?}");
                assert_eq!(show(ctx, term, true), expected, "~p of {value:?}");
                assert_eq!(show_at(ctx, term, false, 3), expected, "~W of {value:?}");
                assert_eq!(show_at(ctx, term, true, 3), expected, "~P of {value:?}");
            }
        });
    }

    #[test]
    fn a_non_finite_float_is_outside_the_oracle_domain() {
        with_ctx(|ctx| {
            // OTP cannot hold `inf`/`NaN` (its `binary_to_term/1` rejects a
            // `NEW_FLOAT_EXT` with exponent `16#7FF`), so there is nothing to
            // compare against; the printer must not panic on one.
            for (value, expected) in [(f64::INFINITY, "inf"), (f64::NEG_INFINITY, "-inf")] {
                let term = ctx.make_float(value).expect("fits");
                assert_eq!(show(ctx, term, false), expected);
            }
            let nan = ctx.make_float(f64::NAN).expect("fits");
            assert_eq!(show(ctx, nan, false), "NaN");
        });
    }

    #[test]
    fn floats_render_the_same_nested() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let ten = ctx.make_float(1.0e10).expect("fits");
            let five = ctx.make_float(1.0e5).expect("fits");
            let small = ctx.make_float(1.0e-6).expect("fits");

            let tuple = ctx.tuple(&[five, small]).expect("fits");
            let tail = ctx.cons(tuple, NIL).expect("fits");
            let list = ctx.cons(ten, tail).expect("fits");
            // `erl -noshell -eval 'io:format("~p~n",[[1.0e10,{1.0e5,1.0e-6}]])'`.
            assert_eq!(show(ctx, list, false), "[1.0e10,{1.0e5,1.0e-6}]");
            assert_eq!(show(ctx, list, true), "[1.0e10,{1.0e5,1.0e-6}]");

            let map = canonical_flatmap(ctx, &[(a, ten)]);
            assert_eq!(show(ctx, map, false), "#{a => 1.0e10}");
        });
    }

    #[test]
    fn printable_string_rule() {
        with_ctx(|ctx| {
            // "abc" == [97,98,99].
            let abc = ctx
                .list(&[
                    ctx.int(97).expect("fits"),
                    ctx.int(98).expect("fits"),
                    ctx.int(99).expect("fits"),
                ])
                .expect("fits");
            assert_eq!(show(ctx, abc, true), "\"abc\"");
            assert_eq!(show(ctx, abc, false), "[97,98,99]");

            // A byte below 32 disqualifies the whole list (even with `~p`).
            let controls = ctx
                .list(&[ctx.int(31).expect("fits"), ctx.int(32).expect("fits")])
                .expect("fits");
            assert_eq!(show(ctx, controls, true), "[31,32]");

            // 127 is outside `32..=126`.
            let high = ctx.list(&[ctx.int(127).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, high, true), "[127]");

            // Escaping: `"` (34) and `\` (92).
            let quote = ctx.list(&[ctx.int(34).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, quote, true), "\"\\\"\"");
            let backslash = ctx.list(&[ctx.int(92).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, backslash, true), "\"\\\\\"");
        });
    }

    #[test]
    fn printable_rule_propagates_into_tuples() {
        with_ctx(|ctx| {
            let ok = ctx.atom("ok").expect("seeded");
            let hi = ctx
                .list(&[ctx.int(104).expect("fits"), ctx.int(105).expect("fits")])
                .expect("fits");
            let float = ctx.make_float(1.5).expect("fits");
            let tuple = ctx.tuple(&[ok, hi, float]).expect("fits");
            assert_eq!(show(ctx, tuple, true), "{ok,\"hi\",1.5}");
        });
    }

    #[test]
    fn nested_lists_respect_the_rule() {
        with_ctx(|ctx| {
            let inner1 = ctx
                .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            let inner2 = ctx.list(&[ctx.int(3).expect("fits")]).expect("fits");
            let outer = ctx.list(&[inner1, inner2]).expect("fits");
            assert_eq!(show(ctx, outer, true), "[[1,2],[3]]");
        });
    }

    #[test]
    fn binaries_render_in_the_oracle_form() {
        with_ctx(|ctx| {
            // `~w` of `<<"abc">>` is `<<97,98,99>>`; `~p` is `<<"abc">>`.
            // (`erl -noshell -eval 'io:format("~p~n",[io_lib:format("~w",[<<"abc">>])])'`)
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show(ctx, abc, false), "<<97,98,99>>");
            assert_eq!(show(ctx, abc, true), "<<\"abc\">>");

            // Non-printable bytes stay in the `~w` form under `~p` too.
            let low = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show(ctx, low, false), "<<1,2,3>>");
            assert_eq!(show(ctx, low, true), "<<1,2,3>>");

            // An empty binary is `<<>>` in both forms (oracle).
            let empty = bits::make_binary(ctx.heap(), &[]).expect("fits");
            assert_eq!(show(ctx, empty, false), "<<>>");
            assert_eq!(show(ctx, empty, true), "<<>>");
            assert_eq!(show_at(ctx, empty, true, 1), "<<>>");

            // `"`/`\` escape inside the `~p` string form.
            let esc = bits::make_binary(ctx.heap(), b"\"\\").expect("fits");
            assert_eq!(show(ctx, esc, true), "<<\"\\\"\\\\\">>");
        });
    }

    #[test]
    fn non_byte_aligned_bitstrings_render_in_the_oracle_form() {
        with_ctx(|ctx| {
            // All pinned to erts-17.1, e.g.
            // `erl -noshell -eval 'io:format("~w~n",[<<1:1>>]).'`.
            // `<<1:1>>` — one bit, no whole leading byte.
            let one = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert_eq!(show(ctx, one, false), "<<1:1>>");
            assert_eq!(show(ctx, one, true), "<<1:1>>");
            // `<<5:3,3:2>>` = `10111` ⇒ `<<23:5>>`.
            let five = bits::make_bitstring(ctx.heap(), &[0b1011_1000], 5).expect("fits");
            assert_eq!(show(ctx, five, false), "<<23:5>>");
            // `<<1:9>>` — one whole leading byte then one trailing bit.
            let nine = bits::make_bitstring(ctx.heap(), &[0x00, 0b1000_0000], 9).expect("fits");
            assert_eq!(show(ctx, nine, false), "<<0,1:1>>");
            // `<<255:12>>` = `00001111 1111` ⇒ `<<15,15:4>>`.
            let twelve = bits::make_bitstring(ctx.heap(), &[0x0F, 0b1111_0000], 12).expect("fits");
            assert_eq!(show(ctx, twelve, false), "<<15,15:4>>");
            // `<<0:3>>` — every bit zero.
            let zero = bits::make_bitstring(ctx.heap(), &[0], 3).expect("fits");
            assert_eq!(show(ctx, zero, false), "<<0:3>>");

            // Nested in a tuple and a list (oracle).
            let a = ctx.atom("a").expect("seeded");
            let tuple = ctx.tuple(&[one, a]).expect("fits");
            assert_eq!(show(ctx, tuple, false), "{<<1:1>>,a}");
            let two = ctx.int(2).expect("fits");
            let list = ctx.list(&[one, two]).expect("fits");
            assert_eq!(show(ctx, list, false), "[<<1:1>>,2]");

            // Depth: `~W`/`~P` spend one level per body element (each whole byte, plus
            // the trailing `Value:Bits` field), then emit `...`.
            assert_eq!(show_at(ctx, one, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, one, true, 1), "<<...>>");
            assert_eq!(show_at(ctx, one, false, 0), "...");
            assert_eq!(show_at(ctx, one, false, 2), "<<1:1>>");
            // `<<255:12>>`: `d=2` ⇒ `<<15,...>>`, `d=3` ⇒ full.
            assert_eq!(show_at(ctx, twelve, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, twelve, false, 2), "<<15,...>>");
            assert_eq!(show_at(ctx, twelve, false, 3), "<<15,15:4>>");
            // `<<1:9>>`: one whole byte then the trailing bit.
            assert_eq!(show_at(ctx, nine, false, 2), "<<0,...>>");
            assert_eq!(show_at(ctx, nine, false, 3), "<<0,1:1>>");
            // `<<5:3,3:2>>` is a single element, so it survives depth 2 intact.
            assert_eq!(show_at(ctx, five, false, 2), "<<23:5>>");
        });
    }

    #[test]
    fn maps_render_with_spaces_around_the_arrow() {
        with_ctx(|ctx| {
            // `~w` of `#{1 => 2, 3 => 4}` is `#{1 => 2,3 => 4}` (oracle).
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let three = ctx.int(3).expect("fits");
            let four = ctx.int(4).expect("fits");
            let map = canonical_flatmap(ctx, &[(three, four), (one, two)]);
            assert_eq!(show(ctx, map, false), "#{1 => 2,3 => 4}");
            assert_eq!(show(ctx, map, true), "#{1 => 2,3 => 4}");

            // `~p` of `#{<<"k">> => 1}` is `#{<<"k">> => 1}` (printable propagates).
            let key = bits::make_binary(ctx.heap(), b"k").expect("fits");
            let map = crate::access::make_flatmap(ctx.heap(), &[(key, one)]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{<<107>> => 1}");
            assert_eq!(show(ctx, map, true), "#{<<\"k\">> => 1}");

            // An empty map is `#{}`.
            let empty = ctx.atom("hi").expect("seeded");
            let map = crate::access::make_flatmap(ctx.heap(), &[]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{}");
            assert_eq!(show(ctx, empty, false), "hi");
        });
    }

    #[test]
    fn atom_keys_sort_by_atom_index_and_others_by_term_order() {
        with_ctx(|ctx| {
            // Atoms `b` then `a`: `a` was seeded after `b`? The seeded order is
            // ["true","false","ok","hello","Hello","a","b","c","abc","hi"], so `a`
            // has a lower index than `b` and sorts first (oracle rule
            // `erts_cmp_flatmap_keys`: atom keys by raw term word).
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let map = canonical_flatmap(ctx, &[(b, one), (a, two)]);
            assert_eq!(show(ctx, map, false), "#{a => 2,b => 1}");

            // Mixed classes use the term order (`1` < `a` < `{a}`).
            let tuple = ctx.tuple(&[a]).expect("fits");
            let map = canonical_flatmap(ctx, &[(tuple, one), (a, two), (one, one)]);
            assert_eq!(show(ctx, map, false), "#{1 => 1,a => 2,{a} => 1}");
        });
    }

    #[test]
    fn depth_limit_matches_io_lib_write() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let inner = ctx.tuple(&[c, make_small(1)]).expect("fits");
            let mid = ctx.tuple(&[b, inner]).expect("fits");
            let outer = ctx.tuple(&[a, mid]).expect("fits");
            // `~W` of `{a,{b,{c,1}}}`: -1 ⇒ full, 4 ⇒ `{a,{b,...}}`, 3 ⇒ `{a,{...}}`,
            // 2 ⇒ `{a,...}`, 1 ⇒ `{...}`, 0 ⇒ `...`.
            assert_eq!(show_at(ctx, outer, false, -1), "{a,{b,{c,1}}}");
            assert_eq!(show_at(ctx, outer, false, 4), "{a,{b,...}}");
            assert_eq!(show_at(ctx, outer, false, 3), "{a,{...}}");
            assert_eq!(show_at(ctx, outer, false, 2), "{a,...}");
            assert_eq!(show_at(ctx, outer, false, 1), "{...}");
            assert_eq!(show_at(ctx, outer, false, 0), "...");

            // `~W` of `[1,2,3,4,5]`.
            let list = ctx
                .list(&[
                    make_small(1),
                    make_small(2),
                    make_small(3),
                    make_small(4),
                    make_small(5),
                ])
                .expect("fits");
            assert_eq!(show_at(ctx, list, false, 3), "[1,2|...]");
            assert_eq!(show_at(ctx, list, false, 2), "[1|...]");
            assert_eq!(show_at(ctx, list, false, 1), "[...]");
            assert_eq!(show_at(ctx, list, false, 6), "[1,2,3,4,5]");
            assert_eq!(show_at(ctx, NIL, false, 1), "[]");
        });
    }

    #[test]
    fn binary_and_map_depth_limit() {
        with_ctx(|ctx| {
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show_at(ctx, bin, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, bin, false, 2), "<<1,...>>");
            // A printable binary at depth 1 is still elided (oracle).
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show_at(ctx, abc, true, 1), "<<...>>");

            // `~W` of `#{a=>1,b=>2,c=>3}`: 1 ⇒ `#{}`, 2 ⇒ first pair then `,...`,
            // 3 ⇒ two pairs then `,...`.
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let map = crate::access::make_flatmap(
                ctx.heap(),
                &[(a, make_small(1)), (b, make_small(2)), (c, make_small(3))],
            )
            .expect("fits");
            assert_eq!(show_at(ctx, map, false, 1), "#{}");
            assert_eq!(show_at(ctx, map, false, 2), "#{a => 1,...}");
            assert_eq!(show_at(ctx, map, false, 3), "#{a => 1,b => 2,...}");
        });
    }

    /// The `~W` binary depth rule, pinned to the erts-17.1 oracle
    /// (`io_lib:write_binary/3`'s `write_binary_body/4`: one depth level per byte,
    /// then `...` while bytes remain).
    #[test]
    fn binary_depth_limit_matches_io_lib_write() {
        with_ctx(|ctx| {
            // `erl -noshell -eval 'io:format("~W~n",[<<1,2,3>>,2]).'` ⇒ `<<1,...>>`.
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show_at(ctx, bin, false, 0), "...");
            assert_eq!(show_at(ctx, bin, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, bin, false, 2), "<<1,...>>");
            assert_eq!(show_at(ctx, bin, false, 3), "<<1,2,...>>");
            assert_eq!(show_at(ctx, bin, false, 4), "<<1,2,3>>");
            assert_eq!(show_at(ctx, bin, false, 5), "<<1,2,3>>");
            assert_eq!(show_at(ctx, bin, false, UNLIMITED_DEPTH), "<<1,2,3>>");

            // `~W` (`printable = false`) renders a printable binary in decimal, with
            // the same one-level-per-byte budget.
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show_at(ctx, abc, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, abc, false, 2), "<<97,...>>");
            assert_eq!(show_at(ctx, abc, false, 3), "<<97,98,...>>");
            assert_eq!(show_at(ctx, abc, false, 4), "<<97,98,99>>");
            assert_eq!(show_at(ctx, abc, false, 5), "<<97,98,99>>");

            // An empty binary is `<<>>` at every depth.
            let empty = bits::make_binary(ctx.heap(), &[]).expect("fits");
            assert_eq!(show_at(ctx, empty, false, 1), "<<>>");
            assert_eq!(show_at(ctx, empty, false, 3), "<<>>");
            assert_eq!(show_at(ctx, empty, false, UNLIMITED_DEPTH), "<<>>");

            // The depth is consumed across sibling elements, exactly as for any other
            // container (`{<<1,...>>,<<...>>}` at depth 3).
            let tuple = ctx.tuple(&[bin, bin]).expect("fits");
            assert_eq!(show_at(ctx, tuple, false, 2), "{<<...>>,...}");
            assert_eq!(show_at(ctx, tuple, false, 3), "{<<1,...>>,<<...>>}");
            assert_eq!(show_at(ctx, tuple, false, 4), "{<<1,2,...>>,<<1,...>>}");
            assert_eq!(show_at(ctx, tuple, false, 6), "{<<1,2,3>>,<<1,2,3>>}");

            let list = ctx.list(&[bin, bin]).expect("fits");
            assert_eq!(show_at(ctx, list, false, 2), "[<<...>>|...]");
            assert_eq!(show_at(ctx, list, false, 3), "[<<1,...>>,<<...>>]");
            assert_eq!(show_at(ctx, list, false, 4), "[<<1,2,...>>,<<1,...>>]");
            assert_eq!(show_at(ctx, list, false, 6), "[<<1,2,3>>,<<1,2,3>>]");

            let a = ctx.atom("a").expect("seeded");
            let map = crate::access::make_flatmap(ctx.heap(), &[(a, bin)]).expect("fits");
            assert_eq!(show_at(ctx, map, false, 1), "#{}");
            assert_eq!(show_at(ctx, map, false, 2), "#{a => <<...>>,...}");
            assert_eq!(show_at(ctx, map, false, 3), "#{a => <<1,...>>}");
            assert_eq!(show_at(ctx, map, false, 5), "#{a => <<1,2,3>>}");
        });
    }

    #[test]
    fn bignums_render_in_decimal() {
        with_ctx(|ctx| {
            // 123456789012345678901234567890 = 0x18EE90FF6C373E0EE4E3F0AD2.
            // `erl -noshell -eval 'io:format("~w~n",[123456789012345678901234567890])'`
            let value = 123456789012345678901234567890u128;
            let low = value as u64;
            let high = (value >> 64) as u64;
            let big = beam_codec::term::make_big(ctx.heap(), false, &[low, high]).expect("fits");
            assert_eq!(show(ctx, big, false), "123456789012345678901234567890");
            assert_eq!(show(ctx, big, true), "123456789012345678901234567890");

            // A negative bignum keeps the sign.
            let neg = beam_codec::term::make_big(ctx.heap(), true, &[low, high]).expect("fits");
            assert_eq!(show(ctx, neg, false), "-123456789012345678901234567890");

            // 2**128 (a single high digit) — `erl` prints `340282366920938463463374607431768211456`.
            let pow = beam_codec::term::make_big(ctx.heap(), false, &[0, 0, 1]).expect("fits");
            assert_eq!(show(ctx, pow, false), "340282366920938463463374607431768211456");
        });
    }

    #[test]
    fn big_decimal_conversion_edge_cases() {
        assert_eq!(format_big_decimal(false, &[0]), "0");
        assert_eq!(format_big_decimal(false, &[9]), "9");
        assert_eq!(format_big_decimal(false, &[10, 0]), "10");
        assert_eq!(format_big_decimal(true, &[255]), "-255");
        // 2**64 = 18446744073709551616.
        assert_eq!(format_big_decimal(false, &[0, 1]), "18446744073709551616");
    }

    #[test]
    fn internal_refs_render_with_the_ref_shape() {
        with_ctx(|ctx| {
            let reference = crate::access::make_ref_thing(ctx.heap(), 42).expect("fits");
            assert_eq!(show(ctx, reference, false), "#Ref<0.0.0.42>");
            assert_eq!(show(ctx, reference, true), "#Ref<0.0.0.42>");

            let pid = ProcessId::new(10, 0).to_eterm();
            assert_eq!(show(ctx, pid, false), "<0.10.0>");
        });
    }

    #[test]
    fn bare_atom_name_rule() {
        // `io_lib:quote_atom/2` — bare names.
        assert!(is_bare_atom("a"));
        assert!(is_bare_atom("hello"));
        assert!(is_bare_atom("x1_@z"));
        assert!(is_bare_atom("foo@bar"));
        assert!(is_bare_atom("héllo"));
        assert!(is_bare_atom("café"));
        assert!(is_bare_atom("àbc"));
        assert!(is_bare_atom("ß"));
        assert!(is_bare_atom("ÿ"));
        assert!(is_bare_atom("a1@_Z"));
        // Quoted: an empty name, a bad first character, a bad later character.
        assert!(!is_bare_atom(""));
        assert!(!is_bare_atom("Foo"));
        assert!(!is_bare_atom("_foo"));
        assert!(!is_bare_atom("1x"));
        assert!(!is_bare_atom("a-b"));
        assert!(!is_bare_atom(" a"));
        assert!(!is_bare_atom("a b"));
        assert!(!is_bare_atom("a.b"));
        assert!(!is_bare_atom("a\nb"));
        // `÷` is excluded from `ß..ÿ` in both positions, `×` from `À..Þ`.
        assert!(!is_bare_atom("÷a"));
        assert!(!is_bare_atom("a÷"));
        assert!(!is_bare_atom("a×b"));
        assert!(is_bare_atom("aØb"));
        // A leading character above `16#FF` is never a name initial.
        assert!(!is_bare_atom("Āa"));
        assert!(!is_bare_atom("aĀ"));
        // Reserved words are always quoted, other short names are not.
        assert!(!is_bare_atom("and"));
        assert!(is_bare_atom("rec"));
        assert!(is_bare_atom("query"));
        assert_eq!(format_atom("_foo"), "'_foo'");
        assert_eq!(format_atom(""), "''");
        assert_eq!(format_atom("héllo"), "héllo");
        assert_eq!(format_atom("and"), "'and'");
    }

    #[test]
    fn atom_quoting_and_escaping_match_the_oracle() {
        // Every expectation below is the flattened `io_lib:format("~w",[A])` of the
        // system `erl` (OTP 29.1.1 / erts-17.1), probed with
        // `binary_to_atom(<<B>>, latin1)` for the whole byte range and with
        // `binary_to_atom(unicode:characters_to_binary([C]), utf8)` above it.
        let cases: &[(&str, &str)] = &[
            ("a", "a"),
            ("abc", "abc"),
            ("x1_@z", "x1_@z"),
            ("héllo", "héllo"),
            ("café", "café"),
            ("àbc", "àbc"),
            ("ß", "ß"),
            ("ÿ", "ÿ"),
            ("", "''"),
            ("Hello", "'Hello'"),
            ("_foo", "'_foo'"),
            ("1x", "'1x'"),
            ("a-b", "'a-b'"),
            ("a b", "'a b'"),
            ("a'b", "'a\\'b'"),
            ("a\\b", "'a\\\\b'"),
            // `"` is *not* escaped inside a quoted atom (the quote is `'`).
            ("a\"b", "'a\"b'"),
            // Control characters: the mnemonics, then three octal digits.
            ("a\x08b", "'a\\bb'"),
            ("a\tb", "'a\\tb'"),
            ("a\nb", "'a\\nb'"),
            ("a\x0bb", "'a\\vb'"),
            ("a\x0cb", "'a\\fb'"),
            ("a\rb", "'a\\rb'"),
            ("a\x1bb", "'a\\eb'"),
            ("\0", "'\\000'"),
            ("\x01", "'\\001'"),
            ("\x0e", "'\\016'"),
            ("\x1f", "'\\037'"),
            // `16#7F` is `\d`, `16#80..16#9F` are octal, `16#A0..16#FF` verbatim.
            ("a\x7fb", "'a\\db'"),
            ("\u{80}", "'\\200'"),
            ("a\u{9f}b", "'a\\237b'"),
            ("\u{a0}", "'\u{a0}'"),
            ("a\u{a0}b", "'a\u{a0}b'"),
            ("ø", "ø"),
            ("÷", "'÷'"),
            // Above `16#FF`: `\x{H}` with uppercase hex digits.
            ("Ā", "'\\x{100}'"),
            ("aĀ", "'a\\x{100}'"),
            ("\u{12c}", "'\\x{12C}'"),
            ("\u{7ff}", "'\\x{7FF}'"),
            ("\u{1f600}", "'\\x{1F600}'"),
            ("\u{10ffff}", "'\\x{10FFFF}'"),
            // Reserved words are quoted.
            ("and", "'and'"),
            ("end", "'end'"),
            ("fun", "'fun'"),
            ("maybe", "'maybe'"),
            ("else", "'else'"),
            ("rec", "rec"),
            ("query", "query"),
        ];
        for (name, expected) in cases {
            assert_eq!(format_atom(name), *expected, "atom {name:?}");
        }

        // The byte-code sweep of the **bareness** decision, derived from the
        // oracle's `quote_atom/2`: a single-character name is bare iff the
        // character is an `a..z` / `ß..ÿ`-minus-`÷` initial (a one-character name
        // has no later character to check).
        for code in 0u32..=0x2FF {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            let name = c.to_string();
            let expected = matches!(c, 'a'..='z')
                || (('\u{00DF}'..='\u{00FF}').contains(&c) && c != '\u{00F7}');
            assert_eq!(is_bare_atom(&name), expected, "single char {code:#X}");
        }

        // The **later-character** sweep: `"a" + C` is bare iff `name_char(C)`.
        for code in 0u32..=0x2FF {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            let name = format!("a{c}");
            let expected = matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '@')
                || (('\u{00DF}'..='\u{00FF}').contains(&c) && c != '\u{00F7}')
                || (('\u{00C0}'..='\u{00DE}').contains(&c) && c != '\u{00D7}');
            assert_eq!(is_bare_atom(&name), expected, "trailing char {code:#X}");
        }

        // The escaping sweep for the whole latin1 range: every byte `0..=255` in
        // a name that is always quoted (`"a" + C + " "`). The oracle's expected
        // escape is spelled out by the same `string_char(unicode_as_latin1, …)`
        // clause table (probed for all 256 values).
        for code in 0u32..=0xFF {
            let c = char::from_u32(code).expect("a byte is a char");
            let name = format!("a{c} ");
            let rendered = format_atom(&name);
            assert!(
                rendered.starts_with('\'') && rendered.ends_with('\''),
                "{code:#X}"
            );
            let inner = &rendered[1..rendered.len() - 1];
            let expected = match code {
                0x27 => "a\\' ".to_string(),
                0x5C => "a\\\\ ".to_string(),
                0x08 => "a\\b ".to_string(),
                0x09 => "a\\t ".to_string(),
                0x0A => "a\\n ".to_string(),
                0x0B => "a\\v ".to_string(),
                0x0C => "a\\f ".to_string(),
                0x0D => "a\\r ".to_string(),
                0x1B => "a\\e ".to_string(),
                0x7F => "a\\d ".to_string(),
                c if (0x20..=0x7E).contains(&c) || (0xA0..=0xFF).contains(&c) => {
                    format!("a{} ", char::from_u32(c).expect("a char"))
                }
                c => format!(
                    "a\\{}{}{} ",
                    char::from(b'0' + ((c >> 6) & 7) as u8),
                    char::from(b'0' + ((c >> 3) & 7) as u8),
                    char::from(b'0' + (c & 7) as u8)
                ),
            };
            assert_eq!(inner, expected, "escaping byte {code:#X}");
        }
    }

    #[test]
    fn proper_list_collects_or_rejects() {
        with_ctx(|ctx| {
            assert_eq!(proper_list(NIL).expect("nil is a proper list").len(), 0);
            let list = ctx
                .list(&[make_small(1), make_small(2)])
                .expect("fits");
            assert_eq!(proper_list(list).expect("proper").len(), 2);
            // An improper list and a non-list are both rejected.
            let improper = ctx.cons(make_small(1), make_small(2)).expect("fits");
            assert!(proper_list(improper).is_none());
            assert!(proper_list(make_small(3)).is_none());
        });
    }

    /// An **external** pid/ref is the typed [`unsupported_bif`] capability gap, not a
    /// malformed argument: the oracle renders its channel number through
    /// `atom_val(x->sysname)` — the *running* VM's node-atom index, runtime state the
    /// frozen external-identifier term does not carry (see [`render_chunk`]).
    ///
    /// A **fun** renders its `erlang:fun_to_list/1` text (the `~w`/`~p` leaf): an
    /// `EXPORT_EXT` fun as `fun Module:Function/Arity` and a `NEW_FUN_EXT` fun as
    /// `#Fun<Module.OldIndex.OldUniq>`. Only a `FunKind::Local` (`make_fun3`) fun
    /// stays the typed capability gap: its inlined `[header, uniq, index]` descriptor
    /// carries no module/name to render, so any text would be fabricated.
    #[test]
    fn external_identifiers_and_local_funs_are_typed_gaps() {
        with_ctx_seeded(&["lists", "reverse", "nonode@nohost", "foo"], |ctx| {
            // An external pid and an external ref are both the typed capability gap.
            let node = ctx.atom("hi").expect("seeded");
            let external_pid =
                beam_codec::term::make_external_pid(ctx.heap(), node, 1, 2, 3).expect("fits");
            assert!(matches!(
                format_term(ctx, external_pid, false),
                Err(beam_common::BeamError::Raise(ref r))
                    if r.reason.as_atom() == Some("unsupported_bif")
            ));
            let external_ref =
                beam_codec::term::make_external_ref(ctx.heap(), node, &[5], 0).expect("fits");
            assert!(matches!(
                format_term(ctx, external_ref, false),
                Err(beam_common::BeamError::Raise(ref r))
                    if r.reason.as_atom() == Some("unsupported_bif")
            ));

            // A **fun** now renders faithfully — the `~w`/`~p` leaf is
            // `erlang:fun_to_list/1`. An `External` (`EXPORT_EXT`) fun is
            // `fun Module:Function/Arity` ...
            let lists = atom_val(ctx.atom("lists").expect("seeded"));
            let reverse = atom_val(ctx.atom("reverse").expect("seeded"));
            let external = beam_codec::term::make_fun_external(ctx.heap(), 1, lists, reverse)
                .expect("the external fun fits");
            assert_eq!(show(ctx, external, false), "fun lists:reverse/1");
            assert_eq!(show(ctx, external, true), "fun lists:reverse/1");
            // ... and the C `print_atom_name` quoting applies to the module/function
            // atoms (`@` forces quotes), unlike `io_lib`'s `write_atom`.
            let node_atom = atom_val(ctx.atom("nonode@nohost").expect("seeded"));
            let foo = atom_val(ctx.atom("foo").expect("seeded"));
            let quoted = beam_codec::term::make_fun_external(ctx.heap(), 1, node_atom, foo)
                .expect("the external fun fits");
            assert_eq!(show(ctx, quoted, false), "fun 'nonode@nohost':foo/1");

            // An `Imported` (`NEW_FUN_EXT`) fun takes the printer's local-fun arm:
            // `#Fun<Module.OldIndex.OldUniq>`, from its descriptor.
            let creator = beam_term::fun::FunPid {
                node: node_atom,
                number: 1,
                serial: 0,
                creation: 0,
            };
            let imported = beam_codec::term::make_fun_imported(
                ctx.heap(),
                1,
                lists,
                &[0u8; 16],
                7,
                3,
                1234,
                creator,
                &[],
            )
            .expect("the imported fun fits");
            assert_eq!(show(ctx, imported, false), "#Fun<lists.3.1234>");
            assert_eq!(show(ctx, imported, true), "#Fun<lists.3.1234>");

            // A `Local` (`make_fun3`) fun stays the typed capability gap — the
            // documented structural gap: this `[header, uniq, index]` descriptor has
            // no module/name for `fun_to_list/1` to render.
            let words: Box<[Eterm]> = vec![
                Eterm::from_raw(beam_term::FUN_SUBTAG | (2usize << 8)),
                Eterm::from_raw(0), // uniq
                Eterm::from_raw(0), // index
            ]
            .into_boxed_slice();
            let fun = beam_term::make_boxed(words.as_ptr());
            assert!(matches!(
                format_term(ctx, fun, false),
                Err(beam_common::BeamError::Raise(ref r))
                    if r.reason.as_atom() == Some("unsupported_bif")
            ));

            // Sanity: the atom itself still renders.
            assert_eq!(show(ctx, node, false), "hi");
        });
    }

    /// `~w` of a 20_000-element flat list renders **byte-identically** on a 256 KiB
    /// stack.
    ///
    /// The list tail is walked once per element (`io_lib:write_tail/4`), which used to
    /// recurse once per element and overflow the native stack well below this length;
    /// `on_small_stack` pins that (see its docs). The oracle prints such a list, so the
    /// abort was a bug and the walk must be total.
    #[test]
    fn a_long_list_renders_without_per_element_stack_recursion() {
        let (rendered, limited) = on_small_stack(|| {
            with_ctx_capacity(1 << 20, |ctx| {
                let elems: Vec<Eterm> = (1..=LONG_CONTAINER_LEN).map(make_small).collect();
                let list = ctx.list(&elems).expect("the long list fits");
                (
                    format_term(ctx, list, false).expect("the long list renders"),
                    // A small depth stops the walk after two elements — the loop must
                    // still consume exactly one depth level per element.
                    format_term_depth(ctx, list, false, 3)
                        .expect("the depth-limited list renders"),
                )
            })
        });

        let mut expected = String::from("[");
        for i in 1..=LONG_CONTAINER_LEN {
            if i > 1 {
                expected.push(',');
            }
            expected.push_str(&i.to_string());
        }
        expected.push(']');
        assert_eq!(rendered, expected);
        assert_eq!(limited, "[1,2|...]");
    }

    /// `~w` of a 20_000-element tuple renders **byte-identically** on a 256 KiB stack
    /// (same per-element-walk regression as the list above, `io_lib:write_tuple/6`).
    #[test]
    fn a_long_tuple_renders_without_per_element_stack_recursion() {
        let (rendered, limited) = on_small_stack(|| {
            with_ctx_capacity(1 << 20, |ctx| {
                let elems: Vec<Eterm> = (1..=LONG_CONTAINER_LEN).map(make_small).collect();
                let tuple = ctx.tuple(&elems).expect("the long tuple fits");
                (
                    format_term(ctx, tuple, false).expect("the long tuple renders"),
                    format_term_depth(ctx, tuple, false, 3)
                        .expect("the depth-limited tuple renders"),
                )
            })
        });

        let mut expected = String::from("{");
        for i in 1..=LONG_CONTAINER_LEN {
            if i > 1 {
                expected.push(',');
            }
            expected.push_str(&i.to_string());
        }
        expected.push('}');
        assert_eq!(rendered, expected);
        assert_eq!(limited, "{1,2,...}");
    }

    /// `~w` of a 20_000-pair map renders on a 256 KiB stack (same per-pair-walk
    /// totality requirement as above, `io_lib:write_map_body/5`).
    ///
    /// Unlike the list/tuple walk, the old per-pair recursion happened to be compiled
    /// into a loop at `opt-level = 1` (no self-`call` in
    /// `render_map_body_chunk`'s disassembly), so this case did *not* abort before the
    /// rewrite; it pins the walk's totality — and would catch a re-introduced
    /// per-pair recursion the moment the compiler stops loop-ifying it.
    ///
    /// The pairs come out in the map's HAMT slot order, so the assertion is on the
    /// *structure* (the `,` separators, the ` => ` within each pair, and the exact key
    /// set) rather than on one expected string.
    #[test]
    fn a_large_map_renders_without_per_pair_stack_recursion() {
        // `beam_term::map::map_words_capacity` reserves `21 * pairs + 18` words for a
        // hash map; give the heap that plus slack.
        let capacity = 21 * LARGE_MAP_PAIRS as usize + 4096;
        let (rendered, limited) = on_small_stack(move || {
            with_ctx_capacity(capacity, |ctx| {
                let entries: Vec<(Eterm, Eterm)> = (1..=LARGE_MAP_PAIRS)
                    .map(|k| (make_small(k), make_small(k)))
                    .collect();
                let map = crate::access::make_map(ctx.heap(), &entries).expect("the map fits");
                (
                    format_term(ctx, map, false).expect("the large map renders"),
                    // The map's `depth == 1` arm must precede the pair bound, exactly as
                    // in the recursive form: one pair, then `,...`.
                    format_term_depth(ctx, map, false, 2).expect("the depth-limited map"),
                )
            })
        });

        assert!(
            rendered.starts_with("#{") && rendered.ends_with('}'),
            "a map renders `#{{…}}`: {rendered:.40}…"
        );
        let body = &rendered[2..rendered.len() - 1];
        // One pair per `,`-separated entry, and each entry is `Key => Value`.
        assert_eq!(body.matches(" => ").count(), LARGE_MAP_PAIRS as usize);
        assert_eq!(body.split(',').count(), LARGE_MAP_PAIRS as usize);
        let mut keys: Vec<i64> = body
            .split(',')
            .map(|entry| {
                let (key, value) = entry.split_once(" => ").expect("a `Key => Value` pair");
                assert_eq!(key, value, "`~w` of the map `K => K`");
                key.parse().expect("a small integer key")
            })
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, (1..=LARGE_MAP_PAIRS).collect::<Vec<_>>());

        assert_eq!(limited.matches(" => ").count(), 1, "{limited:.40}…");
        assert!(limited.starts_with("#{") && limited.ends_with(",...}"));
    }
}
