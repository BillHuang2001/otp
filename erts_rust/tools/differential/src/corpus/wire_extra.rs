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

//! Contract **A.5** — distribution wire / External Term Format, expanded: tag-level
//! byte goldens and encode/decode round trips.
//!
//! This group materially widens the A.5 ETF corpus beyond [`super::wire`] and
//! [`super::etf`]. Every case is a self-terminating `-eval` probe run under **both**
//! runtimes with identical argv, so it `agree`s only when the Rust runtime's ETF codec
//! produces the byte-for-byte same image (a `term_to_binary` golden) or the same value.
//! The covered tag families:
//!
//! * integers — `SMALL_INTEGER_EXT` (97) / `INTEGER_EXT` (98) boundaries,
//!   `SMALL_BIG_EXT` (110) / `LARGE_BIG_EXT` (111) (2^63, 2^64, 2^100, ±2^k, the 255/256
//!   digit little-endian threshold), via `term_to_binary` byte goldens and a stable
//!   leading-`tag`-byte predicate for the impractically long images;
//! * floats — `NEW_FLOAT_EXT` (70) bit-exact images (±0.0, 1.0, π, e, 0.001, 1.0e100)
//!   and `binary_to_term(term_to_binary(F)) =:= F` round trips;
//! * atoms — `SMALL_ATOM_UTF8_EXT` (119) / `ATOM_UTF8_EXT` (118) latin1 + non-latin1
//!   names, the empty atom, a 255-byte name, and the 118 → 119 re-encode direction;
//! * lists — `NIL_EXT` (106), `STRING_EXT` (107, the 255/256 byte boundary),
//!   `LIST_EXT` (108) incl. atom/binary/integer tails and long round trips;
//! * tuples — `SMALL_TUPLE_EXT` (104) / `LARGE_TUPLE_EXT` (105) at the 255/256 arity
//!   boundary, nested round trips;
//! * binaries — `BINARY_EXT` (109) at the 255/256-byte length boundary (always a 32-bit
//!   length — no small-binary tag exists) and `BIT_BINARY_EXT` (77) at every bit-count a
//!   non-byte-aligned value exercises (1..15) plus round trips, and the byte-aligned edge
//!   (8/16 bits collapse back to `BINARY_EXT`);
//! * maps — `MAP_EXT` (116): single-key byte goldens, and multi-key maps only ever
//!   through round-trip equality / `maps:size` / sorted key lists (iteration order is
//!   per-VM and never asserted);
//! * funs — `EXPORT_EXT` (113) for external funs (`fun M:F/A`) plus the decode round trip;
//! * framing — `VERSION_MAGIC` (131) as the first byte, the `compressed` (80) option
//!   (`[compressed]`, `{compressed,Level}`, and the no-op small-term path),
//!   `{minor_version,1|2}`, hand-built `VERSION_MAGIC` blobs and the first-byte/tag
//!   determinism predicates. The **compressed** framing is pinned (the leading `<<131,80,…>>`
//!   and the 32-bit uncompressed size agree) but the zlib *middle* is **never** byte-compared:
//!   the two runtimes' compressors emit different bytes there (the trailing Adler-32 still
//!   matches), so a raw compressed golden would drift — only the `binary_to_term` round trip
//!   is asserted;
//! * `erlang:binary_to_term/2` — the `[safe]` accept/reject surface, the `[used]`
//!   `{Term, BytesUsed}` shape, and the truncated / garbage / wrong-arity error shapes.
//! * decode-side hand-built blobs for tags an encode-only case never emits — the legacy
//!   `SMALL_ATOM_EXT` (115) and ASCII `FLOAT_EXT` (99), a `LARGE_TUPLE_EXT` (105), a
//!   zero-arity `SMALL_TUPLE_EXT` (104), a non-byte `LIST_EXT` (108), the zero-length
//!   `STRING_EXT` (107) / `BINARY_EXT` (109) / `MAP_EXT` (116), and the decode → re-encode
//!   byte identity of a `BINARY_EXT` and a 1-bit `BIT_BINARY_EXT` (77); plus option
//!   identities on top of the framing — `{minor_version,2}` equals the default image, a
//!   small term is left uncompressed (`[compressed]` byte-identical), and the
//!   `[compressed]` / `[compressed, {minor_version,2}]` / `[safe]` round trips over a
//!   bitstring, a bignum, a mixed compound and a long list.
//! * evaluation limits — the `-eval` **literal/parser** shapes that the system-ERTS oracle
//!   evaluates and the Rust `beam_rt` now evaluates too: a `≥ 2^63` integer literal, a
//!   multi-element improper list `[1,2|3]`, and an integer binary segment wider than 64 bits
//!   (`<<0:2048>>`). These were once **honest capability gaps** (the runtime raised its
//!   documented `beam_rt: unsupported: …` reason, exit `3`); `beam_rt`'s `-eval` literal/parser
//!   now grows each shape, so the cases **`agree`** and are regression-pinned. The `_unsupported`
//!   id suffixes are historical — kept only so the ids stay byte-stable.
//!
//! * the distribution **control-message payload tuples** (`DOP_*`) — the `{Tag, …}`
//!   shapes a distributed signal frames: `{1, …}` (LINK), `{2, node()}` (SEND),
//!   `{3, …, Reason}` (EXIT), `{4, …}` (UNLINK), `{5, node()}` (NODE_LINK),
//!   `{6, …, Name, Msg}` (REG_SEND), `{7, …}` (GROUP_LEADER), `{8, …}` (EXIT2),
//!   `{19, …, Ref, Opts}` (MONITOR_P), `{20, …, Ref}` (DEMONITOR_P) and
//!   `{21, …, Ref, Reason}` (MONITOR_P_EXIT). The all-atom shapes are byte goldens;
//!   every shape carrying a pid or a reference is pinned through a **shape/tag
//!   predicate** (`tuple_size/1`, `element/2`, `is_pid/1`, `is_reference/1`, the
//!   decode → re-encode byte identity), never a raw byte image, because the
//!   id/creation/serial fields are per-VM-start values;
//! * the `ERL_DIST_HEADER` framing — `VERSION_MAGIC` (131) + `PASS_THROUGH` (112) is a
//!   distribution *header* flag, not a term, so `binary_to_term/1` refuses it (`badarg`)
//!   in both runtimes; a hand-built header's 4-byte big-endian control-message length
//!   prefix is round-tripped and both payload terms are decoded from it;
//! * external-id images asserted as **shape predicates** — `NEW_PID_EXT` (88) and
//!   `V4_PORT_EXT` (120) of the runtime's *own* pid/port (image size, the 15-byte
//!   node-name prefix, the decode → re-encode byte identity) plus the
//!   `NEWER_REFERENCE_EXT` (90) local-node reference image; and the node-identity
//!   surface around them — `erlang:node/1` of a decoded external reference,
//!   `erlang:nodes/1`, `erlang:set_cookie/2`, and the handshake-shaped `{ok, …}` /
//!   `{error, …}` reply tuples.
//!
//! A live handshake and the on-wire `DOP_*`/`DFLAG_*` negotiation are **not**
//! expressible through a single CLI process (they need two connected nodes) — an
//! honest, documented gap, not a fabricated case (see `super::wire`); only the
//! *payload shapes* those messages carry are pinned here, as ordinary ETF terms.

use super::{Case, CaseKind, Contract, Golden, Probe};

/// A default-probe (`term_to_binary`) case: the byte-exact canonical ETF image.
fn t(id: &'static str, description: &'static str, expr: &'static str, stdout: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Distribution],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::TermToBinary,
        },
        golden: Golden {
            stdout: Some(stdout),
            exit_code: 0,
        },
    }
}

/// A `~w`-probe case: the textual rendering of a stable predicate or shape.
fn v(id: &'static str, description: &'static str, expr: &'static str, stdout: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Distribution],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(stdout),
            exit_code: 0,
        },
    }
}

/// A `~w`-probe case whose expression is expected to raise `error:badarg`.
fn verr(id: &'static str, description: &'static str, expr: &'static str) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::Distribution],
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some("exception error badarg"),
            exit_code: 1,
        },
    }
}

const TRUE: &str = "ok <<131,119,4,116,114,117,101>>";

/// The expanded A.5 wire/ETF cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- integers: SMALL_INTEGER_EXT (97) / INTEGER_EXT (98) boundaries -------------
        t("wire_extra/ie_small_int_1", "the integer 1 encodes as SMALL_INTEGER_EXT (97)", "1", "ok <<131,97,1>>"),
        t("wire_extra/ie_small_int_127", "127 is the largest 7-bit value, still SMALL_INTEGER_EXT", "127", "ok <<131,97,127>>"),
        t("wire_extra/ie_small_int_128", "128 crosses the 7-bit boundary but stays SMALL_INTEGER_EXT", "128", "ok <<131,97,128>>"),
        t("wire_extra/ie_small_int_254", "254 is one below the SMALL_INTEGER_EXT ceiling", "254", "ok <<131,97,254>>"),
        v("wire_extra/ie_tag_small_integer", "the leading tag byte of 200 is SMALL_INTEGER_EXT (97)", "binary:at(term_to_binary(200),1)", "ok 97"),
        v("wire_extra/ie_tag_small_integer_255", "the leading tag byte of 255 is still SMALL_INTEGER_EXT (97)", "binary:at(term_to_binary(255),1)", "ok 97"),
        v("wire_extra/ie_tag_small_integer_zero", "the leading tag byte of 0 is SMALL_INTEGER_EXT (97)", "binary:at(term_to_binary(0),1)", "ok 97"),
        v("wire_extra/ie_tag_integer", "the leading tag byte of 1000 is INTEGER_EXT (98)", "binary:at(term_to_binary(1000),1)", "ok 98"),
        t("wire_extra/ie_int_ext_300", "300 overflows one byte and encodes as INTEGER_EXT (98)", "300", "ok <<131,98,0,0,1,44>>"),
        t("wire_extra/ie_int_ext_neg1", "-1 encodes as a sign-extended INTEGER_EXT (98)", "-1", "ok <<131,98,255,255,255,255>>"),
        t("wire_extra/ie_int_ext_neg256", "-256 encodes as INTEGER_EXT (98)", "-256", "ok <<131,98,255,255,255,0>>"),
        t("wire_extra/ie_int_ext_int32_max", "2^31-1 is the largest INTEGER_EXT (98)", "2147483647", "ok <<131,98,127,255,255,255>>"),
        t("wire_extra/ie_int_ext_int32_min", "-2^31 is the smallest INTEGER_EXT (98)", "-2147483648", "ok <<131,98,128,0,0,0>>"),
        // -- big integers: SMALL_BIG_EXT (110) / LARGE_BIG_EXT (111) -------------------
        t("wire_extra/ie_small_big_int32_overflow", "2^31 overflows INTEGER_EXT into SMALL_BIG_EXT (110)", "2147483648", "ok <<131,110,4,0,0,0,0,128>>"),
        t("wire_extra/ie_small_big_neg_int32_overflow", "-(2^31+1) is a negative SMALL_BIG_EXT (110)", "-2147483649", "ok <<131,110,4,1,1,0,0,128>>"),
        t("wire_extra/ie_small_big_2_pow_63_minus1", "2^63-1 is the largest 8-digit SMALL_BIG_EXT (110)", "(1 bsl 63) - 1", "ok <<131,110,8,0,255,255,255,255,255,255,255,127>>"),
        t("wire_extra/ie_small_big_2_pow_63", "2^63 is an 8-digit SMALL_BIG_EXT (110)", "1 bsl 63", "ok <<131,110,8,0,0,0,0,0,0,0,0,128>>"),
        t("wire_extra/ie_small_big_neg_2_pow_63", "-2^63 is an 8-digit negative SMALL_BIG_EXT (110)", "-(1 bsl 63)", "ok <<131,110,8,1,0,0,0,0,0,0,0,128>>"),
        t("wire_extra/ie_small_big_2_pow_64", "2^64 is a 9-digit SMALL_BIG_EXT (110)", "1 bsl 64", "ok <<131,110,9,0,0,0,0,0,0,0,0,0,1>>"),
        t("wire_extra/ie_small_big_neg_2_pow_64", "-2^64 is a 9-digit negative SMALL_BIG_EXT (110)", "-(1 bsl 64)", "ok <<131,110,9,1,0,0,0,0,0,0,0,0,1>>"),
        t("wire_extra/ie_small_big_2_pow_64_plus1", "2^64+1 exercises the low digit of a 9-digit big", "(1 bsl 64) + 1", "ok <<131,110,9,0,1,0,0,0,0,0,0,0,1>>"),
        t("wire_extra/ie_small_big_2_pow_100", "2^100 is a 13-digit little-endian SMALL_BIG_EXT (110)", "1 bsl 100", "ok <<131,110,13,0,0,0,0,0,0,0,0,0,0,0,0,0,16>>"),
        v("wire_extra/ie_tag_small_big", "a 100-bit integer leads with SMALL_BIG_EXT (110)", "binary:at(term_to_binary(1 bsl 100),1)", "ok 110"),
        v("wire_extra/ie_tag_small_big_2032", "2^2032 (255 digits) is the largest SMALL_BIG_EXT (110)", "binary:at(term_to_binary(1 bsl 2032),1)", "ok 110"),
        v("wire_extra/ie_tag_large_big_2040", "2^2040 (256 digits) crosses into LARGE_BIG_EXT (111)", "binary:at(term_to_binary(1 bsl 2040),1)", "ok 111"),
        v("wire_extra/ie_tag_large_big_2048", "2^2048 (257 digits) is a LARGE_BIG_EXT (111)", "binary:at(term_to_binary(1 bsl 2048),1)", "ok 111"),
        t("wire_extra/ie_roundtrip_neg_bignum", "binary_to_term/1 inverts term_to_binary/1 for a negative bignum", "binary_to_term(term_to_binary(-(1 bsl 200))) =:= -(1 bsl 200)", TRUE),
        // -- floats: NEW_FLOAT_EXT (70) ------------------------------------------------
        t("wire_extra/fl_zero", "0.0 encodes as an all-zero NEW_FLOAT_EXT (70)", "0.0", "ok <<131,70,0,0,0,0,0,0,0,0>>"),
        t("wire_extra/fl_neg_zero", "-0.0 keeps the sign bit in NEW_FLOAT_EXT (70)", "-0.0", "ok <<131,70,128,0,0,0,0,0,0,0>>"),
        t("wire_extra/fl_one", "1.0 is IEEE-754 0x3FF0000000000000", "1.0", "ok <<131,70,63,240,0,0,0,0,0,0>>"),
        t("wire_extra/fl_pi", "pi's double image is bit-exact", "3.141592653589793", "ok <<131,70,64,9,33,251,84,68,45,24>>"),
        t("wire_extra/fl_neg_pi", "-pi's double image is bit-exact", "-3.141592653589793", "ok <<131,70,192,9,33,251,84,68,45,24>>"),
        t("wire_extra/fl_large_1e100", "1.0e100's double image is bit-exact", "1.0e100", "ok <<131,70,84,178,73,173,37,148,195,125>>"),
        t("wire_extra/fl_milli", "0.001's double image is bit-exact", "0.001", "ok <<131,70,63,80,98,77,210,241,169,252>>"),
        t("wire_extra/fl_e", "e's double image is bit-exact", "2.718281828459045", "ok <<131,70,64,5,191,10,139,20,87,105>>"),
        t("wire_extra/fl_roundtrip_pi", "binary_to_term/1 inverts term_to_binary/1 for pi", "binary_to_term(term_to_binary(3.141592653589793)) =:= 3.141592653589793", TRUE),
        t("wire_extra/fl_roundtrip_neg_half", "binary_to_term/1 inverts term_to_binary/1 for -0.5", "binary_to_term(term_to_binary(-0.5)) =:= -0.5", TRUE),
        t("wire_extra/fl_roundtrip_many", "a spread of doubles all round-trip exactly", "lists:all(fun(F) -> binary_to_term(term_to_binary(F)) =:= F end, [0.0,1.0,-1.5,3.141592653589793,1.0e100,1.0e-100,123456.789])", TRUE),
        t("wire_extra/fl_decode_neg_zero_blob", "a fixed -0.0 NEW_FLOAT_EXT (70) blob re-encodes to -0.0", "term_to_binary(binary_to_term(<<131,70,128,0,0,0,0,0,0,0>>)) =:= term_to_binary(-0.0)", TRUE),
        // -- atoms: SMALL_ATOM_UTF8_EXT (119) / ATOM_UTF8_EXT (118) ---------------------
        t("wire_extra/at_empty", "the empty atom encodes as a zero-length SMALL_ATOM_UTF8_EXT (119)", "''", "ok <<131,119,0>>"),
        t("wire_extra/at_underscore", "the atom '_' encodes as SMALL_ATOM_UTF8_EXT (119)", "'_'", "ok <<131,119,1,95>>"),
        t("wire_extra/at_long_latin1", "a plain ASCII atom encodes as SMALL_ATOM_UTF8_EXT (119)", "'naive'", "ok <<131,119,5,110,97,105,118,101>>"),
        t("wire_extra/at_latin1_upsilon", "a Latin-1 atom encodes its UTF-8 bytes under SMALL_ATOM_UTF8_EXT (119)", "list_to_atom([252,98,101,114])", "ok <<131,119,5,195,188,98,101,114>>"),
        t("wire_extra/at_unicode_pi", "a 2-byte UTF-8 Greek atom encodes as SMALL_ATOM_UTF8_EXT (119)", "list_to_atom([960])", "ok <<131,119,2,207,128>>"),
        t("wire_extra/at_unicode_emoji", "a 4-byte UTF-8 astral atom encodes as SMALL_ATOM_UTF8_EXT (119)", "list_to_atom([128169])", "ok <<131,119,4,240,159,146,169>>"),
        t("wire_extra/at_utf8_two_byte", "a Cyrillic atom encodes its two UTF-8 bytes", "list_to_atom([1040])", "ok <<131,119,2,208,144>>"),
        t("wire_extra/at_utf8_bmp_greek_sigma", "a BMP atom encodes its three UTF-8 bytes", "list_to_atom([931])", "ok <<131,119,2,206,163>>"),
        v("wire_extra/at_255_len_tag", "a 255-byte atom name still uses SMALL_ATOM_UTF8_EXT (119)", "binary:at(term_to_binary(list_to_atom(lists:duplicate(255,$a))),1)", "ok 119"),
        v("wire_extra/at_255_len_size", "the 255-byte atom image is the one-byte tag+length plus 255 name bytes", "byte_size(term_to_binary(list_to_atom(lists:duplicate(255,$a))))", "ok 258"),
        t("wire_extra/at_255_roundtrip", "the longest (255-byte) atom name round-trips", "binary_to_term(term_to_binary(list_to_atom(lists:duplicate(255,$a)))) =:= list_to_atom(lists:duplicate(255,$a))", TRUE),
        t("wire_extra/at_decode_118_utf8", "a fixed ATOM_UTF8_EXT (118) blob with a 2-byte UTF-8 name decodes to the code point", "binary_to_term(<<131,118,0,2,206,177>>) =:= list_to_atom([945])", TRUE),
        v("wire_extra/at_decode_118_reencodes_as_119", "an atom decoded from the 16-bit-length ATOM_UTF8_EXT (118) re-encodes as SMALL_ATOM_UTF8_EXT (119)", "term_to_binary(binary_to_term(<<131,118,0,3,102,111,111>>))", "ok <<131,119,3,102,111,111>>"),
        t("wire_extra/at_decode_119_emoji_eq", "a fixed SMALL_ATOM_UTF8_EXT (119) astral blob decodes to the code point", "binary_to_term(<<131,119,4,240,159,146,169>>) =:= list_to_atom([128169])", TRUE),
        // -- tuples: SMALL_TUPLE_EXT (104) / LARGE_TUPLE_EXT (105) ---------------------
        t("wire_extra/tu_arity1", "a 1-tuple encodes as SMALL_TUPLE_EXT (104)", "{a}", "ok <<131,104,1,119,1,97>>"),
        t("wire_extra/tu_arity2_int", "a 2-tuple of small ints encodes as SMALL_TUPLE_EXT (104)", "{1,2}", "ok <<131,104,2,97,1,97,2>>"),
        t("wire_extra/tu_arity3_roundtrip", "a mixed 3-tuple round-trips", "binary_to_term(term_to_binary({a,1,<<2>>})) =:= {a,1,<<2>>}", TRUE),
        t("wire_extra/tu_nested", "a nested tuple round-trips", "binary_to_term(term_to_binary({ok,{1,[a,b]},<<3>>})) =:= {ok,{1,[a,b]},<<3>>}", TRUE),
        v("wire_extra/tu_arity255_tag", "a 255-element tuple is the largest SMALL_TUPLE_EXT (104)", "binary:at(term_to_binary(list_to_tuple(lists:duplicate(255,x))),1)", "ok 104"),
        v("wire_extra/tu_arity255_size", "a 255-element tuple image is 3 + 255*3 bytes", "byte_size(term_to_binary(list_to_tuple(lists:duplicate(255,x))))", "ok 768"),
        v("wire_extra/tu_arity256_tag", "a 256-element tuple crosses into LARGE_TUPLE_EXT (105)", "binary:at(term_to_binary(list_to_tuple(lists:duplicate(256,x))),1)", "ok 105"),
        t("wire_extra/tu_arity255_roundtrip", "a 255-element tuple round-trips", "binary_to_term(term_to_binary(list_to_tuple(lists:duplicate(255,x)))) =:= list_to_tuple(lists:duplicate(255,x))", TRUE),
        t("wire_extra/tu_arity256_roundtrip", "a 256-element tuple round-trips", "binary_to_term(term_to_binary(list_to_tuple(lists:duplicate(256,x)))) =:= list_to_tuple(lists:duplicate(256,x))", TRUE),
        // -- lists: NIL_EXT (106) / STRING_EXT (107) / LIST_EXT (108) ------------------
        t("wire_extra/li_improper_atom_tail", "an improper list with an atom tail encodes as LIST_EXT (108) with a raw tail", "[a|b]", "ok <<131,108,0,0,0,1,119,1,97,119,1,98>>"),
        t("wire_extra/li_improper_binary_tail", "an improper list with a binary tail encodes as LIST_EXT (108)", "[1|<<2>>]", "ok <<131,108,0,0,0,1,97,1,109,0,0,0,1,2>>"),
        t("wire_extra/li_improper_nested", "an improper list with an integer tail encodes as LIST_EXT (108)", "[1|[2|3]]", "ok <<131,108,0,0,0,2,97,1,97,2,97,3>>"),
        t("wire_extra/li_cons_improper_long_tail", "a three-cons improper list with an atom tail encodes as LIST_EXT (108)", "[1|[2|[3|x]]]", "ok <<131,108,0,0,0,3,97,1,97,2,97,3,119,1,120>>"),
        t("wire_extra/li_single_nonbyte", "a one-element list with a non-byte element uses LIST_EXT (108)", "[300]", "ok <<131,108,0,0,0,1,98,0,0,1,44,106>>"),
        t("wire_extra/li_two_nonbyte", "a two-element list with non-byte elements uses LIST_EXT (108)", "[300,400]", "ok <<131,108,0,0,0,2,98,0,0,1,44,98,0,0,1,144,106>>"),
        v("wire_extra/li_string_255_tag", "a 255-byte list is a STRING_EXT (107)", "binary:at(term_to_binary(lists:duplicate(255,$a)),1)", "ok 107"),
        v("wire_extra/li_string_256_tag", "a 256-byte list is still a STRING_EXT (107) (16-bit length)", "binary:at(term_to_binary(lists:duplicate(256,$a)),1)", "ok 107"),
        v("wire_extra/li_list_255_tag", "a 255-element list with a non-byte element is a LIST_EXT (108), not STRING_EXT", "binary:at(term_to_binary(lists:duplicate(255,300)),1)", "ok 108"),
        v("wire_extra/li_list_256_tag", "a 256-element list with a non-byte element is a LIST_EXT (108), not STRING_EXT", "binary:at(term_to_binary(lists:duplicate(256,300)),1)", "ok 108"),
        v("wire_extra/li_empty_nil_size", "the empty list is the two-byte NIL_EXT (106) image", "byte_size(term_to_binary([]))", "ok 2"),
        t("wire_extra/li_string_long_roundtrip", "a 1000-byte string list round-trips", "binary_to_term(term_to_binary(lists:duplicate(1000,$a))) =:= lists:duplicate(1000,$a)", TRUE),
        t("wire_extra/li_long_list_roundtrip", "a 1000-element integer list round-trips", "binary_to_term(term_to_binary(lists:seq(1,1000))) =:= lists:seq(1,1000)", TRUE),
        t("wire_extra/li_improper_roundtrip", "a 2-cons improper list round-trips", "binary_to_term(term_to_binary([a|[b|c]])) =:= [a|[b|c]]", TRUE),
        t("wire_extra/li_nested_int_lists", "a list of lists round-trips", "binary_to_term(term_to_binary([[1,2],[3,4]])) =:= [[1,2],[3,4]]", TRUE),
        // -- binaries: BINARY_EXT (109) / BIT_BINARY_EXT (77) --------------------------
        t("wire_extra/bi_one_byte", "a one-byte binary encodes as BINARY_EXT (109)", "<<0>>", "ok <<131,109,0,0,0,1,0>>"),
        t("wire_extra/bi_two_bytes", "a two-byte binary encodes as BINARY_EXT (109)", "<<1,2>>", "ok <<131,109,0,0,0,2,1,2>>"),
        v("wire_extra/bi_256_size", "a 256-byte binary image is the six-byte header plus 256 bytes", "byte_size(term_to_binary(binary:copy(<<0>>,256)))", "ok 262"),
        v("wire_extra/bi_255_size", "a 255-byte binary image is the six-byte header plus 255 bytes", "byte_size(term_to_binary(binary:copy(<<0>>,255)))", "ok 261"),
        v("wire_extra/bi_255_tag", "a 255-byte binary still leads with BINARY_EXT (109)", "binary:at(term_to_binary(binary:copy(<<0>>,255)),1)", "ok 109"),
        t("wire_extra/bi_large_roundtrip", "a 1000-byte binary round-trips", "binary_to_term(term_to_binary(binary:copy(<<0>>,1000))) =:= binary:copy(<<0>>,1000)", TRUE),
        t("wire_extra/bi_roundtrip_bytes", "a four-byte binary round-trips", "binary_to_term(term_to_binary(<<1,2,3,255>>)) =:= <<1,2,3,255>>", TRUE),
        t("wire_extra/bi_roundtrip_empty", "the empty binary round-trips", "binary_to_term(term_to_binary(<<>>)) =:= <<>>", TRUE),
        v("wire_extra/bit_1_bit_tag", "a 1-bit bitstring leads with BIT_BINARY_EXT (77)", "binary:at(term_to_binary(<<1:1>>),1)", "ok 77"),
        t("wire_extra/bit_2_bits", "a 2-bit bitstring encodes its bit count and padded last byte", "<<2#11:2>>", "ok <<131,77,0,0,0,1,2,192>>"),
        t("wire_extra/bit_3_bits", "a 3-bit bitstring encodes a bit count of 3", "<<2#101:3>>", "ok <<131,77,0,0,0,1,3,160>>"),
        t("wire_extra/bit_4_bits", "a 4-bit bitstring encodes a bit count of 4", "<<2#1010:4>>", "ok <<131,77,0,0,0,1,4,160>>"),
        t("wire_extra/bit_5_bits", "a 5-bit bitstring encodes a bit count of 5", "<<2#10101:5>>", "ok <<131,77,0,0,0,1,5,168>>"),
        t("wire_extra/bit_7_bits", "a 7-bit bitstring encodes a bit count of 7", "<<1:7>>", "ok <<131,77,0,0,0,1,7,2>>"),
        t("wire_extra/bit_9_bits", "a 9-bit bitstring spans two bytes with a bit count of 1", "<<3:9>>", "ok <<131,77,0,0,0,2,1,1,128>>"),
        t("wire_extra/bit_12_bits", "a 12-bit bitstring spans two bytes with a bit count of 4", "<<5:12>>", "ok <<131,77,0,0,0,2,4,0,80>>"),
        t("wire_extra/bit_1_roundtrip", "a 1-bit bitstring round-trips", "binary_to_term(term_to_binary(<<1:1>>)) =:= <<1:1>>", TRUE),
        t("wire_extra/bit_3_roundtrip", "a 3-bit bitstring round-trips", "binary_to_term(term_to_binary(<<5:3>>)) =:= <<5:3>>", TRUE),
        v("wire_extra/bit_8_bits_is_binary_tag", "an 8-bit value is byte-aligned and encodes as BINARY_EXT (109), not BIT_BINARY_EXT", "binary:at(term_to_binary(<<1:8>>),1)", "ok 109"),
        v("wire_extra/bit_8_bits_is_binary_size", "an 8-bit value's image is the six-byte header plus one byte", "byte_size(term_to_binary(<<1:8>>))", "ok 7"),
        v("wire_extra/bit_16_bits_is_binary_tag", "a 16-bit value is byte-aligned and encodes as BINARY_EXT (109)", "binary:at(term_to_binary(<<1:16>>),1)", "ok 109"),
        t("wire_extra/bit_13_roundtrip", "a 13-bit bitstring round-trips", "binary_to_term(term_to_binary(<<5:13>>)) =:= <<5:13>>", TRUE),
        t("wire_extra/bit_15_roundtrip", "a 15-bit bitstring round-trips", "binary_to_term(term_to_binary(<<7:15>>)) =:= <<7:15>>", TRUE),
        // -- maps: MAP_EXT (116) ------------------------------------------------------
        t("wire_extra/ma_single_int_key", "a single integer-keyed map encodes as MAP_EXT (116)", "#{1 => 2}", "ok <<131,116,0,0,0,1,97,1,97,2>>"),
        t("wire_extra/ma_single_atom_stringval", "a single atom-keyed map with a string value encodes as MAP_EXT (116)", "#{k => \"v\"}", "ok <<131,116,0,0,0,1,119,1,107,107,0,1,118>>"),
        t("wire_extra/ma_single_binary_key", "a single binary-keyed map encodes as MAP_EXT (116)", "#{<<1>> => <<2>>}", "ok <<131,116,0,0,0,1,109,0,0,0,1,1,109,0,0,0,1,2>>"),
        t("wire_extra/ma_two_key_roundtrip_eq", "a two-key map round-trips", "binary_to_term(term_to_binary(#{a=>1,b=>2})) =:= #{a=>1,b=>2}", TRUE),
        v("wire_extra/ma_three_key_size", "a three-key map keeps its size across the round trip", "maps:size(binary_to_term(term_to_binary(#{a=>1,b=>2,c=>3})))", "ok 3"),
        v("wire_extra/ma_32_keys_size", "a 32-key map (the flatmap ceiling) keeps its size across the round trip", "maps:size(binary_to_term(term_to_binary(maps:from_list([{N,N}||N<-lists:seq(1,32)]))))", "ok 32"),
        t("wire_extra/ma_32_keys_roundtrip", "a 32-key map round-trips", "binary_to_term(term_to_binary(maps:from_list([{N,N}||N<-lists:seq(1,32)]))) =:= maps:from_list([{N,N}||N<-lists:seq(1,32)])", TRUE),
        v("wire_extra/ma_sorted_keys_two", "a three-key map's keys come back sorted", "lists:sort(maps:keys(binary_to_term(term_to_binary(#{3=>3,1=>1,2=>2}))))", "ok [1,2,3]"),
        t("wire_extra/ma_nested_roundtrip", "a nested map/list/binary structure round-trips", "binary_to_term(term_to_binary(#{a => #{b => [1,2,#{c => <<3>>}]}})) =:= #{a => #{b => [1,2,#{c => <<3>>}]}}", TRUE),
        t("wire_extra/ma_empty_roundtrip", "the empty map round-trips", "binary_to_term(term_to_binary(#{})) =:= #{}", TRUE),
        // -- funs: EXPORT_EXT (113) ----------------------------------------------------
        t("wire_extra/fun_export_map_encode", "the external fun lists:map/2 encodes as EXPORT_EXT (113)", "term_to_binary(fun lists:map/2)", "ok <<131,109,0,0,0,16,131,113,119,5,108,105,115,116,115,119,3,109,97,112,97,2>>"),
        t("wire_extra/fun_export_length_encode", "the external fun erlang:length/1 encodes as EXPORT_EXT (113)", "term_to_binary(fun erlang:length/1)", "ok <<131,109,0,0,0,20,131,113,119,6,101,114,108,97,110,103,119,6,108,101,110,103,116,104,97,1>>"),
        t("wire_extra/fun_export_roundtrip_eq", "an external fun round-trips", "binary_to_term(term_to_binary(fun lists:map/2)) =:= fun lists:map/2", TRUE),
        t("wire_extra/fun_export_is_function2", "a decoded external fun is a function of arity 2", "is_function(binary_to_term(term_to_binary(fun lists:map/2)), 2)", TRUE),
        t("wire_extra/fun_export_is_function1", "a decoded external fun is a function of arity 1", "is_function(binary_to_term(term_to_binary(fun erlang:length/1)), 1)", TRUE),
        v("wire_extra/fun_export_decode_apply", "a decoded external fun is callable", "binary_to_term(term_to_binary(fun lists:reverse/1))([1,2,3])", "ok [3,2,1]"),
        // -- compression: COMPRESSED (80) ---------------------------------------------
        v("wire_extra/co_large_tag", "a large compressible term leads with COMPRESSED (80)", "binary:at(term_to_binary(lists:duplicate(1000,$a), [compressed]),1)", "ok 80"),
        v("wire_extra/co_first_byte", "a compressed image still starts with VERSION_MAGIC (131)", "binary:first(term_to_binary(lists:duplicate(1000,$a),[compressed]))", "ok 131"),
        v("wire_extra/co_prefix_two_value", "a compressed image's first two bytes are 131 then 80", "binary:part(term_to_binary(lists:duplicate(1000,$a),[compressed]),0,2)", "ok <<131,80>>"),
        t("wire_extra/co_size_lt_uncompressed", "compressing a repetitive term shrinks it", "byte_size(term_to_binary(lists:duplicate(1000,$a),[compressed])) < byte_size(term_to_binary(lists:duplicate(1000,$a)))", TRUE),
        v("wire_extra/co_small_not_compressed_tag", "a term too small to gain is left uncompressed (SMALL_INTEGER_EXT 97)", "binary:at(term_to_binary(42, [compressed]),1)", "ok 97"),
        t("wire_extra/co_large_roundtrip_eq", "a compressed large term round-trips", "binary_to_term(term_to_binary(lists:duplicate(1000,$a), [compressed])) =:= lists:duplicate(1000,$a)", TRUE),
        t("wire_extra/co_level9_roundtrip", "the {compressed,9} option round-trips", "binary_to_term(term_to_binary(lists:seq(1,500), [{compressed,9}])) =:= lists:seq(1,500)", TRUE),
        t("wire_extra/co_level1_roundtrip", "the {compressed,1} option round-trips", "binary_to_term(term_to_binary(lists:seq(1,500), [{compressed,1}])) =:= lists:seq(1,500)", TRUE),
        t("wire_extra/co_roundtrip_nested", "a compressed nested map/list round-trips", "binary_to_term(term_to_binary(#{a => lists:seq(1,100)}, [compressed])) =:= #{a => lists:seq(1,100)}", TRUE),
        // -- minor_version ------------------------------------------------------------
        v("wire_extra/mv1_nonlatin_atom_tag", "a non-latin1 atom is still SMALL_ATOM_UTF8_EXT (119) under {minor_version,1}", "binary:at(term_to_binary(list_to_atom([945]), [{minor_version,1}]),1)", "ok 119"),
        v("wire_extra/mv2_nonlatin_atom_tag", "a non-latin1 atom is still SMALL_ATOM_UTF8_EXT (119) under {minor_version,2}", "binary:at(term_to_binary(list_to_atom([945]), [{minor_version,2}]),1)", "ok 119"),
        v("wire_extra/mv1_float_tag", "a float is NEW_FLOAT_EXT (70) under {minor_version,1}", "binary:at(term_to_binary(1.5, [{minor_version,1}]),1)", "ok 70"),
        t("wire_extra/mv2_roundtrip", "a {minor_version,2} image round-trips", "binary_to_term(term_to_binary({a,1}, [{minor_version,2}])) =:= {a,1}", TRUE),
        t("wire_extra/mv1_bit_binary_eq", "{minor_version,1} does not change a bitstring image", "term_to_binary(<<1:1>>, [{minor_version,1}]) =:= term_to_binary(<<1:1>>)", TRUE),
        // -- binary_to_term/2 options -------------------------------------------------
        v("wire_extra/opt_safe_accept", "[safe] accepts an image that introduces no new atoms", "binary_to_term(term_to_binary({ok,1}), [safe])", "ok {ok,1}"),
        v("wire_extra/opt_safe_accept_compound", "[safe] accepts a compound term of existing atoms", "binary_to_term(term_to_binary([a,1,<<2>>]), [safe])", "ok [a,1,<<2>>]"),
        verr("wire_extra/opt_safe_reject_new_atom", "[safe] rejects an image naming a fresh atom", "binary_to_term(<<131,119,9,122,122,110,101,119,97,116,111,109>>, [safe])"),
        v("wire_extra/opt_used_shape", "[used] returns {Term, BytesUsed}", "binary_to_term(<<131,97,7,9,9>>, [used])", "ok {7,3}"),
        v("wire_extra/opt_used_empty_tail", "[used] on a 3-byte image returns the term and the 3 consumed bytes", "binary_to_term(term_to_binary(42), [used])", "ok {42,3}"),
        v("wire_extra/opt_used_trailing_bytes", "[used] reports the consumed byte count and leaves the rest", "binary_to_term(<<(term_to_binary({ok,1}))/binary, 255>>, [used])", "ok {{ok,1},9}"),
        v("wire_extra/opt_safe_and_used", "[safe,used] combine", "binary_to_term(<<131,97,7,255>>, [safe,used])", "ok {7,3}"),
        verr("wire_extra/opt_bad_option", "an unknown binary_to_term/2 option is badarg", "binary_to_term(term_to_binary(1), [bogus])"),
        // -- error shapes -------------------------------------------------------------
        verr("wire_extra/err_truncated_tuple", "a truncated tuple image is badarg", "binary_to_term(<<131,104,2,119,2,111,107>>)"),
        verr("wire_extra/err_truncated_big", "a truncated SMALL_BIG_EXT image is badarg", "binary_to_term(<<131,110,3,0,0>>)"),
        verr("wire_extra/err_truncated_small_atom", "a truncated SMALL_ATOM_UTF8_EXT image is badarg", "binary_to_term(<<131,119,5,97,98>>)"),
        verr("wire_extra/err_truncated_binary", "a truncated BINARY_EXT image is badarg", "binary_to_term(<<131,109,0,0,0,5,1,2>>)"),
        verr("wire_extra/err_garbage", "a non-ETF byte string is badarg", "binary_to_term(<<1,2,3>>)"),
        verr("wire_extra/err_empty", "an empty input is badarg", "binary_to_term(<<>>)"),
        verr("wire_extra/err_not_binary", "a non-binary input to binary_to_term/1 is badarg", "binary_to_term(42)"),
        verr("wire_extra/err_unknown_tag", "an unknown ETF tag is badarg", "binary_to_term(<<131,255>>)"),
        verr("wire_extra/err_missing_version", "a blob without VERSION_MAGIC is badarg", "binary_to_term(<<104,2,97,1,97,2>>)"),
        v("wire_extra/err_trailing_ignored", "trailing bytes after a complete term are ignored", "binary_to_term(<<131,97,42,1,2,3>>)", "ok 42"),
        // -- version framing ----------------------------------------------------------
        v("wire_extra/vf_first_byte_tuple", "a tuple image starts with VERSION_MAGIC (131)", "binary:first(term_to_binary({1,2}))", "ok 131"),
        v("wire_extra/vf_first_byte_binary", "a binary image starts with VERSION_MAGIC (131)", "binary:first(term_to_binary(<<1>>))", "ok 131"),
        v("wire_extra/vf_first_byte_float", "a float image starts with VERSION_MAGIC (131)", "binary:first(term_to_binary(1.0))", "ok 131"),
        v("wire_extra/vf_first_byte_empty_list", "the empty-list image starts with VERSION_MAGIC (131)", "binary:first(term_to_binary([]))", "ok 131"),
        v("wire_extra/vf_first_byte_of_encoded_binary", "re-encoding an ETF image starts with VERSION_MAGIC (131)", "binary:first(term_to_binary(term_to_binary(foo)))", "ok 131"),
        v("wire_extra/vf_second_byte_small_tuple", "the tag after VERSION_MAGIC of a 1-tuple is SMALL_TUPLE_EXT (104)", "binary:at(term_to_binary({1}),1)", "ok 104"),
        v("wire_extra/vf_len_tuple", "the {a,1,<<2>>} image is 14 bytes", "byte_size(term_to_binary({a,1,<<2>>}))", "ok 14"),
        t("wire_extra/vf_determinism_tuple", "term_to_binary/1 is deterministic for a tuple", "term_to_binary({a,1,<<2>>}) =:= term_to_binary({a,1,<<2>>})", TRUE),
        t("wire_extra/vf_determinism_map", "term_to_binary/1 is deterministic for a map", "term_to_binary(#{a=>1}) =:= term_to_binary(#{a=>1})", TRUE),
        v("wire_extra/vf_decode_hand_built_nil", "a hand-built VERSION_MAGIC + NIL_EXT blob decodes to the empty list", "binary_to_term(<<131,106>>)", "ok []"),
        v("wire_extra/vf_decode_hand_built_tuple", "a hand-built VERSION_MAGIC + SMALL_TUPLE_EXT blob decodes to a tuple", "binary_to_term(<<131,104,2,97,1,97,2>>)", "ok {1,2}"),
        v("wire_extra/vf_decode_hand_built_atom", "a hand-built VERSION_MAGIC + SMALL_ATOM_UTF8_EXT blob decodes to an atom", "binary_to_term(<<131,119,4,116,114,117,101>>)", "ok true"),
        // -- round trips across every family ------------------------------------------
        t("wire_extra/rt_negative_ints", "a list of negative integers (incl. the 64-bit floor) round-trips", "binary_to_term(term_to_binary([-1,-300,-(1 bsl 63)])) =:= [-1,-300,-(1 bsl 63)]", TRUE),
        t("wire_extra/rt_small_ints_0_255", "the 0..255 integer range round-trips", "binary_to_term(term_to_binary(lists:seq(0,255))) =:= lists:seq(0,255)", TRUE),
        t("wire_extra/rt_bignum_pow200", "a 201-bit integer round-trips", "binary_to_term(term_to_binary(1 bsl 200)) =:= 1 bsl 200", TRUE),
        t("wire_extra/rt_map_single_nested", "a single-key map of mixed types round-trips", "binary_to_term(term_to_binary(#{key => [1, {2, <<3>>}, 4.5]})) =:= #{key => [1, {2, <<3>>}, 4.5]}", TRUE),
        t("wire_extra/rt_list_of_binaries", "a list of binaries round-trips", "binary_to_term(term_to_binary([<<1>>,<<2>>,<<3>>])) =:= [<<1>>,<<2>>,<<3>>]", TRUE),
        t("wire_extra/rt_deep_tuple", "a deeply nested tuple round-trips", "binary_to_term(term_to_binary({a,{b,{c,{d,e}}}})) =:= {a,{b,{c,{d,e}}}}", TRUE),
        t("wire_extra/rt_bit_binary_13", "a 13-bit bitstring round-trips", "binary_to_term(term_to_binary(<<5:13>>)) =:= <<5:13>>", TRUE),
        t("wire_extra/rt_deep_list", "a deeply nested list round-trips", "binary_to_term(term_to_binary([1,[2,[3,[4,[5]]]]])) =:= [1,[2,[3,[4,[5]]]]]", TRUE),
        t("wire_extra/rt_mixed_terms", "a spread of mixed-type terms each round-trips", "lists:all(fun(E) -> binary_to_term(term_to_binary(E)) =:= E end, [[], [1,2,3], <<1,2>>, <<5:3>>, {a,b}, #{k=>v}])", TRUE),
        // -- decode-side hand-built tag blobs the encode-only cases never exercised -------
        // Every shape below is a *fixed* ETF blob decoded under both runtimes and compared
        // through a value / re-encode predicate, so the tag's decode path is pinned even
        // when term_to_binary/1 would never emit that exact image.
        v("wire_extra/at_decode_115_small_atom_latin1", "a legacy SMALL_ATOM_EXT (115) blob decodes and re-encodes as SMALL_ATOM_UTF8_EXT (119)", "term_to_binary(binary_to_term(<<131,115,3,102,111,111>>))", "ok <<131,119,3,102,111,111>>"),
        t("wire_extra/fl_decode_99_legacy_eq", "a legacy ASCII FLOAT_EXT (99) image (from {minor_version,0}) decodes back to the double", "binary_to_term(term_to_binary(1.5, [{minor_version,0}])) =:= 1.5", TRUE),
        t("wire_extra/tu_decode_105_large_tuple_eq", "a hand-built LARGE_TUPLE_EXT (105) blob decodes to its 3-tuple", "binary_to_term(<<131,105,0,0,0,3,97,1,97,2,97,3>>) =:= {1,2,3}", TRUE),
        t("wire_extra/tu_decode_104_zero_arity_eq", "a hand-built zero-arity SMALL_TUPLE_EXT (104) blob decodes to the empty tuple", "binary_to_term(<<131,104,0>>) =:= {}", TRUE),
        t("wire_extra/li_decode_108_nonbyte_eq", "a hand-built LIST_EXT (108) blob with a non-byte element decodes to the list", "binary_to_term(<<131,108,0,0,0,2,97,1,98,0,0,1,44,106>>) =:= [1,300]", TRUE),
        t("wire_extra/li_decode_107_empty_string_eq", "a hand-built zero-length STRING_EXT (107) blob decodes to the empty list", "binary_to_term(<<131,107,0,0>>) =:= []", TRUE),
        t("wire_extra/bi_decode_109_empty_eq", "a hand-built zero-length BINARY_EXT (109) blob decodes to the empty binary", "binary_to_term(<<131,109,0,0,0,0>>) =:= <<>>", TRUE),
        t("wire_extra/bi_decode_109_identity_eq", "a hand-built BINARY_EXT (109) blob re-encodes to identical bytes", "term_to_binary(binary_to_term(<<131,109,0,0,0,2,1,2>>)) =:= <<131,109,0,0,0,2,1,2>>", TRUE),
        t("wire_extra/bit_decode_77_identity_eq", "a hand-built 1-bit BIT_BINARY_EXT (77) blob re-encodes to identical bytes", "term_to_binary(binary_to_term(<<131,77,0,0,0,1,1,128>>)) =:= <<131,77,0,0,0,1,1,128>>", TRUE),
        t("wire_extra/ma_decode_116_empty_eq", "a hand-built zero-pair MAP_EXT (116) blob decodes to the empty map", "binary_to_term(<<131,116,0,0,0,0>>) =:= #{}", TRUE),
        t("wire_extra/ma_decode_116_single_nil_value_eq", "a hand-built MAP_EXT (116) blob whose value is NIL_EXT decodes to a single-pair map", "binary_to_term(<<131,116,0,0,0,1,97,1,106>>) =:= #{1 => []}", TRUE),
        v("wire_extra/ma_empty_encode_bytes", "the empty map is the six-byte MAP_EXT (116) image", "term_to_binary(#{})", "ok <<131,116,0,0,0,0>>"),
        v("wire_extra/bi_empty_encode_bytes", "the empty binary is the six-byte BINARY_EXT (109) image", "term_to_binary(<<>>)", "ok <<131,109,0,0,0,0>>"),
        verr("wire_extra/err_version_magic_only", "a bare VERSION_MAGIC (131) with no term is badarg", "binary_to_term(<<131>>)"),
        // -- term_to_binary/2 option identities on top of the framing cases ---------------
        t("wire_extra/mv2_default_exact_eq", "the {minor_version,2} image equals the default image byte for byte", "term_to_binary(#{a=>1,b=>2}, [{minor_version,2}]) =:= term_to_binary(#{a=>1,b=>2})", TRUE),
        t("wire_extra/co_noop_small_term_exact_eq", "a small term is left uncompressed, so [compressed] is byte-identical to the default image", "term_to_binary(foo, [compressed]) =:= term_to_binary(foo)", TRUE),
        t("wire_extra/co_roundtrip_bitstring_eq", "a bitstring round-trips through the [compressed] option", "binary_to_term(term_to_binary(<<5:13>>, [compressed])) =:= <<5:13>>", TRUE),
        t("wire_extra/co_roundtrip_bignum_eq", "a 201-bit integer round-trips through the [compressed] option", "binary_to_term(term_to_binary(1 bsl 200, [compressed])) =:= 1 bsl 200", TRUE),
        t("wire_extra/co_roundtrip_mixed_compound_eq", "a mixed float/atom/binary/list/map structure round-trips through [compressed]", "binary_to_term(term_to_binary({1.5,a,<<1,2>>,[3],#{k=>v}}, [compressed])) =:= {1.5,a,<<1,2>>,[3],#{k=>v}}", TRUE),
        t("wire_extra/co_mv2_roundtrip_eq", "the combined [compressed, {minor_version,2}] options round-trip a long list", "binary_to_term(term_to_binary(lists:seq(1,500), [compressed, {minor_version,2}])) =:= lists:seq(1,500)", TRUE),
        t("wire_extra/opt_safe_compressed_accept_eq", "[safe] accepts a compressed image that introduces no new atoms", "binary_to_term(term_to_binary({ok,1}, [compressed]), [safe]) =:= {ok,1}", TRUE),
        // -- distribution control-message payload tuples (`DOP_*`) --------------------
        // The distributed-signal encoder frames every operation as an ETF control tuple
        // `{Tag, …}`. A live handshake / a node-to-node loopback needs two connected nodes
        // (see the module docs), but such a tuple is *just an ETF term*, so its payload
        // shape IS reachable through one CLI process. The all-atom shapes are byte goldens;
        // every shape carrying a pid or a reference is pinned through a shape/tag predicate
        // only — the id/creation/serial fields are per-VM-start values.
        t("wire_extra/dist_send_tag_2_node_bytes", "the DOP_SEND (2) control tuple {2, node()} encodes as a 2-tuple of a small integer and the UTF-8 local-node atom", "{2, node()}", "ok <<131,104,2,97,2,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        t("wire_extra/dist_link_tag_1_node_nodes_bytes", "the DOP_LINK (1) control tuple {1, node(), node()} encodes both node atoms", "{1, node(), node()}", "ok <<131,104,3,97,1,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        t("wire_extra/dist_unlink_tag_4_node_nodes_bytes", "the DOP_UNLINK (4) control tuple {4, node(), node()} encodes both node atoms", "{4, node(), node()}", "ok <<131,104,3,97,4,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        t("wire_extra/dist_node_link_tag_5_node_bytes", "the DOP_NODE_LINK (5) control tuple {5, node()} encodes the node atom", "{5, node()}", "ok <<131,104,2,97,5,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        t("wire_extra/dist_exit_tag_3_reason_bytes", "the DOP_EXIT (3) control tuple {3, node(), node(), normal} encodes its four fields", "{3, node(), node(), normal}", "ok <<131,104,4,97,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,6,110,111,114,109,97,108>>"),
        v("wire_extra/dist_send_tag_2_tag_byte", "the DOP_SEND (2) control tuple leads with SMALL_TUPLE_EXT (104) after VERSION_MAGIC", "binary:at(term_to_binary({2, node()}),1)", "ok 104"),
        v("wire_extra/dist_reg_send_tag_6_from_pid_is_pid", "the DOP_REG_SEND (6) control tuple's second field decodes to a pid", "is_pid(element(2, binary_to_term(term_to_binary({6, self(), world, hello}))))", "ok true"),
        v("wire_extra/dist_reg_send_tag_6_reencode_identity", "a DOP_REG_SEND (6) control tuple's decode -> re-encode is byte-identical", "term_to_binary(binary_to_term(term_to_binary({6, self(), world, hello}))) =:= term_to_binary({6, self(), world, hello})", "ok true"),
        v("wire_extra/dist_exit_tag_3_reason_element", "the DOP_EXIT (3) control tuple's reason field round-trips", "element(4, binary_to_term(term_to_binary({3, self(), self(), normal})))", "ok normal"),
        v("wire_extra/dist_monitor_p_tag_19_ref_is_reference", "the DOP_MONITOR_P (19) control tuple's reference field decodes to a reference", "begin Ref = binary_to_term(<<131,90,0,1,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202>>), is_reference(element(4, binary_to_term(term_to_binary({19, self(), self(), Ref, [monitor]})))) end", "ok true"),
        v("wire_extra/dist_demonitor_p_tag_20_first_element", "the DOP_DEMONITOR_P (20) control tuple keeps its tag as its first element", "begin Ref = binary_to_term(<<131,90,0,1,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202>>), element(1, binary_to_term(term_to_binary({20, self(), self(), Ref}))) end", "ok 20"),
        v("wire_extra/dist_monitor_p_exit_tag_21_reason_element", "the DOP_MONITOR_P_EXIT (21) control tuple's reason field round-trips", "begin Ref = binary_to_term(<<131,90,0,1,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202>>), element(5, binary_to_term(term_to_binary({21, self(), self(), Ref, noproc}))) end", "ok noproc"),
        v("wire_extra/dist_group_leader_tag_7_tuple_size", "the DOP_GROUP_LEADER (7) control tuple is a 3-tuple", "tuple_size(binary_to_term(term_to_binary({7, self(), self()})))", "ok 3"),
        v("wire_extra/dist_exit2_tag_8_reason_element", "the DOP_EXIT2 (8) control tuple's reason field round-trips", "element(4, binary_to_term(term_to_binary({8, self(), self(), kill})))", "ok kill"),
        // -- the `ERL_DIST_HEADER` framing (`VERSION_MAGIC` + `PASS_THROUGH`) ----------
        verr("wire_extra/dist_header_pass_through_only_badarg", "the bare distribution header <<131,112>> is a header flag, not a term, so binary_to_term/1 refuses it with badarg", "binary_to_term(<<131,112>>)"),
        verr("wire_extra/dist_header_pass_through_zero_control_len_badarg", "a header with a zero control-message length and no payload is still not a term (badarg)", "binary_to_term(<<131,112,0,0,0,0>>)"),
        v("wire_extra/dist_header_roundtrip_control_and_message", "a hand-built 131,112 header carrying a control term and a message term decodes both payloads", "begin Ctrl = term_to_binary({2, node()}), Msg = term_to_binary({ok,42}), Frame = <<131,112,(byte_size(Ctrl)):32,Ctrl/binary,Msg/binary>>, <<131,112,CL:32,Rest/binary>> = Frame, <<Ctl:CL/binary,Msg2/binary>> = Rest, {binary_to_term(Ctl), binary_to_term(Msg2)} end", "ok {{2,nonode@nohost},{ok,42}}"),
        v("wire_extra/dist_header_tag_and_control_len", "the header's first two bytes are VERSION_MAGIC (131) and PASS_THROUGH (112), and its 4-byte big-endian control length is the control term's size", "begin Ctrl = term_to_binary({2, node()}), Msg = term_to_binary({ok,42}), Frame = <<131,112,(byte_size(Ctrl)):32,Ctrl/binary,Msg/binary>>, {binary:at(Frame,0), binary:at(Frame,1), byte_size(Ctrl)} end", "ok {131,112,20}"),
        // -- external funs: EXPORT_EXT (113) under `{minor_version, _}` ---------------
        v("wire_extra/exp_ext_mv2_utf8_atom_bytes", "an external fun under {minor_version,2} encodes its module/function atoms as SMALL_ATOM_UTF8_EXT (119)", "term_to_binary(fun lists:map/2, [{minor_version,2}])", "ok <<131,113,119,5,108,105,115,116,115,119,3,109,97,112,97,2>>"),
        v("wire_extra/exp_ext_mv2_equals_default", "an external fun's {minor_version,2} image equals the default image byte for byte", "term_to_binary(fun lists:map/2, [{minor_version,2}]) =:= term_to_binary(fun lists:map/2)", "ok true"),
        v("wire_extra/exp_ext_mv1_legacy_atom_bytes", "an external fun under {minor_version,1} encodes its module/function atoms as the legacy ATOM_EXT (100)", "term_to_binary(fun lists:map/2, [{minor_version,1}])", "ok <<131,113,100,0,5,108,105,115,116,115,100,0,3,109,97,112,97,2>>"),
        v("wire_extra/exp_ext_mv1_differs_from_default", "an external fun's {minor_version,1} image differs from the default image (the nested atoms switch to ATOM_EXT)", "term_to_binary(fun lists:map/2, [{minor_version,1}]) =/= term_to_binary(fun lists:map/2)", "ok true"),
        t("wire_extra/exp_ext_mv1_reencode_as_utf8_bytes", "an EXPORT_EXT image decoded from {minor_version,1} re-encodes with the default (UTF-8) atom tags", "binary_to_term(term_to_binary(fun lists:map/2, [{minor_version,1}]))", "ok <<131,113,119,5,108,105,115,116,115,119,3,109,97,112,97,2>>"),
        // -- `{minor_version, _}`: the re-encode identity of a legacy image -----------
        t("wire_extra/mv_legacy_atom_reencode_bytes", "a legacy ATOM_EXT (100) atom image decodes and re-encodes as SMALL_ATOM_UTF8_EXT (119)", "binary_to_term(term_to_binary(hello,[{minor_version,1}]))", "ok <<131,119,5,104,101,108,108,111>>"),
        v("wire_extra/mv_legacy_float_decode_roundtrip", "a legacy ASCII FLOAT_EXT (99) image (from {minor_version,0}) decodes back to the double", "binary_to_term(term_to_binary(3.5, [{minor_version,0}])) =:= 3.5", "ok true"),
        // -- `binary_to_term/2`: `[safe]` / `[used]` shapes ---------------------------
        v("wire_extra/b2t_safe_existing_atom_accept", "[safe] accepts an image naming an atom that already exists in the node", "begin _ = list_to_atom(\"wz_safe_probe_atom\"), binary_to_term(term_to_binary(wz_safe_probe_atom), [safe]) end", "ok wz_safe_probe_atom"),
        verr("wire_extra/b2t_safe_nested_new_atom_reject", "[safe] rejects a fresh atom nested inside a list image", "binary_to_term(<<131,108,0,0,0,1,119,9,122,122,110,101,119,97,116,111,109,106>>, [safe])"),
        v("wire_extra/b2t_used_two_terms_bytes", "[used] reports the first term of a two-term buffer and its byte count", "binary_to_term(<<(term_to_binary(1))/binary,(term_to_binary(2))/binary>>, [used])", "ok {1,3}"),
        v("wire_extra/b2t_used_local_pid_bytes", "[used] reports the 29-byte size of the runtime's own NEW_PID_EXT (88) image", "element(2, binary_to_term(<<(term_to_binary(self()))/binary,255>>, [used]))", "ok 29"),
        verr("wire_extra/b2t_unknown_option_alongside_safe", "an unknown binary_to_term/2 option alongside [safe] is badarg", "binary_to_term(term_to_binary(1),[safe,bogus])"),
        // -- local external-id images as shape predicates ----------------------------
        v("wire_extra/ids_self_pid_image_size", "the runtime's own pid image (NEW_PID_EXT 88) is 29 bytes: 2 + a 15-byte node atom + a 12-byte tail", "byte_size(term_to_binary(self()))", "ok 29"),
        v("wire_extra/ids_self_pid_node_prefix", "the 15-byte node-name prefix of the runtime's own pid image names the local node", "binary:part(term_to_binary(self()),2,15)", "ok <<119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        v("wire_extra/ids_internal_port_rereencode_identity", "decoding the runtime's own port image (V4_PORT_EXT 120) and re-encoding it is byte-identical", "begin P = open_port({spawn,\"cat\"},[binary]), B = term_to_binary(P), B2 = term_to_binary(binary_to_term(B)), port_close(P), B2 =:= B end", "ok true"),
        v("wire_extra/ids_internal_port_image_size", "the runtime's own port image (V4_PORT_EXT 120) is 29 bytes: 2 + a 15-byte node atom + a 12-byte tail", "begin P = open_port({spawn,\"cat\"},[binary]), N = byte_size(term_to_binary(P)), port_close(P), N end", "ok 29"),
        v("wire_extra/ids_local_reference_rereencode_identity", "a NEWER_REFERENCE_EXT (90) image naming the local node decodes and re-encodes to identical bytes", "term_to_binary(binary_to_term(<<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>)) =:= <<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>", "ok true"),
        // -- node identity around the external ids + handshake-shaped tuples ---------
        v("wire_extra/node_of_external_reference_is_other", "erlang:node/1 of a decoded NEWER_REFERENCE_EXT (90) names the node atom that image carries", "node(binary_to_term(<<131,90,0,1,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202>>))", "ok other"),
        v("wire_extra/nodes_connected_is_empty", "erlang:nodes(connected) is [] on a non-distributed node (no distribution layer has connected a peer)", "erlang:nodes(connected)", "ok []"),
        verr("wire_extra/set_cookie_other_node_binary_cookie_badarg", "erlang:set_cookie/2 on a remote-looking node with a binary cookie is badarg (a cookie must be an atom)", "erlang:set_cookie('someother@node', <<\"cookie\">>)"),
        t("wire_extra/hs_ok_node_bytes", "a handshake-shaped {ok, Node} reply tuple encodes the local-node atom", "{ok, node()}", "ok <<131,104,2,119,2,111,107,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        t("wire_extra/hs_error_distribution_not_started_bytes", "a handshake-shaped {error, Reason} reply tuple encodes the distribution_not_started atom", "{error, distribution_not_started}", "ok <<131,104,2,119,5,101,114,114,111,114,119,24,100,105,115,116,114,105,98,117,116,105,111,110,95,110,111,116,95,115,116,97,114,116,101,100>>"),
        // -- evaluation limits: regression-pinned `-eval` literal/parser shapes -------------
        // Each golden is the oracle's own `ok …` line and the Rust runtime now reproduces it,
        // so each case `agree`s. They were once honest capability gaps (`beam_rt: unsupported:
        // …`, exit 3); the `_unsupported` id suffixes are historical — kept only so the ids
        // stay byte-stable.
        v("wire_extra/eval_bigint_literal_unsupported", "a 2^63 integer literal now evaluates in the Rust runtime (regression-pinned agreement)", "9223372036854775808", "ok 9223372036854775808"),
        v("wire_extra/eval_improper_list_literal_unsupported", "a multi-element improper list literal [1,2|3] now evaluates in the Rust runtime (regression-pinned agreement)", "[1,2|3]", "ok [1,2|3]"),
        v("wire_extra/eval_wide_binary_segment_unsupported", "an integer binary segment wider than 64 bits now evaluates in the Rust runtime (regression-pinned agreement)", "erlang:bit_size(<<0:2048>>)", "ok 2048"),
        // The newly-closed shapes are deepened so they cannot silently regress: a negative
        // big-integer literal, a literal above 2^63, a three-element improper-list head, and
        // binary-segment widths straddling the old 64-bit ceiling.
        v("wire_extra/eval_negative_bigint_literal", "a negative big-integer literal lower than -2^63 evaluates", "-9223372036854775809", "ok -9223372036854775809"),
        v("wire_extra/eval_bigint_literal_above_2_pow_63", "an integer literal one above 2^63 evaluates", "9223372036854775809", "ok 9223372036854775809"),
        v("wire_extra/eval_improper_list_three_element_head", "a three-element improper list literal [1,2,3|4] evaluates", "[1,2,3|4]", "ok [1,2,3|4]"),
        v("wire_extra/eval_wide_binary_segment_128", "a 128-bit integer binary segment evaluates", "erlang:bit_size(<<0:128>>)", "ok 128"),
        v("wire_extra/eval_wide_binary_segment_65", "a 65-bit integer binary segment (one bit above the old 64-bit ceiling) evaluates", "erlang:bit_size(<<0:65>>)", "ok 65"),
    ]
}

#[cfg(test)]
mod tests {
    use super::{group, TRUE};
    use crate::corpus::Contract;
    use std::collections::HashSet;

    #[test]
    fn wire_extra_cases_are_well_formed() {
        let cases = group();
        assert!(
            cases.len() >= 100,
            "the expanded A.5 group should be broad (got {})",
            cases.len()
        );
        let mut seen = HashSet::new();
        for case in &cases {
            assert!(case.id.starts_with("wire_extra/"), "unexpected id {}", case.id);
            assert!(seen.insert(case.id), "duplicate case id {}", case.id);
            assert!(
                !case.description.is_empty(),
                "case {} needs a description",
                case.id
            );
            assert_eq!(
                case.contracts,
                &[Contract::Distribution],
                "case {} must evidence the distribution contract",
                case.id
            );
            let expr = case.expr().expect("every wire_extra case is an eval case");
            assert!(!expr.is_empty(), "case {} has an empty expression", case.id);
            assert!(case.extra_argv().is_empty(), "case {} sets extra argv", case.id);
            assert!(case.env().is_empty(), "case {} sets an env", case.id);
            let golden = case.golden.stdout.expect("every case has a stdout golden");
            assert!(
                golden.starts_with("ok ") || golden.starts_with("exception "),
                "case {}: unexpected golden {golden:?}",
                case.id
            );
            assert_eq!(
                golden.starts_with("ok "),
                case.golden.exit_code == 0,
                "case {}: exit code disagrees with the golden shape",
                case.id
            );
        }
    }

    #[test]
    fn wire_extra_covers_the_tag_families() {
        let cases = group();
        // A representative id from each distinct tag family the group exercises.
        for id in [
            "wire_extra/ie_small_big_2_pow_64",
            "wire_extra/ie_tag_large_big_2040",
            "wire_extra/fl_pi",
            "wire_extra/at_decode_118_utf8",
            "wire_extra/at_255_len_tag",
            "wire_extra/tu_arity256_tag",
            "wire_extra/li_improper_atom_tail",
            "wire_extra/bit_9_bits",
            "wire_extra/bit_8_bits_is_binary_tag",
            "wire_extra/bi_255_tag",
            "wire_extra/li_list_256_tag",
            "wire_extra/ma_single_binary_key",
            "wire_extra/fun_export_map_encode",
            "wire_extra/co_large_tag",
            "wire_extra/co_roundtrip_nested",
            "wire_extra/mv2_nonlatin_atom_tag",
            "wire_extra/vf_decode_hand_built_atom",
            "wire_extra/rt_mixed_terms",
            "wire_extra/opt_used_shape",
            "wire_extra/err_truncated_tuple",
        ] {
            assert!(
                cases.iter().any(|c| c.id == id),
                "expected the tag-family case {id}"
            );
        }
    }

    #[test]
    fn wire_extra_covers_the_evaluation_limit_family() {
        let cases = group();
        // The `-eval` literal/parser family: each golden is the oracle's own `ok …` line and
        // the Rust runtime now reproduces it, so the case `agree`s (these were once honest
        // capability gaps that flipped to agreement once `beam_rt` grew the shape). Pin the ids
        // and their value-probe goldens here so the agreements cannot regress silently.
        for (id, golden) in [
            ("wire_extra/eval_bigint_literal_unsupported", "ok 9223372036854775808"),
            ("wire_extra/eval_improper_list_literal_unsupported", "ok [1,2|3]"),
            ("wire_extra/eval_wide_binary_segment_unsupported", "ok 2048"),
            ("wire_extra/eval_negative_bigint_literal", "ok -9223372036854775809"),
            ("wire_extra/eval_bigint_literal_above_2_pow_63", "ok 9223372036854775809"),
            ("wire_extra/eval_improper_list_three_element_head", "ok [1,2,3|4]"),
            ("wire_extra/eval_wide_binary_segment_128", "ok 128"),
            ("wire_extra/eval_wide_binary_segment_65", "ok 65"),
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the evaluation-limit case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some(golden),
                "case {id} must carry the oracle-derived golden"
            );
            assert_eq!(case.golden.exit_code, 0, "case {id} golden must exit 0");
        }
    }

    #[test]
    fn wire_extra_covers_the_distribution_control_and_header_family() {
        let cases = group();
        // The distribution control-message payload tuples (`DOP_*`), the `ERL_DIST_HEADER`
        // framing, the external-fun `{minor_version,_}` images, the `binary_to_term/2`
        // `[safe]`/`[used]` shapes and the local external-id shape predicates added on top of
        // the tag-level cases. Every golden is the oracle's own line and the Rust runtime
        // reproduces it (each case `agree`s); pin the ids and their goldens here so the
        // agreements cannot regress silently.
        for (id, golden) in [
            ("wire_extra/dist_send_tag_2_node_bytes", "ok <<131,104,2,97,2,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/dist_link_tag_1_node_nodes_bytes", "ok <<131,104,3,97,1,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/dist_unlink_tag_4_node_nodes_bytes", "ok <<131,104,3,97,4,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/dist_node_link_tag_5_node_bytes", "ok <<131,104,2,97,5,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/dist_exit_tag_3_reason_bytes", "ok <<131,104,4,97,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,119,6,110,111,114,109,97,108>>"),
            ("wire_extra/dist_send_tag_2_tag_byte", "ok 104"),
            ("wire_extra/dist_reg_send_tag_6_from_pid_is_pid", "ok true"),
            ("wire_extra/dist_reg_send_tag_6_reencode_identity", "ok true"),
            ("wire_extra/dist_exit_tag_3_reason_element", "ok normal"),
            ("wire_extra/dist_monitor_p_tag_19_ref_is_reference", "ok true"),
            ("wire_extra/dist_demonitor_p_tag_20_first_element", "ok 20"),
            ("wire_extra/dist_monitor_p_exit_tag_21_reason_element", "ok noproc"),
            ("wire_extra/dist_group_leader_tag_7_tuple_size", "ok 3"),
            ("wire_extra/dist_exit2_tag_8_reason_element", "ok kill"),
            ("wire_extra/dist_header_roundtrip_control_and_message", "ok {{2,nonode@nohost},{ok,42}}"),
            ("wire_extra/dist_header_tag_and_control_len", "ok {131,112,20}"),
            ("wire_extra/exp_ext_mv2_utf8_atom_bytes", "ok <<131,113,119,5,108,105,115,116,115,119,3,109,97,112,97,2>>"),
            ("wire_extra/exp_ext_mv2_equals_default", "ok true"),
            ("wire_extra/exp_ext_mv1_legacy_atom_bytes", "ok <<131,113,100,0,5,108,105,115,116,115,100,0,3,109,97,112,97,2>>"),
            ("wire_extra/exp_ext_mv1_differs_from_default", "ok true"),
            ("wire_extra/exp_ext_mv1_reencode_as_utf8_bytes", "ok <<131,113,119,5,108,105,115,116,115,119,3,109,97,112,97,2>>"),
            ("wire_extra/mv_legacy_atom_reencode_bytes", "ok <<131,119,5,104,101,108,108,111>>"),
            ("wire_extra/mv_legacy_float_decode_roundtrip", "ok true"),
            ("wire_extra/b2t_safe_existing_atom_accept", "ok wz_safe_probe_atom"),
            ("wire_extra/b2t_used_two_terms_bytes", "ok {1,3}"),
            ("wire_extra/b2t_used_local_pid_bytes", "ok 29"),
            ("wire_extra/ids_self_pid_image_size", "ok 29"),
            ("wire_extra/ids_self_pid_node_prefix", "ok <<119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/ids_internal_port_rereencode_identity", "ok true"),
            ("wire_extra/ids_internal_port_image_size", "ok 29"),
            ("wire_extra/ids_local_reference_rereencode_identity", "ok true"),
            ("wire_extra/node_of_external_reference_is_other", "ok other"),
            ("wire_extra/nodes_connected_is_empty", "ok []"),
            ("wire_extra/hs_ok_node_bytes", "ok <<131,104,2,119,2,111,107,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/hs_error_distribution_not_started_bytes", "ok <<131,104,2,119,5,101,114,114,111,114,119,24,100,105,115,116,114,105,98,117,116,105,111,110,95,110,111,116,95,115,116,97,114,116,101,100>>"),
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the distribution/header case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some(golden),
                "case {id} must carry the oracle-derived golden"
            );
            assert_eq!(case.golden.exit_code, 0, "case {id} golden must exit 0");
        }
        // The header-refusal / `[safe]`-reject / unknown-option shapes are the error cases.
        for id in [
            "wire_extra/dist_header_pass_through_only_badarg",
            "wire_extra/dist_header_pass_through_zero_control_len_badarg",
            "wire_extra/b2t_safe_nested_new_atom_reject",
            "wire_extra/b2t_unknown_option_alongside_safe",
            "wire_extra/set_cookie_other_node_binary_cookie_badarg",
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the error case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some("exception error badarg"),
                "case {id} must carry the oracle-derived badarg golden"
            );
            assert_eq!(case.golden.exit_code, 1, "case {id} golden must exit 1");
        }
    }

    #[test]
    fn wire_extra_covers_the_decode_side_and_option_families() {
        let cases = group();
        // The decode-side hand-built tag blobs and the `term_to_binary/2` option identities
        // added on top of the encode-only cases. Every golden is the oracle's own line and the
        // Rust runtime reproduces it (each case `agree`s); pin the ids and their goldens here so
        // the agreements cannot regress silently.
        for (id, golden) in [
            ("wire_extra/at_decode_115_small_atom_latin1", "ok <<131,119,3,102,111,111>>"),
            ("wire_extra/fl_decode_99_legacy_eq", TRUE),
            ("wire_extra/tu_decode_105_large_tuple_eq", TRUE),
            ("wire_extra/tu_decode_104_zero_arity_eq", TRUE),
            ("wire_extra/li_decode_108_nonbyte_eq", TRUE),
            ("wire_extra/li_decode_107_empty_string_eq", TRUE),
            ("wire_extra/bi_decode_109_empty_eq", TRUE),
            ("wire_extra/bi_decode_109_identity_eq", TRUE),
            ("wire_extra/bit_decode_77_identity_eq", TRUE),
            ("wire_extra/ma_decode_116_empty_eq", TRUE),
            ("wire_extra/ma_decode_116_single_nil_value_eq", TRUE),
            ("wire_extra/ma_empty_encode_bytes", "ok <<131,116,0,0,0,0>>"),
            ("wire_extra/bi_empty_encode_bytes", "ok <<131,109,0,0,0,0>>"),
            ("wire_extra/mv2_default_exact_eq", TRUE),
            ("wire_extra/co_noop_small_term_exact_eq", TRUE),
            ("wire_extra/co_roundtrip_bitstring_eq", TRUE),
            ("wire_extra/co_roundtrip_bignum_eq", TRUE),
            ("wire_extra/co_roundtrip_mixed_compound_eq", TRUE),
            ("wire_extra/co_mv2_roundtrip_eq", TRUE),
            ("wire_extra/opt_safe_compressed_accept_eq", TRUE),
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the decode/option case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some(golden),
                "case {id} must carry the oracle-derived golden"
            );
            assert_eq!(case.golden.exit_code, 0, "case {id} golden must exit 0");
        }
        // The bare VERSION_MAGIC shape is the one error case in this family.
        let err = cases
            .iter()
            .find(|c| c.id == "wire_extra/err_version_magic_only")
            .expect("expected the bare VERSION_MAGIC error case");
        assert_eq!(err.golden.stdout, Some("exception error badarg"));
        assert_eq!(err.golden.exit_code, 1);
    }
}
