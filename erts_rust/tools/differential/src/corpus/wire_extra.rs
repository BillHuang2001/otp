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
//! The **widened** block adds the remaining tag byte goldens and refusal shapes:
//!
//! * integers — the `SMALL_INTEGER_EXT` (97) lower/upper bounds (`0`/`255`), the
//!   `INTEGER_EXT` (98) `2^16` crossing and mid-range / negative images, the
//!   `2^32` `SMALL_BIG_EXT` (110) pair, and the **decode** rules (a magnitude fitting a
//!   60-bit small decodes to a small and re-encodes as `97`, leading zero digits are
//!   stripped, **any** non-zero sign byte means negative, a zero magnitude is `0`), plus
//!   the `LARGE_BIG_EXT` (111) → `INTEGER_EXT` re-encode and a 301-bit round trip;
//! * floats — more `NEW_FLOAT_EXT` (70) byte goldens (`0.5`, `-1.5`), a fixed `70` decode,
//!   the subnormal round trip, and the legacy ASCII `FLOAT_EXT` (99) canonical-`%.20e`
//!   rule (a `"1.5"`-NUL-padded body is refused);
//! * atoms — the empty atom across `ATOM_EXT` (100) / `SMALL_ATOM_UTF8_EXT` (119) /
//!   `ATOM_UTF8_EXT` (118), the 16-bit-length `118` boundary a 255-code-point multi-byte
//!   name crosses (510 bytes), and the node-atom decode;
//! * lists/tuples/binaries — the `NIL_EXT` (106) and empty-`SMALL_TUPLE_EXT` (104) byte
//!   goldens, the 65535/65536-byte `STRING_EXT` (107) → `LIST_EXT` (108) boundary, the
//!   `LARGE_TUPLE_EXT` (105) decode + 257/300-element round trips, the declared-arity vs
//!   element-count rule, a zero-valued 1-bit image, a 7-bit decode and the 12-bit image's
//!   bit arithmetic;
//! * maps/funs — a single float-keyed byte golden, the 32 → 33 pair flatmap → HAMT
//!   boundary, `erlang:abs/1` as `EXPORT_EXT` (113), and a `{minor_version,1}` external
//!   fun's legacy `ATOM_EXT` atoms;
//! * framing/compression — the `{minor_version,0|1|2}` legacy-compound / node-atom /
//!   control-tuple images and the `{minor_version,2}` == default identity, the
//!   `[compressed]` no-op on the two smallest terms, `[used]` over a compressed image and
//!   the `[compressed, {minor_version,2}]` combination;
//! * the remaining refusal shapes — `DIST_HEADER` (68), the removed `FUN_EXT` (117), the
//!   dead `NEW_CACHE` (78), the unassigned tag `0`, a corrupt-zlib / empty `COMPRESSED` (80)
//!   body, the legacy PID_EXT (103) / PORT_EXT (102) tiny-creation bound (`> 3`), a
//!   `REFERENCE_EXT` (101) first id `>= MAX_REFERENCE` and a `NEWER_REFERENCE_EXT` (90) id
//!   count `> MAX_REF_IDS`;
//! * more `DOP_*` control tuples — `DOP_SEND_TT` (12), `DOP_EXIT_TT` (13),
//!   `DOP_REG_SEND_TT` (16), `DOP_EXIT2_TT` (18), `DOP_MONITOR_P_EXIT` (21),
//!   `DOP_SEND_SENDER` (22), `DOP_SEND_SENDER_TT` (23), `DOP_PAYLOAD` (24) and
//!   `DOP_PAYLOAD_SEND` (29) — all-atom byte goldens where the shape carries no id, and
//!   shape/tag predicates where it carries a pid; plus `erlang:node/1` of a remote
//!   `NEW_PID_EXT` (88) and the legacy `REFERENCE_EXT` → `NEWER_REFERENCE_EXT` re-encode.
//! * the **handshake control messages** themselves — the DFLAG-carrying name message
//!   `{node(), Flags}` (a zero and a full 32-bit `DFLAG_*` word), the five status atoms
//!   (`ok`/`nok`/`alive`/`not_alive`/`ok_simultaneous`), the bare `challenge` integer
//!   (`INTEGER_EXT` 98) and the `{ok, Challenge}` / `{ok, Our, Peer, PeerName}` reply
//!   tuples — each a byte golden, since every shape is an ordinary ETF term that carries
//!   no per-VM-start id.
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
        // -- the handshake control messages themselves (the DFLAG-carrying name message,
        // the status atoms and the challenge/reply integers) --------------------------
        // The `send_name` / `recv_name` message is the 2-tuple `{Name, Flags}` whose
        // `Flags` word is the OR of the handshake's `DFLAG_*` bits; the status message is
        // one of five atoms; the challenge and its replies are integers / small tuples.
        // Each is *just an ETF term*, so its payload shape is reachable through one CLI
        // process and pinned as a byte golden (the all-atom / all-integer shapes carry no
        // per-VM-start id, so they are VM-stable).
        t("wire_extra/hs_name_message_zero_flags_bytes", "the handshake name message {node(), 0} encodes the node atom and a zero DFLAG word", "{node(), 0}", "ok <<131,104,2,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,97,0>>"),
        t("wire_extra/hs_name_message_full_flags_bytes", "the handshake name message carries a full 32-bit DFLAG word (a SMALL_BIG_EXT above 2^31-1)", "{node(), 16#ffffffff}", "ok <<131,104,2,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,110,4,0,255,255,255,255>>"),
        t("wire_extra/hs_status_atoms_bytes", "the five handshake status atoms (ok/nok/alive/not_alive/ok_simultaneous) encode as SMALL_ATOM_UTF8_EXT", "[ok, nok, alive, not_alive, ok_simultaneous]", "ok <<131,108,0,0,0,5,119,2,111,107,119,3,110,111,107,119,5,97,108,105,118,101,119,9,110,111,116,95,97,108,105,118,101,119,15,111,107,95,115,105,109,117,108,116,97,110,101,111,117,115,106>>"),
        t("wire_extra/hs_challenge_int_bytes", "the handshake challenge is a bare integer, encoding as INTEGER_EXT (98)", "123456789", "ok <<131,98,7,91,205,21>>"),
        t("wire_extra/hs_challenge_reply_ok_bytes", "the handshake challenge_reply {ok, Challenge} encodes both fields", "{ok, 123456789}", "ok <<131,104,2,119,2,111,107,98,7,91,205,21>>"),
        t("wire_extra/hs_challenge_reply_full_peer_bytes", "the handshake challenge_reply {ok, OurChallenge, PeerChallenge, PeerName} encodes the peer node atom", "{ok, 123456789, 987654321, peer@host}", "ok <<131,104,4,119,2,111,107,98,7,91,205,21,98,58,222,104,177,119,9,112,101,101,114,64,104,111,115,116>>"),
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
        // -- integers: the remaining SMALL_INTEGER_EXT / INTEGER_EXT / SMALL_BIG_EXT byte
        // goldens (the lower bound, the 0..255 upper bound, the 2^16/2^32 crossings, and the
        // mid-range / negative INTEGER_EXT images) ---------------------------------------
        t("wire_extra/ie_small_int_zero_bytes", "0 is the SMALL_INTEGER_EXT (97) lower bound, one tag byte and a zero", "0", "ok <<131,97,0>>"),
        t("wire_extra/ie_small_int_255_bytes", "255 is the largest SMALL_INTEGER_EXT (97) value", "255", "ok <<131,97,255>>"),
        t("wire_extra/ie_int_ext_256", "256 is the smallest INTEGER_EXT (98) value", "256", "ok <<131,98,0,0,1,0>>"),
        t("wire_extra/ie_int_ext_65535", "65535 is the largest 16-bit INTEGER_EXT (98) value", "65535", "ok <<131,98,0,0,255,255>>"),
        t("wire_extra/ie_int_ext_100000", "100000 is a mid-range INTEGER_EXT (98) value", "100000", "ok <<131,98,0,1,134,160>>"),
        t("wire_extra/ie_int_ext_neg300", "-300 is a negative INTEGER_EXT (98) value", "(-300)", "ok <<131,98,255,255,254,212>>"),
        t("wire_extra/ie_int_ext_neg65536", "-65536 is a sign-extended INTEGER_EXT (98) value", "(-65536)", "ok <<131,98,255,255,0,0>>"),
        t("wire_extra/ie_int_ext_neg100001", "-100001 is a negative mid-range INTEGER_EXT (98) value", "(-100001)", "ok <<131,98,255,254,121,95>>"),
        t("wire_extra/ie_small_big_2_pow_32", "2^32 is a five-digit SMALL_BIG_EXT (110) with a low-order low byte", "4294967296", "ok <<131,110,5,0,0,0,0,0,1>>"),
        t("wire_extra/ie_small_big_neg_2_pow_32", "-(2^32) is a five-digit negative SMALL_BIG_EXT (110)", "(-(1 bsl 32))", "ok <<131,110,5,1,0,0,0,0,1>>"),
        // -- the decode direction of the integer tags: a small-big whose magnitude fits a
        // small decodes to a small (and re-encodes as SMALL_INTEGER_EXT), the leading-zero and
        // sign-byte rules, and the LARGE_BIG_EXT → INTEGER_EXT re-encode ------------------
        v("wire_extra/ie_decode_97_zero", "a hand-built SMALL_INTEGER_EXT (97) zero blob decodes to 0", "binary_to_term(<<131,97,0>>)", "ok 0"),
        v("wire_extra/ie_decode_98_256", "a hand-built INTEGER_EXT (98) blob decodes to the value it carries", "binary_to_term(<<131,98,0,0,1,0>>)", "ok 256"),
        t("wire_extra/ie_decode_110_to_97_reencode", "a SMALL_BIG_EXT (110) whose magnitude fits a small decodes to a small and re-encodes as SMALL_INTEGER_EXT (97)", "binary_to_term(<<131,110,1,0,42>>)", "ok <<131,97,42>>"),
        v("wire_extra/ie_decode_110_leading_zero_stripped", "a SMALL_BIG_EXT (110) with leading zero digits decodes to 0", "binary_to_term(<<131,110,2,0,0,0>>)", "ok 0"),
        v("wire_extra/ie_decode_110_sign_byte_two_is_negative", "any non-zero SMALL_BIG_EXT (110) sign byte means negative (the sign byte 2 negates too)", "binary_to_term(<<131,110,1,2,5>>)", "ok -5"),
        v("wire_extra/ie_decode_110_neg_zero_magnitude_is_zero", "a negative SMALL_BIG_EXT (110) with a zero magnitude decodes to 0", "binary_to_term(<<131,110,1,1,0>>)", "ok 0"),
        t("wire_extra/ie_decode_111_to_98_reencode", "a LARGE_BIG_EXT (111) blob whose value fits a signed 32-bit re-encodes as INTEGER_EXT (98)", "binary_to_term(<<131,111,0,0,0,2,0,0,1>>)", "ok <<131,98,0,0,1,0>>"),
        v("wire_extra/ie_roundtrip_bsl300", "a 301-bit integer (two LARGE_BIG_EXT digits) round-trips", "binary_to_term(term_to_binary(1 bsl 300)) =:= 1 bsl 300", "ok true"),
        // -- floats: more NEW_FLOAT_EXT (70) byte goldens, a fixed 70 decode, a subnormal
        // round trip, and the legacy ASCII FLOAT_EXT (99) canonical-rendering rule ----------
        t("wire_extra/fl_half", "0.5 is IEEE-754 0x3FE0000000000000", "0.5", "ok <<131,70,63,224,0,0,0,0,0,0>>"),
        t("wire_extra/fl_neg_one_and_half", "-1.5 keeps the sign bit in NEW_FLOAT_EXT (70)", "(-1.5)", "ok <<131,70,191,248,0,0,0,0,0,0>>"),
        v("wire_extra/fl_decode_70_one", "a fixed NEW_FLOAT_EXT (70) image for 1.0 decodes back to the double", "binary_to_term(<<131,70,63,240,0,0,0,0,0,0>>)", "ok 1.0"),
        v("wire_extra/fl_roundtrip_subnormal", "the smallest subnormal double round-trips through NEW_FLOAT_EXT (70)", "binary_to_term(term_to_binary(1.0e-320)) =:= 1.0e-320", "ok true"),
        verr("wire_extra/err_float_ext_noncanonical_badarg", "a legacy ASCII FLOAT_EXT (99) image that is not a canonical %.20e rendering ('1.5' NUL-padded) is refused with badarg", "binary_to_term(<<131,99,49,46,53,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0>>)"),
        // -- atoms: the empty atom across the three decode tags, the 16-bit-length
        // ATOM_UTF8_EXT (118) boundary a 255-code-point multi-byte name crosses, and the
        // node-atom decode --------------------------------------------------------------
        v("wire_extra/at_decode_100_empty", "a legacy zero-length ATOM_EXT (100) blob decodes to the empty atom", "binary_to_term(<<131,100,0,0>>)", "ok ''"),
        v("wire_extra/at_decode_119_empty", "a zero-length SMALL_ATOM_UTF8_EXT (119) blob decodes to the empty atom", "binary_to_term(<<131,119,0>>)", "ok ''"),
        v("wire_extra/at_decode_118_empty", "a zero-length ATOM_UTF8_EXT (118) blob decodes to the empty atom", "binary_to_term(<<131,118,0,0>>)", "ok ''"),
        v("wire_extra/at_255_multibyte_tag_118", "a 255-code-point atom whose UTF-8 name is 510 bytes overflows the 8-bit length and uses ATOM_UTF8_EXT (118)", "binary:at(term_to_binary(list_to_atom(lists:duplicate(255,945))),1)", "ok 118"),
        v("wire_extra/at_255_multibyte_size", "the 510-byte atom image is the tag, a 16-bit length and the 510 name bytes", "byte_size(term_to_binary(list_to_atom(lists:duplicate(255,945))))", "ok 514"),
        v("wire_extra/at_255_multibyte_roundtrip", "the 510-byte (255-code-point) atom name round-trips", "binary_to_term(term_to_binary(list_to_atom(lists:duplicate(255,945)))) =:= list_to_atom(lists:duplicate(255,945))", "ok true"),
        v("wire_extra/at_decode_118_hello_bytes", "a hand-built ATOM_UTF8_EXT (118) blob for hello decodes to the atom's character list", "atom_to_list(binary_to_term(<<131,118,0,5,104,101,108,108,111>>))", "ok [104,101,108,108,111]"),
        v("wire_extra/at_decode_node_atom_is_node", "a hand-built SMALL_ATOM_UTF8_EXT (119) blob spelling the local node decodes to erlang:node/0", "binary_to_term(<<131,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>) =:= node()", "ok true"),
        // -- lists / strings: the NIL_EXT byte golden, the 65535-byte STRING_EXT (107) →
        // 65536-byte LIST_EXT (108) boundary, and the zero-length LIST_EXT decode -----------
        t("wire_extra/li_nil_bytes", "the empty list is the two-byte NIL_EXT (106) image", "[]", "ok <<131,106>>"),
        v("wire_extra/li_string_65535_tag", "a 65535-byte byte list is the largest STRING_EXT (107)", "binary:at(term_to_binary(lists:duplicate(65535,$a)),1)", "ok 107"),
        v("wire_extra/li_string_65536_tag", "a 65536-byte byte list overflows the 16-bit STRING_EXT length and becomes LIST_EXT (108)", "binary:at(term_to_binary(lists:duplicate(65536,$a)),1)", "ok 108"),
        v("wire_extra/li_string_65535_size", "a 65535-byte STRING_EXT image is the tag, a 16-bit length and the bytes", "byte_size(term_to_binary(lists:duplicate(65535,$a)))", "ok 65539"),
        v("wire_extra/li_string_65536_roundtrip", "a 65536-element byte list round-trips through LIST_EXT (108)", "binary_to_term(term_to_binary(lists:duplicate(65536,$a))) =:= lists:duplicate(65536,$a)", "ok true"),
        v("wire_extra/li_decode_108_empty_list_eq", "a hand-built zero-length LIST_EXT (108) with a NIL_EXT tail decodes to the empty list", "binary_to_term(<<131,108,0,0,0,0,106>>)", "ok []"),
        // -- tuples: the empty-tuple byte golden, a LARGE_TUPLE_EXT (105) decode, the
        // arity/element-count consistency rule, and the 256-element LARGE_TUPLE round trip --
        t("wire_extra/tu_empty_bytes", "the empty tuple encodes as a zero-arity SMALL_TUPLE_EXT (104)", "{}", "ok <<131,104,0>>"),
        v("wire_extra/tu_decode_105_arity1_eq", "a hand-built LARGE_TUPLE_EXT (105) blob of arity 1 decodes to its 1-tuple", "binary_to_term(<<131,105,0,0,0,1,119,1,97>>)", "ok {a}"),
        verr("wire_extra/err_tuple_arity_mismatch_badarg", "a SMALL_TUPLE_EXT (104) blob whose declared arity (255) exceeds the elements present is refused with badarg", "binary_to_term(<<131,104,255,119,1,97>>)"),
        v("wire_extra/tu_arity257_roundtrip_size", "a 257-element tuple (a LARGE_TUPLE_EXT 105) round-trips", "tuple_size(binary_to_term(term_to_binary(list_to_tuple(lists:duplicate(257,x)))))", "ok 257"),
        v("wire_extra/tu_arity300_tag", "a 300-element tuple encodes with the LARGE_TUPLE_EXT (105) tag", "binary:at(term_to_binary(list_to_tuple(lists:duplicate(300,x))),1)", "ok 105"),
        v("wire_extra/tu_decode_105_is_tuple", "a hand-built LARGE_TUPLE_EXT (105) blob decodes to a tuple", "is_tuple(binary_to_term(<<131,105,0,0,0,2,97,1,97,2>>))", "ok true"),
        // -- binaries / bitstrings: a zero-valued 1-bit image, a 7-bit decode, and the
        // bit-count arithmetic of a 12-bit image ------------------------------------------
        t("wire_extra/bit_zero_bit_one_size", "a 1-bit bitstring with a zero value encodes as BIT_BINARY_EXT (77)", "<<0:1>>", "ok <<131,77,0,0,0,1,1,0>>"),
        v("wire_extra/bit_decode_77_seven", "a fixed 7-bit BIT_BINARY_EXT (77) blob decodes to the bitstring it carries", "binary_to_term(<<131,77,0,0,0,1,7,2>>)", "ok <<1:7>>"),
        v("wire_extra/bit_decode_77_twelve_bits_size", "a 12-bit BIT_BINARY_EXT (77) image decodes to a 12-bit bitstring (2 bytes minus 4 spare bits)", "bit_size(binary_to_term(<<131,77,0,0,0,2,4,0,80>>))", "ok 12"),
        // -- maps: a single float-keyed byte golden (a non-atom key is VM-stable), and the
        // 32 → 33 pair flatmap → HAMT boundary --------------------------------------------
        t("wire_extra/ma_single_float_key", "a single float-keyed map encodes as MAP_EXT (116) with a NEW_FLOAT_EXT key", "#{1.5 => a}", "ok <<131,116,0,0,0,1,70,63,248,0,0,0,0,0,0,119,1,97>>"),
        v("wire_extra/ma_33_keys_hamt_size", "a 33-key map (one past the flatmap ceiling) keeps its size across the round trip", "maps:size(binary_to_term(term_to_binary(maps:from_list([{N,N}||N<-lists:seq(1,33)]))))", "ok 33"),
        v("wire_extra/ma_33_keys_hamt_roundtrip", "a 33-key hash-map round-trips", "binary_to_term(term_to_binary(maps:from_list([{N,N}||N<-lists:seq(1,33)]))) =:= maps:from_list([{N,N}||N<-lists:seq(1,33)])", "ok true"),
        v("wire_extra/ma_40_keys_sorted", "a 40-key map's integer keys come back sorted after the round trip", "lists:sort(maps:keys(binary_to_term(term_to_binary(maps:from_list([{N,N}||N<-lists:seq(1,40)])))))", "ok [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40]"),
        // -- funs: `erlang:abs/1` as an EXPORT_EXT (113) byte golden, and the legacy
        // ATOM_EXT (100) module/function atoms of an `< 2` minor-version external fun ------
        v("wire_extra/fun_export_abs_encode", "the external fun erlang:abs/1 encodes as EXPORT_EXT (113)", "term_to_binary(fun erlang:abs/1)", "ok <<131,113,119,6,101,114,108,97,110,103,119,3,97,98,115,97,1>>"),
        v("wire_extra/fun_export_abs_roundtrip_eq", "the external fun erlang:abs/1 round-trips", "binary_to_term(term_to_binary(fun erlang:abs/1)) =:= fun erlang:abs/1", "ok true"),
        v("wire_extra/fun_export_mv1_reverse_bytes", "an external fun under {minor_version,1} encodes its module/function atoms as the legacy ATOM_EXT (100)", "term_to_binary(fun lists:reverse/1, [{minor_version,1}])", "ok <<131,113,100,0,5,108,105,115,116,115,100,0,7,114,101,118,101,114,115,101,97,1>>"),
        v("wire_extra/fun_export_mv1_reverse_roundtrip", "an external fun encoded under {minor_version,1} round-trips", "binary_to_term(term_to_binary(fun lists:reverse/1, [{minor_version,1}])) =:= fun lists:reverse/1", "ok true"),
        // -- `{minor_version, _}`: the legacy atoms/float dialect over a compound term, the
        // node atom under `{minor_version,1}`, and `{minor_version,2}` == the default ------
        v("wire_extra/mv0_tuple_legacy_atom_bytes", "term_to_binary/2 with {minor_version,0} encodes a compound term's atom as the legacy ATOM_EXT (100)", "term_to_binary({hello,1}, [{minor_version,0}])", "ok <<131,104,2,100,0,5,104,101,108,108,111,97,1>>"),
        v("wire_extra/mv0_tuple_roundtrip_eq", "a {minor_version,0} compound image round-trips", "binary_to_term(term_to_binary({hello,1}, [{minor_version,0}])) =:= {hello,1}", "ok true"),
        v("wire_extra/mv1_atom_legacy_bytes", "term_to_binary/2 with {minor_version,1} encodes an atom as the 16-bit-length ATOM_EXT (100)", "term_to_binary(foo, [{minor_version,1}])", "ok <<131,100,0,3,102,111,111>>"),
        v("wire_extra/mv2_control_tuple_bytes", "term_to_binary/2 with {minor_version,2} encodes a control tuple byte-identically to the default encoding", "term_to_binary({2, node()}, [{minor_version,2}])", "ok <<131,104,2,97,2,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        v("wire_extra/mv1_node_atom_legacy_bytes", "term_to_binary/2 with {minor_version,1} encodes the 13-byte node atom as the legacy ATOM_EXT (100)", "term_to_binary(node(), [{minor_version,1}])", "ok <<131,100,0,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
        v("wire_extra/mv2_list_default_exact_eq", "the {minor_version,2} image equals the default image byte for byte for a long list", "term_to_binary(lists:seq(1,10), [{minor_version,2}]) =:= term_to_binary(lists:seq(1,10))", "ok true"),
        // -- compression: the no-op path on the two smallest terms, the `[used]` term shape
        // over a compressed image, and the compressed + minor-version combination ----------
        v("wire_extra/co_empty_list_noop_bytes", "the empty list is too small to gain, so [compressed] leaves it as the NIL_EXT image", "term_to_binary([], [compressed])", "ok <<131,106>>"),
        v("wire_extra/co_small_int_noop_bytes", "a small integer is left uncompressed, so [compressed] is the SMALL_INTEGER_EXT image", "term_to_binary(1, [compressed])", "ok <<131,97,1>>"),
        v("wire_extra/co_used_term_matches", "a compressed image decoded under [used] still yields the original term", "element(1, binary_to_term(term_to_binary(lists:duplicate(1000,$a), [compressed]), [used])) =:= lists:duplicate(1000,$a)", "ok true"),
        v("wire_extra/co_mv2_tuple_roundtrip_eq", "the combined [compressed, {minor_version,2}] options round-trip a tuple", "binary_to_term(term_to_binary({1,2,3}, [compressed, {minor_version,2}])) =:= {1,2,3}", "ok true"),
        // -- the remaining refusal shapes: the DIST_HEADER (68) / removed FUN (117) /
        // NEW_CACHE (78) / unknown tag 0 tags, the corrupt-zlib and empty COMPRESSED body,
        // and the legacy tiny-creation / MAX_REFERENCE / MAX_REF_IDS bounds -----------------
        verr("wire_extra/err_dist_header_tag_badarg", "the DIST_HEADER (68) tag is a distribution framing tag, not a term, so binary_to_term/1 refuses it with badarg", "binary_to_term(<<131,68,0,0,0,0>>)"),
        verr("wire_extra/err_fun_ext_removed_badarg", "the FUN_EXT (117) tag was removed in OTP 23 and is refused with badarg", "binary_to_term(<<131,117,0>>)"),
        verr("wire_extra/err_new_cache_tag_badarg", "the dead NEW_CACHE (78) tag is refused with badarg", "binary_to_term(<<131,78,0>>)"),
        verr("wire_extra/err_tag_zero_badarg", "the unassigned tag byte 0 is refused with badarg", "binary_to_term(<<131,0>>)"),
        verr("wire_extra/err_compressed_bad_zlib_badarg", "a COMPRESSED (80) image whose body is not a zlib stream is refused with badarg", "binary_to_term(<<131,80,0,0,0,3,104,101,108>>)"),
        verr("wire_extra/err_compressed_empty_body_badarg", "a COMPRESSED (80) image with a zero declared size and no body is refused with badarg", "binary_to_term(<<131,80,0,0,0,0>>)"),
        verr("wire_extra/err_pid_ext_creation_four_badarg", "the legacy PID_EXT (103) creation byte must be tiny (<= 3), so creation 4 is refused with badarg", "binary_to_term(<<131,103,100,0,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0,4>>)"),
        verr("wire_extra/err_port_ext_creation_four_badarg", "the legacy PORT_EXT (102) creation byte must be tiny (<= 3), so creation 4 is refused with badarg", "binary_to_term(<<131,102,100,0,5,111,116,104,101,114,0,0,0,7,4>>)"),
        verr("wire_extra/err_reference_ext_first_id_badarg", "a legacy REFERENCE_EXT (101) whose first id is >= MAX_REFERENCE (2^18) is refused with badarg", "binary_to_term(<<131,101,100,0,5,111,116,104,101,114,0,4,0,0,0>>)"),
        verr("wire_extra/err_newer_reference_too_many_ids_badarg", "a NEWER_REFERENCE_EXT (90) with more than MAX_REF_IDS (5) id words is refused with badarg", "binary_to_term(<<131,90,0,6,100,0,5,111,116,104,101,114,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0>>)"),
        // -- the remaining distribution control-message payload tuples (`DOP_*`): the
        // trace-token (TT) and sender / payload variants, all-atom byte goldens where the
        // shape carries no id, and shape/tag predicates (tuple_size, element type, decode ->
        // re-encode identity) where it carries a pid ----------------------------------------
        v("wire_extra/dist_monitor_p_exit_tag_21_atom_bytes", "the DOP_MONITOR_P_EXIT (21) control tuple {21, From, To, Ref, Reason} with atoms encodes byte-exactly", "term_to_binary({21, foo, bar, baz, noproc})", "ok <<131,104,5,97,21,119,3,102,111,111,119,3,98,97,114,119,3,98,97,122,119,6,110,111,112,114,111,99>>"),
        v("wire_extra/dist_send_tt_tag_12_atom_bytes", "the DOP_SEND_TT (12) control tuple {12, From, To, Token, Msg} with atoms encodes byte-exactly", "term_to_binary({12, foo, bar, baz, hello})", "ok <<131,104,5,97,12,119,3,102,111,111,119,3,98,97,114,119,3,98,97,122,119,5,104,101,108,108,111>>"),
        v("wire_extra/dist_payload_tag_24_atom_bytes", "the DOP_PAYLOAD (24) control tuple {24, From, To, Token, Msg} with atoms encodes byte-exactly", "term_to_binary({24, foo, bar, baz, hello})", "ok <<131,104,5,97,24,119,3,102,111,111,119,3,98,97,114,119,3,98,97,122,119,5,104,101,108,108,111>>"),
        v("wire_extra/dist_send_tt_tag_12_tuple_size", "the DOP_SEND_TT (12) control tuple {12, From, To, Token, Msg} is a 5-tuple", "tuple_size(binary_to_term(term_to_binary({12, self(), self(), [], hello})))", "ok 5"),
        v("wire_extra/dist_send_tt_tag_12_reencode_identity", "a DOP_SEND_TT (12) control tuple's decode -> re-encode is byte-identical", "term_to_binary(binary_to_term(term_to_binary({12, self(), self(), [], hello}))) =:= term_to_binary({12, self(), self(), [], hello})", "ok true"),
        v("wire_extra/dist_exit_tt_tag_13_reason_element", "the DOP_EXIT_TT (13) control tuple's reason field round-trips", "element(5, binary_to_term(term_to_binary({13, self(), self(), [], killed})))", "ok killed"),
        v("wire_extra/dist_reg_send_tt_tag_16_name_element", "the DOP_REG_SEND_TT (16) control tuple's registered-name field round-trips", "element(3, binary_to_term(term_to_binary({16, self(), world, [], hello})))", "ok world"),
        v("wire_extra/dist_exit2_tt_tag_18_reason_element", "the DOP_EXIT2_TT (18) control tuple's reason field round-trips", "element(5, binary_to_term(term_to_binary({18, self(), self(), [], normal})))", "ok normal"),
        v("wire_extra/dist_send_sender_tag_22_from_is_pid", "the DOP_SEND_SENDER (22) control tuple's sender field decodes to a pid", "is_pid(element(2, binary_to_term(term_to_binary({22, self(), self(), hello}))))", "ok true"),
        v("wire_extra/dist_send_sender_tt_tag_23_tuple_size", "the DOP_SEND_SENDER_TT (23) control tuple {23, From, To, Token, Msg} is a 5-tuple", "tuple_size(binary_to_term(term_to_binary({23, self(), self(), [], hello})))", "ok 5"),
        v("wire_extra/dist_payload_tag_24_tuple_size", "the DOP_PAYLOAD (24) control tuple {24, From, To, Token, Msg} is a 5-tuple", "tuple_size(binary_to_term(term_to_binary({24, self(), self(), [], hello})))", "ok 5"),
        v("wire_extra/dist_payload_send_tag_29_tuple_size", "the DOP_PAYLOAD_SEND (29) control tuple {29, From, To, Msg} is a 4-tuple", "tuple_size(binary_to_term(term_to_binary({29, self(), self(), hello})))", "ok 4"),
        // -- external ids: the node of a decoded remote NEW_PID_EXT (88), and the legacy
        // REFERENCE_EXT (101) -> NEWER_REFERENCE_EXT (90) re-encode ------------------------
        v("wire_extra/exp_id_node_of_external_pid", "erlang:node/1 of a decoded remote NEW_PID_EXT (88) names the node atom that image carries", "node(binary_to_term(<<131,88,119,7,102,111,111,64,98,97,114,0,0,0,10,0,0,0,0,0,0,0,0>>))", "ok foo@bar"),
        v("wire_extra/exp_id_reference_ext_rereencode_bytes", "a legacy REFERENCE_EXT (101) blob decodes and re-encodes as the equivalent NEWER_REFERENCE_EXT (90) image", "term_to_binary(binary_to_term(<<131,101,119,7,102,111,111,64,98,97,114,0,0,0,1,0>>))", "ok <<131,90,0,1,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1>>"),
        // -- `term_to_binary/2` `{minor_version, N}`: the dialect's effect on the tag set --
        // `N >= 2` writes the UTF-8 atom tags (`SMALL_ATOM_UTF8_EXT` 119 / `ATOM_UTF8_EXT`
        // 118) and `NEW_FLOAT_EXT` (70); `N <= 1` writes the legacy `ATOM_EXT` (100) /
        // ASCII `FLOAT_EXT` (99) forms — **wherever** an atom or float sits, including a
        // map's keys and a list's elements. Every other tag is untouched by the dialect,
        // so the pid/port/reference/bignum/binary images are unchanged. The byte goldens
        // below pin the atom-tag switch; the identity cases pin the tags it must NOT move.
        v("wire_extra/mv2_atom_utf8_bytes", "{minor_version,2} encodes a small atom as SMALL_ATOM_UTF8_EXT (119)", "term_to_binary(hello, [{minor_version, 2}])", "ok <<131,119,5,104,101,108,108,111>>"),
        v("wire_extra/mv1_map_legacy_atom_key_bytes", "{minor_version,1} encodes a map's atom key as the legacy ATOM_EXT (100)", "term_to_binary(#{a => 1}, [{minor_version, 1}])", "ok <<131,116,0,0,0,1,100,0,1,97,97,1>>"),
        v("wire_extra/mv0_map_legacy_atom_key_bytes", "{minor_version,0} also encodes a map's atom key as the legacy ATOM_EXT (100)", "term_to_binary(#{a => 1}, [{minor_version, 0}])", "ok <<131,116,0,0,0,1,100,0,1,97,97,1>>"),
        v("wire_extra/mv0_list_legacy_atom_bytes", "{minor_version,0} encodes a list of atoms with legacy ATOM_EXT (100) elements", "term_to_binary([a,b],[{minor_version,0}])", "ok <<131,108,0,0,0,2,100,0,1,97,100,0,1,98,106>>"),
        v("wire_extra/mv1_list_legacy_atom_bytes", "{minor_version,1} also encodes a list's atoms as legacy ATOM_EXT (100)", "term_to_binary([a,b],[{minor_version,1}])", "ok <<131,108,0,0,0,2,100,0,1,97,100,0,1,98,106>>"),
        v("wire_extra/mv2_list_utf8_atom_bytes", "{minor_version,2} encodes a list's atoms as SMALL_ATOM_UTF8_EXT (119)", "term_to_binary([a,b],[{minor_version,2}])", "ok <<131,108,0,0,0,2,119,1,97,119,1,98,106>>"),
        v("wire_extra/mv1_nonlatin_atom_bytes", "{minor_version,1} keeps a non-latin1 atom's UTF-8 bytes under SMALL_ATOM_UTF8_EXT (119)", "term_to_binary(list_to_atom([945]), [{minor_version, 1}])", "ok <<131,119,2,206,177>>"),
        t("wire_extra/mv0_bignum_identity", "minor_version 0 leaves a bignum image unchanged", "begin T = 12345678901234567890, term_to_binary(T,[{minor_version,0}]) =:= term_to_binary(T) end", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/mv0_binary_identity", "minor_version 0 leaves a binary image unchanged", "begin B = <<1,2,3>>, term_to_binary(B,[{minor_version,0}]) =:= term_to_binary(B) end", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/mv2_ref_identity", "minor_version 2 leaves a local reference image unchanged", "begin Rf = make_ref(), term_to_binary(Rf,[{minor_version,2}]) =:= term_to_binary(Rf) end", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/mv2_external_pid_identity", "minor_version 2 leaves an external pid image unchanged", "begin P = binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>), term_to_binary(P,[{minor_version,2}]) =:= term_to_binary(P) end", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/mv2_internal_port_identity", "minor_version 2 leaves an internal port image unchanged", "begin P = open_port({spawn,\"cat\"},[binary]), Same = (term_to_binary(P,[{minor_version,2}]) =:= term_to_binary(P)), port_close(P), Same end", "ok <<131,119,4,116,114,117,101>>"),
        // -- `term_to_binary/2` option refusal matrix + the framing identities -----------
        // Each option is validated before the term is encoded: an out-of-range or
        // mistyped `{minor_version, _}` / `{compressed, _}`, a wrapped option, and a
        // wrong-shaped option list are all `badarg`. A repeated option is last-wins, and
        // the COMPRESSED (80) header's declared 32-bit size is the *body* size (the plain
        // image minus `VERSION_MAGIC`), which is pinned as a `{Declared, Plain}` pair —
        // the zlib middle stays unpinned because each encoder chooses its own bytes.
        verr("wire_extra/err_t2b_minor_version_three_badarg", "{minor_version,3} is above the supported maximum (2)", "term_to_binary(1,[{minor_version,3}])"),
        verr("wire_extra/err_t2b_minor_version_negative_badarg", "a negative {minor_version,-1} is badarg", "term_to_binary(1,[{minor_version,-1}])"),
        verr("wire_extra/err_t2b_minor_version_float_badarg", "a float {minor_version,1.0} is badarg", "term_to_binary(1,[{minor_version,1.0}])"),
        verr("wire_extra/err_t2b_compressed_level_ten_badarg", "{compressed,10} is above the zlib maximum (9)", "term_to_binary(1,[{compressed,10}])"),
        verr("wire_extra/err_t2b_compressed_level_negative_badarg", "a negative {compressed,-1} is badarg", "term_to_binary(1,[{compressed,-1}])"),
        verr("wire_extra/err_t2b_compressed_level_float_badarg", "a float {compressed,1.0} is badarg", "term_to_binary(1,[{compressed,1.0}])"),
        verr("wire_extra/err_t2b_option_not_list_badarg", "term_to_binary/2 with a non-list option is badarg", "term_to_binary(1,bogus)"),
        verr("wire_extra/err_t2b_unknown_option_badarg", "an unknown term_to_binary/2 option is badarg", "term_to_binary(1,[bogus])"),
        verr("wire_extra/err_t2b_option_nested_list_badarg", "a wrapped option ({minor_version,1} inside a list) is badarg", "term_to_binary(1,[[minor_version,1]])"),
        verr("wire_extra/err_t2b_minor_version_missing_value_badarg", "{minor_version} with no value is badarg", "term_to_binary(1,[{minor_version}])"),
        verr("wire_extra/err_t2b_minor_version_arity_badarg", "a three-element {minor_version,1,2} option is badarg", "term_to_binary(1,[{minor_version,1,2}])"),
        v("wire_extra/opt_option_repeat_last_wins", "a repeated option keeps the last occurrence", "begin T = hello, term_to_binary(T,[{minor_version,0},{minor_version,2}]) =:= term_to_binary(T,[{minor_version,2}]) end", "ok true"),
        v("wire_extra/opt_option_repeat_last_wins_bytes", "the last repeated minor_version wins, so the atom uses the UTF-8 tag", "term_to_binary(hello,[{minor_version,0},{minor_version,2}])", "ok <<131,119,5,104,101,108,108,111>>"),
        v("wire_extra/opt_compressed_declared_size_pair", "the COMPRESSED (80) header declares the uncompressed body size (one less than the plain image) for a 1000-atom list", "begin T = lists:duplicate(1000, a), U = byte_size(term_to_binary(T)), <<131,80,Sz:32,_/binary>> = term_to_binary(T,[compressed]), {Sz, U} end", "ok {3006,3007}"),
        v("wire_extra/opt_compressed_declared_size_pair_binary", "the COMPRESSED (80) header declares the uncompressed body size for a 5000-byte binary", "begin T = binary:copy(<<0>>,5000), U = byte_size(term_to_binary(T)), <<131,80,Sz:32,_/binary>> = term_to_binary(T,[compressed]), {Sz, U} end", "ok {5005,5006}"),
        t("wire_extra/opt_compressed_level_zero_roundtrip", "the {compressed,0} option round-trips a 2000-byte body", "binary_to_term(term_to_binary(lists:seq(1,2000),[{compressed,0}])) =:= lists:seq(1,2000)", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/opt_compressed_level_two_roundtrip", "the {compressed,2} option round-trips a 2000-byte body", "binary_to_term(term_to_binary(lists:seq(1,2000),[{compressed,2}])) =:= lists:seq(1,2000)", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/opt_compressed_level_six_roundtrip", "the {compressed,6} option round-trips a 2000-byte body", "binary_to_term(term_to_binary(lists:seq(1,2000),[{compressed,6}])) =:= lists:seq(1,2000)", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/opt_compressed_body_binary_roundtrip", "a 5000-byte all-zero binary round-trips through the compressed option", "binary_to_term(term_to_binary(binary:copy(<<0>>,5000),[compressed])) =:= binary:copy(<<0>>,5000)", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/opt_compressed_body_atom_list_roundtrip", "a 300-element repeated-atom list round-trips through the compressed option", "binary_to_term(term_to_binary(lists:duplicate(300,alpha),[compressed])) =:= lists:duplicate(300,alpha)", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/opt_compressed_body_map_roundtrip", "a 50-key integer map round-trips through the compressed option", "binary_to_term(term_to_binary(maps:from_list([{I, I*I} || I <- lists:seq(1,50)]),[compressed])) =:= maps:from_list([{I, I*I} || I <- lists:seq(1,50)])", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/opt_compressed_mv1_roundtrip", "the [compressed, {minor_version,1}] pair round-trips a 500-element list", "binary_to_term(term_to_binary(lists:seq(1,500),[compressed,{minor_version,1}])) =:= lists:seq(1,500)", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/opt_compressed_mv1_differs_from_mv2", "the [compressed, {minor_version,1}] image of an atom list differs from the [compressed, {minor_version,2}] image", "begin T = lists:duplicate(500, alpha), term_to_binary(T,[compressed,{minor_version,1}]) =/= term_to_binary(T,[compressed,{minor_version,2}]) end", "ok true"),
        // -- `binary_to_term/2` over those images: the `[safe]` / `[used]` inverse --------
        v("wire_extra/b2t_used_mv1_atom_bytes", "[used] on a {minor_version,1} atom image reports the atom and its 9 consumed bytes", "binary_to_term(term_to_binary(hello,[{minor_version,1}]),[used])", "ok {hello,9}"),
        v("wire_extra/b2t_safe_used_mv1_atom_bytes", "[safe,used] on a {minor_version,1} atom image reports the atom and its 9 consumed bytes", "binary_to_term(term_to_binary(hello,[{minor_version,1}]),[safe,used])", "ok {hello,9}"),
        v("wire_extra/b2t_safe_export_fun_image", "[safe] accepts an EXPORT_EXT (113) external-fun image", "binary_to_term(term_to_binary(fun lists:reverse/1), [safe])", "ok fun lists:reverse/1"),
        t("wire_extra/b2t_safe_export_fun_is_function1", "[safe] on an external-fun image yields a callable fun of arity 1", "is_function(binary_to_term(term_to_binary(fun lists:reverse/1), [safe]), 1)", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/b2t_safe_export_fun_call", "[safe] on an external-fun image yields a fun that calls through", "begin F = binary_to_term(term_to_binary(fun lists:reverse/1), [safe]), F([1,2,3]) end", "ok [3,2,1]"),
        t("wire_extra/b2t_used_compressed_bytes_is_full_image", "[used] over a compressed image reports the whole image's byte count", "begin T = lists:duplicate(200, 122), B = term_to_binary(T,[compressed]), {T2,U} = binary_to_term(B,[used]), (T2 =:= T) andalso (U =:= byte_size(B)) end", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/b2t_safe_external_pid_image_accept", "[safe] accepts an external NEW_PID_EXT (88) image", "is_pid(binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>, [safe]))", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/b2t_safe_external_ref_image_accept", "[safe] accepts an external NEWER_REFERENCE_EXT (90) image", "is_reference(binary_to_term(<<131,90,0,1,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202,0,0,0,7>>, [safe]))", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/b2t_empty_options_same_as_one_arg", "binary_to_term/2 with an empty option list behaves as binary_to_term/1", "binary_to_term(<<131,97,1>>, [])", "ok 1"),
        verr("wire_extra/err_b2t_option_not_list_badarg", "binary_to_term/2 with a non-list option is badarg", "binary_to_term(<<131,97,1>>, used)"),
        // -- decode-side tag gaps: the shapes an encode-only case never emits ------------
        // The 64-bit id of a `V4_PORT_EXT` (120) and the 32-bit id of a `NEW_PORT_EXT` (89)
        // (see the decoder in `external.c`), the tiny-creation ceiling of the legacy
        // `PORT_EXT` (102) / `PID_EXT` (103) / `REFERENCE_EXT` (101) forms (they upgrade on
        // re-encode to the modern tag), the ignored bytes after a declared id count, the
        // `LIST_EXT` integer tail, a `STRING_EXT` byte above 127, and the `BIT_BINARY_EXT`
        // (77) bit-count bounds.
        v("wire_extra/dec_list_ext_zero_elements_int_tail", "a LIST_EXT (108) with zero elements and an integer tail decodes to that integer", "binary_to_term(<<131,108,0,0,0,0,97,9>>)", "ok 9"),
        v("wire_extra/dec_string_ext_byte_200", "a STRING_EXT (107) byte above 127 decodes to that code point", "binary_to_term(<<131,107,0,1,200>>)", "ok [200]"),
        t("wire_extra/dec_string_ext_abc_is_string", "a STRING_EXT (107) body decodes to a list of the same code points", "binary_to_term(<<131,107,0,3,97,98,99>>) =:= [97,98,99]", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/dec_newer_reference_declared_five_ignores_trailing_sixth", "a NEWER_REFERENCE_EXT (90) declaring five ids re-encodes the five ids and ignores a trailing sixth", "term_to_binary(binary_to_term(<<131,90,0,5,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4,0,0,0,5>>))", "ok <<131,90,0,5,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4>>"),
        t("wire_extra/dec_newer_reference_declared_five_rereencode_size", "the five-id NEWER_REFERENCE_EXT re-encode is 35 bytes", "byte_size(term_to_binary(binary_to_term(<<131,90,0,5,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4,0,0,0,5>>)))", "ok <<131,97,35>>"),
        v("wire_extra/dec_new_pid_ext_creation_three_rereencode", "a NEW_PID_EXT (88) with creation 3 re-encodes byte-identically", "term_to_binary(binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>))", "ok <<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>"),
        t("wire_extra/dec_new_pid_ext_creation_four_is_pid", "a NEW_PID_EXT (88) with the 32-bit creation field 4 decodes to a pid", "is_pid(binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,4>>))", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/dec_new_pid_ext_serial_256_is_pid", "a NEW_PID_EXT (88) with serial 256 decodes to a pid", "is_pid(binary_to_term(<<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,1,0,0,0,0,3>>))", "ok <<131,119,4,116,114,117,101>>"),
        t("wire_extra/dec_new_pid_ext_max_id_is_pid", "a NEW_PID_EXT (88) with the largest 32-bit id and serial decodes to a pid", "is_pid(binary_to_term(<<131,88,119,5,111,116,104,101,114,255,255,255,255,255,255,255,255,0,0,0,3>>))", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/dec_legacy_pid_ext_creation_three_upgrades", "a legacy PID_EXT (103) with creation 3 re-encodes as NEW_PID_EXT (88)", "term_to_binary(binary_to_term(<<131,103,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,3>>))", "ok <<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>"),
        v("wire_extra/dec_legacy_port_ext_creation_three_upgrades", "a legacy PORT_EXT (102) with creation 3 (the tiny creation ceiling) re-encodes as V4_PORT_EXT (120)", "term_to_binary(binary_to_term(<<131,102,119,5,111,116,104,101,114,0,0,0,7,3>>))", "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,3>>"),
        v("wire_extra/dec_legacy_reference_ext_creation_three_upgrades", "a legacy REFERENCE_EXT (101) with creation 3 re-encodes as NEWER_REFERENCE_EXT (90)", "term_to_binary(binary_to_term(<<131,101,119,5,111,116,104,101,114,0,0,0,7,3>>))", "ok <<131,90,0,1,119,5,111,116,104,101,114,0,0,0,3,0,0,0,7>>"),
        t("wire_extra/dec_newer_port_ext_v4_creation_256_is_port", "a V4_PORT_EXT (120) with a 64-bit id and a 32-bit creation of 256 decodes to a port", "is_port(binary_to_term(<<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,1,0>>))", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/dec_newer_port_ext_v4_rereencode", "a V4_PORT_EXT (120) with a 64-bit id re-encodes byte-identically", "term_to_binary(binary_to_term(<<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,1,0>>))", "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,1,0>>"),
        t("wire_extra/dec_new_port_ext_creation_256_is_port", "a NEW_PORT_EXT (89) with a 32-bit id and creation 256 decodes to a port", "is_port(binary_to_term(<<131,89,119,5,111,116,104,101,114,0,0,0,7,0,0,1,0>>))", "ok <<131,119,4,116,114,117,101>>"),
        v("wire_extra/dec_new_port_ext_creation_three_upgrades_to_v4", "a NEW_PORT_EXT (89) with creation 3 re-encodes as V4_PORT_EXT (120)", "term_to_binary(binary_to_term(<<131,89,119,5,111,116,104,101,114,0,0,0,7,0,0,0,3>>))", "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,3>>"),
        v("wire_extra/dec_bit_binary_ext_eight_bits_is_binary", "a BIT_BINARY_EXT (77) declaring 8 bits decodes to a byte-aligned binary", "binary_to_term(<<131,77,0,0,0,1,8,0>>)", "ok <<0>>"),
        verr("wire_extra/err_bit_binary_ext_nine_bits_badarg", "a BIT_BINARY_EXT (77) declaring 9 bits is badarg", "binary_to_term(<<131,77,0,0,0,1,9,0>>)"),
        verr("wire_extra/err_bit_binary_ext_zero_bits_badarg", "a BIT_BINARY_EXT (77) declaring 0 bits is badarg", "binary_to_term(<<131,77,0,0,0,1,0,0>>)"),
        verr("wire_extra/err_small_atom_utf8_invalid_badarg", "a SMALL_ATOM_UTF8_EXT (119) body that is not valid UTF-8 is badarg", "binary_to_term(<<131,119,2,206,64>>)"),
        verr("wire_extra/err_atom_utf8_invalid_badarg", "an ATOM_UTF8_EXT (118) body that is not valid UTF-8 is badarg", "binary_to_term(<<131,118,0,2,206,64>>)"),
        // -- the remaining `DOP_*` control-message tags --------------------------------
        // `dist.h`'s `enum dop` assigns 1-8 to the classic operations, 12/13/16/18 to the
        // trace-token variants, 19-24 to the monitor / sender / payload-exit family, and
        // 29-37 to the spawn-request / alias-send / unlink-id / altact-signal operations.
        // The members pinned above cover 1-8, 12, 13, 16, 18-24 and 29; these cases close
        // the remaining 25-28 and 30-37. A control tuple is *just an ETF term* here (a live
        // handshake needs two connected nodes — see the module docs), so every payload is
        // chosen from small integers, atoms and the (deterministic) local node atom; the
        // byte/2-tuple goldens pin the tag and the tuple encoding.
        v("wire_extra/dist_payload_exit_tt_tag_25_bytes", "the DOP_PAYLOAD_EXIT_TT (25) control tuple round-trips with the node atoms and atom payloads", "{25, node(), node(), normal, hello}", "ok {25,nonode@nohost,nonode@nohost,normal,hello}"),
        v("wire_extra/dist_payload_exit2_tag_26_bytes", "the DOP_PAYLOAD_EXIT2 (26) control tuple round-trips with the node atoms and atom payloads", "{26, node(), node(), normal, hello}", "ok {26,nonode@nohost,nonode@nohost,normal,hello}"),
        v("wire_extra/dist_payload_exit2_tt_tag_27_bytes", "the DOP_PAYLOAD_EXIT2_TT (27) control tuple round-trips with the node atoms and atom payloads", "{27, node(), node(), normal, hello}", "ok {27,nonode@nohost,nonode@nohost,normal,hello}"),
        v("wire_extra/dist_payload_monitor_p_exit_tag_28_bytes", "the DOP_PAYLOAD_MONITOR_P_EXIT (28) control tuple round-trips with the node atoms and atom payloads", "{28, node(), node(), node(), noproc, hello}", "ok {28,nonode@nohost,nonode@nohost,nonode@nohost,noproc,hello}"),
        v("wire_extra/dist_spawn_request_tt_tag_30_bytes", "the DOP_SPAWN_REQUEST_TT (30) control tuple round-trips with the node atoms and atom payloads", "{30, node(), node(), normal, hello, world, token}", "ok {30,nonode@nohost,nonode@nohost,normal,hello,world,token}"),
        v("wire_extra/dist_spawn_reply_tag_31_tag_and_size", "the DOP_SPAWN_REPLY (31) control tuple keeps its tag and its arity", "{element(1, {31, node(), node(), normal}), tuple_size({31, node(), node(), normal})}", "ok {31,4}"),
        v("wire_extra/dist_spawn_reply_tt_tag_32_tag_and_size", "the DOP_SPAWN_REPLY_TT (32) control tuple keeps its tag and its arity", "{element(1, {32, node(), node(), normal, token}), tuple_size({32, node(), node(), normal, token})}", "ok {32,5}"),
        v("wire_extra/dist_alias_send_tag_33_bytes", "the DOP_ALIAS_SEND (33) control tuple round-trips with the node atoms and atom payloads", "{33, node(), node(), hello}", "ok {33,nonode@nohost,nonode@nohost,hello}"),
        v("wire_extra/dist_alias_send_tt_tag_34_tag_and_size", "the DOP_ALIAS_SEND_TT (34) control tuple keeps its tag and its arity", "{element(1, {34, node(), node(), hello, token}), tuple_size({34, node(), node(), hello, token})}", "ok {34,5}"),
        v("wire_extra/dist_unlink_id_tag_35_bytes", "the DOP_UNLINK_ID (35) control tuple round-trips with the node atoms and atom payloads", "{35, node(), node(), someid}", "ok {35,nonode@nohost,nonode@nohost,someid}"),
        v("wire_extra/dist_unlink_id_ack_tag_36_bytes", "the DOP_UNLINK_ID_ACK (36) control tuple round-trips with the node atoms and atom payloads", "{36, node(), node(), someid}", "ok {36,nonode@nohost,nonode@nohost,someid}"),
        v("wire_extra/dist_altact_sig_send_tag_37_tag_and_size", "the DOP_ALTACT_SIG_SEND (37) control tuple keeps its tag and its arity", "{element(1, {37, node(), node(), normal}), tuple_size({37, node(), node(), normal})}", "ok {37,4}"),
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

    #[test]
    fn wire_extra_covers_the_widened_wire_families() {
        let cases = group();
        // The widened A.5 block: the remaining integer/float/atom/list/tuple/binary/map/fun
        // byte goldens, the decode-side hand-built blobs (integer, float, atom, list, tuple,
        // bitstring, external id), the `{minor_version,_}` / `compressed` option identities,
        // the remaining `DOP_*` control tuples and the legacy tiny-creation / MAX_REFERENCE /
        // MAX_REF_IDS refusal shapes. Every golden is the oracle's own line and the Rust
        // runtime reproduces it (each case `agree`s); pin the ids and their goldens here so
        // the agreements cannot regress silently.
        for (id, golden) in [
            ("wire_extra/ie_small_int_zero_bytes", "ok <<131,97,0>>"),
            ("wire_extra/ie_small_int_255_bytes", "ok <<131,97,255>>"),
            ("wire_extra/ie_int_ext_256", "ok <<131,98,0,0,1,0>>"),
            ("wire_extra/ie_int_ext_65535", "ok <<131,98,0,0,255,255>>"),
            ("wire_extra/ie_int_ext_100000", "ok <<131,98,0,1,134,160>>"),
            ("wire_extra/ie_int_ext_neg300", "ok <<131,98,255,255,254,212>>"),
            ("wire_extra/ie_int_ext_neg65536", "ok <<131,98,255,255,0,0>>"),
            ("wire_extra/ie_int_ext_neg100001", "ok <<131,98,255,254,121,95>>"),
            ("wire_extra/ie_small_big_2_pow_32", "ok <<131,110,5,0,0,0,0,0,1>>"),
            ("wire_extra/ie_small_big_neg_2_pow_32", "ok <<131,110,5,1,0,0,0,0,1>>"),
            ("wire_extra/ie_decode_97_zero", "ok 0"),
            ("wire_extra/ie_decode_98_256", "ok 256"),
            ("wire_extra/ie_decode_110_to_97_reencode", "ok <<131,97,42>>"),
            ("wire_extra/ie_decode_110_leading_zero_stripped", "ok 0"),
            ("wire_extra/ie_decode_110_sign_byte_two_is_negative", "ok -5"),
            ("wire_extra/ie_decode_110_neg_zero_magnitude_is_zero", "ok 0"),
            ("wire_extra/ie_decode_111_to_98_reencode", "ok <<131,98,0,0,1,0>>"),
            ("wire_extra/ie_roundtrip_bsl300", "ok true"),
            ("wire_extra/fl_half", "ok <<131,70,63,224,0,0,0,0,0,0>>"),
            ("wire_extra/fl_neg_one_and_half", "ok <<131,70,191,248,0,0,0,0,0,0>>"),
            ("wire_extra/fl_decode_70_one", "ok 1.0"),
            ("wire_extra/fl_roundtrip_subnormal", "ok true"),
            ("wire_extra/at_decode_100_empty", "ok ''"),
            ("wire_extra/at_decode_119_empty", "ok ''"),
            ("wire_extra/at_decode_118_empty", "ok ''"),
            ("wire_extra/at_255_multibyte_tag_118", "ok 118"),
            ("wire_extra/at_255_multibyte_size", "ok 514"),
            ("wire_extra/at_255_multibyte_roundtrip", "ok true"),
            ("wire_extra/at_decode_118_hello_bytes", "ok [104,101,108,108,111]"),
            ("wire_extra/at_decode_node_atom_is_node", "ok true"),
            ("wire_extra/li_nil_bytes", "ok <<131,106>>"),
            ("wire_extra/li_string_65535_tag", "ok 107"),
            ("wire_extra/li_string_65536_tag", "ok 108"),
            ("wire_extra/li_string_65535_size", "ok 65539"),
            ("wire_extra/li_string_65536_roundtrip", "ok true"),
            ("wire_extra/li_decode_108_empty_list_eq", "ok []"),
            ("wire_extra/tu_empty_bytes", "ok <<131,104,0>>"),
            ("wire_extra/tu_decode_105_arity1_eq", "ok {a}"),
            ("wire_extra/tu_arity257_roundtrip_size", "ok 257"),
            ("wire_extra/tu_arity300_tag", "ok 105"),
            ("wire_extra/tu_decode_105_is_tuple", "ok true"),
            ("wire_extra/bit_zero_bit_one_size", "ok <<131,77,0,0,0,1,1,0>>"),
            ("wire_extra/bit_decode_77_seven", "ok <<1:7>>"),
            ("wire_extra/bit_decode_77_twelve_bits_size", "ok 12"),
            ("wire_extra/ma_single_float_key", "ok <<131,116,0,0,0,1,70,63,248,0,0,0,0,0,0,119,1,97>>"),
            ("wire_extra/ma_33_keys_hamt_size", "ok 33"),
            ("wire_extra/ma_33_keys_hamt_roundtrip", "ok true"),
            ("wire_extra/ma_40_keys_sorted", "ok [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33,34,35,36,37,38,39,40]"),
            ("wire_extra/fun_export_abs_encode", "ok <<131,113,119,6,101,114,108,97,110,103,119,3,97,98,115,97,1>>"),
            ("wire_extra/fun_export_abs_roundtrip_eq", "ok true"),
            ("wire_extra/fun_export_mv1_reverse_bytes", "ok <<131,113,100,0,5,108,105,115,116,115,100,0,7,114,101,118,101,114,115,101,97,1>>"),
            ("wire_extra/fun_export_mv1_reverse_roundtrip", "ok true"),
            ("wire_extra/mv0_tuple_legacy_atom_bytes", "ok <<131,104,2,100,0,5,104,101,108,108,111,97,1>>"),
            ("wire_extra/mv0_tuple_roundtrip_eq", "ok true"),
            ("wire_extra/mv1_atom_legacy_bytes", "ok <<131,100,0,3,102,111,111>>"),
            ("wire_extra/mv2_control_tuple_bytes", "ok <<131,104,2,97,2,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/mv1_node_atom_legacy_bytes", "ok <<131,100,0,13,110,111,110,111,100,101,64,110,111,104,111,115,116>>"),
            ("wire_extra/mv2_list_default_exact_eq", "ok true"),
            ("wire_extra/co_empty_list_noop_bytes", "ok <<131,106>>"),
            ("wire_extra/co_small_int_noop_bytes", "ok <<131,97,1>>"),
            ("wire_extra/co_used_term_matches", "ok true"),
            ("wire_extra/co_mv2_tuple_roundtrip_eq", "ok true"),
            ("wire_extra/dist_monitor_p_exit_tag_21_atom_bytes", "ok <<131,104,5,97,21,119,3,102,111,111,119,3,98,97,114,119,3,98,97,122,119,6,110,111,112,114,111,99>>"),
            ("wire_extra/dist_send_tt_tag_12_atom_bytes", "ok <<131,104,5,97,12,119,3,102,111,111,119,3,98,97,114,119,3,98,97,122,119,5,104,101,108,108,111>>"),
            ("wire_extra/dist_payload_tag_24_atom_bytes", "ok <<131,104,5,97,24,119,3,102,111,111,119,3,98,97,114,119,3,98,97,122,119,5,104,101,108,108,111>>"),
            ("wire_extra/dist_send_tt_tag_12_tuple_size", "ok 5"),
            ("wire_extra/dist_send_tt_tag_12_reencode_identity", "ok true"),
            ("wire_extra/dist_exit_tt_tag_13_reason_element", "ok killed"),
            ("wire_extra/dist_reg_send_tt_tag_16_name_element", "ok world"),
            ("wire_extra/dist_exit2_tt_tag_18_reason_element", "ok normal"),
            ("wire_extra/dist_send_sender_tag_22_from_is_pid", "ok true"),
            ("wire_extra/dist_send_sender_tt_tag_23_tuple_size", "ok 5"),
            ("wire_extra/dist_payload_tag_24_tuple_size", "ok 5"),
            ("wire_extra/dist_payload_send_tag_29_tuple_size", "ok 4"),
            ("wire_extra/exp_id_node_of_external_pid", "ok foo@bar"),
            ("wire_extra/exp_id_reference_ext_rereencode_bytes", "ok <<131,90,0,1,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1>>"),
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the widened-wire case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some(golden),
                "case {id} must carry the oracle-derived golden"
            );
            assert_eq!(case.golden.exit_code, 0, "case {id} golden must exit 0");
        }
        // The refusal shapes added by the widened block are badarg.
        for id in [
            "wire_extra/err_float_ext_noncanonical_badarg",
            "wire_extra/err_tuple_arity_mismatch_badarg",
            "wire_extra/err_dist_header_tag_badarg",
            "wire_extra/err_fun_ext_removed_badarg",
            "wire_extra/err_new_cache_tag_badarg",
            "wire_extra/err_tag_zero_badarg",
            "wire_extra/err_compressed_bad_zlib_badarg",
            "wire_extra/err_compressed_empty_body_badarg",
            "wire_extra/err_pid_ext_creation_four_badarg",
            "wire_extra/err_port_ext_creation_four_badarg",
            "wire_extra/err_reference_ext_first_id_badarg",
            "wire_extra/err_newer_reference_too_many_ids_badarg",
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the widened-wire error case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some("exception error badarg"),
                "case {id} must carry the oracle-derived badarg golden"
            );
            assert_eq!(case.golden.exit_code, 1, "case {id} golden must exit 1");
        }
    }

    #[test]
    fn wire_extra_covers_the_widened_options_decode_and_control_families() {
        let cases = group();
        // The third widened A.5 block: the `{minor_version, N}` dialect's effect on the tag
        // set (UTF-8 atom tags above `N >= 2` vs the legacy `ATOM_EXT` form below), the
        // `term_to_binary/2` option refusal matrix, the compressed-body round trips and the
        // COMPRESSED (80) declared-size pairs, the `binary_to_term/2` `[safe]`/`[used]`
        // inverse, the decode-side tag gaps (the `LIST_EXT` integer tail, the `STRING_EXT`
        // high byte, the id-field widths of `V4_PORT_EXT` (120) / `NEW_PORT_EXT` (89), the
        // legacy tiny-creation forms upgrading on re-encode, and the `BIT_BINARY_EXT`
        // bit-count bounds) and the remaining `DOP_*` control tuples 25-28 / 30-37. Every
        // golden is the oracle's own line and the Rust runtime reproduces it (each case
        // `agree`s); pin the ids and their goldens here so the agreements cannot regress
        // silently.
        for (id, golden) in [
            ("wire_extra/b2t_empty_options_same_as_one_arg", "ok 1"),
            ("wire_extra/b2t_safe_export_fun_call", "ok [3,2,1]"),
            ("wire_extra/b2t_safe_export_fun_image", "ok fun lists:reverse/1"),
            ("wire_extra/b2t_safe_export_fun_is_function1", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/b2t_safe_external_pid_image_accept", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/b2t_safe_external_ref_image_accept", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/b2t_safe_used_mv1_atom_bytes", "ok {hello,9}"),
            ("wire_extra/b2t_used_compressed_bytes_is_full_image", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/b2t_used_mv1_atom_bytes", "ok {hello,9}"),
            ("wire_extra/dec_bit_binary_ext_eight_bits_is_binary", "ok <<0>>"),
            ("wire_extra/dec_legacy_pid_ext_creation_three_upgrades", "ok <<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>"),
            ("wire_extra/dec_legacy_port_ext_creation_three_upgrades", "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,3>>"),
            ("wire_extra/dec_legacy_reference_ext_creation_three_upgrades", "ok <<131,90,0,1,119,5,111,116,104,101,114,0,0,0,3,0,0,0,7>>"),
            ("wire_extra/dec_list_ext_zero_elements_int_tail", "ok 9"),
            ("wire_extra/dec_new_pid_ext_creation_four_is_pid", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/dec_new_pid_ext_creation_three_rereencode", "ok <<131,88,119,5,111,116,104,101,114,0,0,0,7,0,0,0,0,0,0,0,3>>"),
            ("wire_extra/dec_new_pid_ext_max_id_is_pid", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/dec_new_pid_ext_serial_256_is_pid", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/dec_new_port_ext_creation_256_is_port", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/dec_new_port_ext_creation_three_upgrades_to_v4", "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,3>>"),
            ("wire_extra/dec_newer_port_ext_v4_creation_256_is_port", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/dec_newer_port_ext_v4_rereencode", "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,1,0>>"),
            ("wire_extra/dec_newer_reference_declared_five_ignores_trailing_sixth", "ok <<131,90,0,5,119,5,111,116,104,101,114,0,0,0,0,0,2,35,202,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4>>"),
            ("wire_extra/dec_newer_reference_declared_five_rereencode_size", "ok <<131,97,35>>"),
            ("wire_extra/dec_string_ext_abc_is_string", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/dec_string_ext_byte_200", "ok [200]"),
            ("wire_extra/dist_alias_send_tag_33_bytes", "ok {33,nonode@nohost,nonode@nohost,hello}"),
            ("wire_extra/dist_alias_send_tt_tag_34_tag_and_size", "ok {34,5}"),
            ("wire_extra/dist_altact_sig_send_tag_37_tag_and_size", "ok {37,4}"),
            ("wire_extra/dist_payload_exit2_tag_26_bytes", "ok {26,nonode@nohost,nonode@nohost,normal,hello}"),
            ("wire_extra/dist_payload_exit2_tt_tag_27_bytes", "ok {27,nonode@nohost,nonode@nohost,normal,hello}"),
            ("wire_extra/dist_payload_exit_tt_tag_25_bytes", "ok {25,nonode@nohost,nonode@nohost,normal,hello}"),
            ("wire_extra/dist_payload_monitor_p_exit_tag_28_bytes", "ok {28,nonode@nohost,nonode@nohost,nonode@nohost,noproc,hello}"),
            ("wire_extra/dist_spawn_reply_tag_31_tag_and_size", "ok {31,4}"),
            ("wire_extra/dist_spawn_reply_tt_tag_32_tag_and_size", "ok {32,5}"),
            ("wire_extra/dist_spawn_request_tt_tag_30_bytes", "ok {30,nonode@nohost,nonode@nohost,normal,hello,world,token}"),
            ("wire_extra/dist_unlink_id_ack_tag_36_bytes", "ok {36,nonode@nohost,nonode@nohost,someid}"),
            ("wire_extra/dist_unlink_id_tag_35_bytes", "ok {35,nonode@nohost,nonode@nohost,someid}"),
            ("wire_extra/mv0_bignum_identity", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/mv0_binary_identity", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/mv0_list_legacy_atom_bytes", "ok <<131,108,0,0,0,2,100,0,1,97,100,0,1,98,106>>"),
            ("wire_extra/mv0_map_legacy_atom_key_bytes", "ok <<131,116,0,0,0,1,100,0,1,97,97,1>>"),
            ("wire_extra/mv1_list_legacy_atom_bytes", "ok <<131,108,0,0,0,2,100,0,1,97,100,0,1,98,106>>"),
            ("wire_extra/mv1_map_legacy_atom_key_bytes", "ok <<131,116,0,0,0,1,100,0,1,97,97,1>>"),
            ("wire_extra/mv1_nonlatin_atom_bytes", "ok <<131,119,2,206,177>>"),
            ("wire_extra/mv2_atom_utf8_bytes", "ok <<131,119,5,104,101,108,108,111>>"),
            ("wire_extra/mv2_external_pid_identity", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/mv2_internal_port_identity", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/mv2_list_utf8_atom_bytes", "ok <<131,108,0,0,0,2,119,1,97,119,1,98,106>>"),
            ("wire_extra/mv2_ref_identity", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_body_atom_list_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_body_binary_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_body_map_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_declared_size_pair", "ok {3006,3007}"),
            ("wire_extra/opt_compressed_declared_size_pair_binary", "ok {5005,5006}"),
            ("wire_extra/opt_compressed_level_six_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_level_two_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_level_zero_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_compressed_mv1_differs_from_mv2", "ok true"),
            ("wire_extra/opt_compressed_mv1_roundtrip", "ok <<131,119,4,116,114,117,101>>"),
            ("wire_extra/opt_option_repeat_last_wins", "ok true"),
            ("wire_extra/opt_option_repeat_last_wins_bytes", "ok <<131,119,5,104,101,108,108,111>>"),
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the widened options/decode/control case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some(golden),
                "case {id} must carry the oracle-derived golden"
            );
            assert_eq!(case.golden.exit_code, 0, "case {id} golden must exit 0");
        }
        // The refusal shapes added by this block — the `term_to_binary/2` option matrix
        // (out-of-range / mistyped / wrapped options), the `binary_to_term/2` option-shape
        // refusal, the `BIT_BINARY_EXT` bit-count bounds and the invalid-UTF-8 atom bodies —
        // are all badarg.
        for id in [
            "wire_extra/err_atom_utf8_invalid_badarg",
            "wire_extra/err_b2t_option_not_list_badarg",
            "wire_extra/err_bit_binary_ext_nine_bits_badarg",
            "wire_extra/err_bit_binary_ext_zero_bits_badarg",
            "wire_extra/err_small_atom_utf8_invalid_badarg",
            "wire_extra/err_t2b_compressed_level_float_badarg",
            "wire_extra/err_t2b_compressed_level_negative_badarg",
            "wire_extra/err_t2b_compressed_level_ten_badarg",
            "wire_extra/err_t2b_minor_version_arity_badarg",
            "wire_extra/err_t2b_minor_version_float_badarg",
            "wire_extra/err_t2b_minor_version_missing_value_badarg",
            "wire_extra/err_t2b_minor_version_negative_badarg",
            "wire_extra/err_t2b_minor_version_three_badarg",
            "wire_extra/err_t2b_option_nested_list_badarg",
            "wire_extra/err_t2b_option_not_list_badarg",
            "wire_extra/err_t2b_unknown_option_badarg",
        ] {
            let case = cases
                .iter()
                .find(|c| c.id == id)
                .unwrap_or_else(|| panic!("expected the widened options/decode error case {id}"));
            assert_eq!(
                case.golden.stdout,
                Some("exception error badarg"),
                "case {id} must carry the oracle-derived badarg golden"
            );
            assert_eq!(case.golden.exit_code, 1, "case {id} golden must exit 1");
        }
    }
}
