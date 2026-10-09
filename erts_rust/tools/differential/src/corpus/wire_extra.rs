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
//! * binaries — `BINARY_EXT` (109) and `BIT_BINARY_EXT` (77) at every bit-count a
//!   non-byte-aligned value exercises (2..15) plus round trips;
//! * maps — `MAP_EXT` (116): single-key byte goldens, and multi-key maps only ever
//!   through round-trip equality / `maps:size` / sorted key lists (iteration order is
//!   per-VM and never asserted);
//! * funs — `EXPORT_EXT` (113) for external funs (`fun M:F/A`) plus the decode round trip;
//! * framing — `VERSION_MAGIC` (131) as the first byte, the `compressed` (80) option
//!   (`[compressed]`, `{compressed,Level}`, and the no-op small-term path),
//!   `{minor_version,1|2}` and the first-byte/tag determinism predicates;
//! * `erlang:binary_to_term/2` — the `[safe]` accept/reject surface, the `[used]`
//!   `{Term, BytesUsed}` shape, and the truncated / garbage / wrong-arity error shapes.
//!
//! A live handshake and the `DOP_*` control framing are **not** expressible through a
//! single CLI process (they need two connected nodes) — an honest, documented gap, not a
//! fabricated case (see `super::wire`).

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
        v("wire_extra/li_empty_nil_size", "the empty list is the two-byte NIL_EXT (106) image", "byte_size(term_to_binary([]))", "ok 2"),
        t("wire_extra/li_string_long_roundtrip", "a 1000-byte string list round-trips", "binary_to_term(term_to_binary(lists:duplicate(1000,$a))) =:= lists:duplicate(1000,$a)", TRUE),
        t("wire_extra/li_long_list_roundtrip", "a 1000-element integer list round-trips", "binary_to_term(term_to_binary(lists:seq(1,1000))) =:= lists:seq(1,1000)", TRUE),
        t("wire_extra/li_improper_roundtrip", "a 2-cons improper list round-trips", "binary_to_term(term_to_binary([a|[b|c]])) =:= [a|[b|c]]", TRUE),
        t("wire_extra/li_nested_int_lists", "a list of lists round-trips", "binary_to_term(term_to_binary([[1,2],[3,4]])) =:= [[1,2],[3,4]]", TRUE),
        // -- binaries: BINARY_EXT (109) / BIT_BINARY_EXT (77) --------------------------
        t("wire_extra/bi_one_byte", "a one-byte binary encodes as BINARY_EXT (109)", "<<0>>", "ok <<131,109,0,0,0,1,0>>"),
        t("wire_extra/bi_two_bytes", "a two-byte binary encodes as BINARY_EXT (109)", "<<1,2>>", "ok <<131,109,0,0,0,2,1,2>>"),
        v("wire_extra/bi_256_size", "a 256-byte binary image is the six-byte header plus 256 bytes", "byte_size(term_to_binary(binary:copy(<<0>>,256)))", "ok 262"),
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
        // -- round trips across every family ------------------------------------------
        t("wire_extra/rt_negative_ints", "a list of negative integers (incl. the 64-bit floor) round-trips", "binary_to_term(term_to_binary([-1,-300,-(1 bsl 63)])) =:= [-1,-300,-(1 bsl 63)]", TRUE),
        t("wire_extra/rt_small_ints_0_255", "the 0..255 integer range round-trips", "binary_to_term(term_to_binary(lists:seq(0,255))) =:= lists:seq(0,255)", TRUE),
        t("wire_extra/rt_bignum_pow200", "a 201-bit integer round-trips", "binary_to_term(term_to_binary(1 bsl 200)) =:= 1 bsl 200", TRUE),
        t("wire_extra/rt_map_single_nested", "a single-key map of mixed types round-trips", "binary_to_term(term_to_binary(#{key => [1, {2, <<3>>}, 4.5]})) =:= #{key => [1, {2, <<3>>}, 4.5]}", TRUE),
        t("wire_extra/rt_list_of_binaries", "a list of binaries round-trips", "binary_to_term(term_to_binary([<<1>>,<<2>>,<<3>>])) =:= [<<1>>,<<2>>,<<3>>]", TRUE),
        t("wire_extra/rt_deep_tuple", "a deeply nested tuple round-trips", "binary_to_term(term_to_binary({a,{b,{c,{d,e}}}})) =:= {a,{b,{c,{d,e}}}}", TRUE),
        t("wire_extra/rt_bit_binary_13", "a 13-bit bitstring round-trips", "binary_to_term(term_to_binary(<<5:13>>)) =:= <<5:13>>", TRUE),
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
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
            "wire_extra/ma_single_binary_key",
            "wire_extra/fun_export_map_encode",
            "wire_extra/co_large_tag",
            "wire_extra/mv2_nonlatin_atom_tag",
            "wire_extra/opt_used_shape",
            "wire_extra/err_truncated_tuple",
        ] {
            assert!(
                cases.iter().any(|c| c.id == id),
                "expected the tag-family case {id}"
            );
        }
    }
}
