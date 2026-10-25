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

//! Contract **A.5** — the distribution wire format (External Term Format, ETF).
//!
//! Distribution frames every message in ETF, and the handshake is a couple of `DFLAG_*`
//! control messages carrying ETF terms. That *codec* is a black box the CLI can reach:
//! an ETF blob is just bytes, so decoding a **fixed** blob that was produced by the
//! reference runtime is a genuine cross-runtime check of the wire encoding, and
//! encoding a term to the same blob is the inverse direction.
//!
//! The cases cover the concrete tags the distribution encoder/decoder actually uses:
//!
//! * numbers — `SMALL_INTEGER_EXT` (97), `INTEGER_EXT` (98), `NEW_FLOAT_EXT` (70),
//!   `SMALL_BIG_EXT` (110), `LARGE_BIG_EXT` (111);
//! * tuples — `SMALL_TUPLE_EXT` (104) and `LARGE_TUPLE_EXT` (105, for a >255-element tuple);
//! * lists — `NIL_EXT` (106), `STRING_EXT` (107), `LIST_EXT` (108) (including an
//!   improper tail);
//! * binaries — `BINARY_EXT` (109) and `BIT_BINARY_EXT` (77);
//! * maps — `MAP_EXT` (116); a *multi-key* map is compared through its **sorted** key list,
//!   because map iteration order is an internal detail and not part of the wire contract,
//!   and a flatmap's canonical **key order** is pinned byte-exactly (`wire/map_ext_*`), as is
//!   the rejection of a blob that repeats a key;
//! * atoms — the legacy `ATOM_EXT` (100), and the UTF-8 `ATOM_UTF8_EXT` (118) /
//!   `SMALL_ATOM_UTF8_EXT` (119) including the UTF-8 bytes of a non-Latin-1 atom;
//! * external things — `NEW_PID_EXT` (88), `NEWER_REFERENCE_EXT` (90), `NEW_PORT_EXT` (89)
//!   and `NEWER_PORT_EXT` (120), plus the *legacy* `PID_EXT` (103), `REFERENCE_EXT` (101),
//!   `NEW_REFERENCE_EXT` (114) and `PORT_EXT` (102) forms — all encoded through the
//!   *decode → re-encode* round-trip, which is deterministic inside one node's encoder. The
//!   terms whose bytes depend on the runtime's **own** node identity — a local `self()` pid and
//!   an internal port — carry per-VM-start values and are pinned (by their stable leading shape)
//!   in the [`super::wire_ids`] group instead;
//! * funs/records — `EXPORT_EXT` (113) (encode and decode → re-encode round-trip),
//!   `NEW_FUN_EXT` (112), and the nominal-record `RECORD_EXT` (67) (decode → re-encode).
//!   `NEW_FUN_EXT` has two halves: the `-eval` (`erl_eval`) lambda, whose image is the one
//!   encode the Rust runtime does not serve ([`wire/new_fun_ext_encode`], a documented gap), and
//!   a **compiled** module's local fun, whose image is byte-identical on both runtimes. The
//!   module-run case `wire/fun_ext_multi_closure_roundtrip` pins the *compiled* half — a fun
//!   that captures a variable from the enclosing scope of a **multi-closure** module (two
//!   `make_fun3` funs, plus a nested `fun`-returning-`fun`) — through the round trip's
//!   observable **shape** (`is_function/2`, the `fun_info/1` identity items, the image byte size
//!   and the re-encode identity), never the image's identity bytes. It claims contract A.2 (the
//!   `.beam`/ETF byte contract) because a module-run case may only claim the contracts it
//!   evidences directly (`corpus::tests::module_run_cases_are_registered_and_well_formed`);
//! * framing — `VERSION_MAGIC` (131) as the first byte, the `compressed` (80) option (both
//!   directions, including its declared uncompressed-size bound), and `{minor_version, 0|1|2}`;
//! * `ATOM_CACHE_REF` (82) — a blob carrying one is decoded with **no** atom-cache context,
//!   which both runtimes refuse identically.
//! * `ATOM_INTERNAL_REF2` (73) / `ATOM_INTERNAL_REF3` (75) — an atom named by its
//!   **atom-table index** rather than its name: an in-range index materializes the atom
//!   (asserted only as `is_atom/1`, never by name, because the index→atom mapping depends
//!   on the node's atom-table bootstrap order), an index past the table is `badarg`, and
//!   the `[safe]` option does not apply to the index forms (the atom already exists).
//! * the `erlang:binary_to_term/2` `[safe]` option — a fresh atom *name* is refused with
//!   `badarg`, while a genuinely `compressed` (80) image is accepted and decoded.
//!
//! The live handshake / a node-to-node loopback and the `DOP_*` control-message framing are
//! **not** expressible as one CLI process evaluating an expression — they need two connected
//! nodes — so they live in [`super::dist`] and [`super::dist_extra`] as `CaseKind::Command`
//! cases: each side's own runtime is the client, an optional [`super::PeerRun`] leg starts a
//! **second process of that same runtime**, and a real `epmd`-less loopback carries the
//! `DOP_*` payloads.
//!
//! ## Honest capability gaps (pinned as `unsupported`, never a mismatch)
//!
//! * The free-standing encode of a `NEW_FUN_EXT` (112) fun is the Rust runtime's remaining
//!   A.5 *encode*-side gap; `wire/new_fun_ext_encode` pins it as `unsupported` (`beam_rt:
//!   unsupported: local_fun`), never a mismatch. It is the `-eval` (`erl_eval`) half only: the
//!   *compiled*-module half **agrees** byte-for-byte (see `wire/fun_ext_multi_closure_roundtrip`).
//!
//! ## Agreement notes
//!
//! * A fun decoded from its own `NEW_FUN_EXT` image is **callable**, and its two raise classes
//!   are pinned: `wire/decoded_new_fun_ext_closure_is_callable` applies the decoded closure (a
//!   plain one and a capturing `X * A + B` one), `wire/decoded_new_fun_ext_bad_index_raises_badfun`
//!   pins `error:{badfun, F}` for an image whose `index` names no lambda of the module or whose
//!   16-byte module checksum is corrupted, and
//!   `wire/decoded_new_fun_ext_unloadable_module_raises_undef` pins `error:undef` for an image
//!   naming a module that is not loadable.
//! * `term_to_binary/2` with `{minor_version, 0|1}` reproduces the *legacy* `ATOM_EXT` (100) /
//!   `FLOAT_EXT` (99) forms, so the `wire/minor_version_*_legacy_encode` cases `agree`.
//!   Decoding a `compressed` (80) blob is covered by the `wire/compressed_decode_*` cases.
//! * The `[local]` option image (`term_to_binary(Term, [local])`) IS served through the BIF: the
//!   image is `131`, `LOCAL_EXT` (121), a 4-byte *local node hash*, then the ordinary ETF body. The
//!   hash is randomised on every node start on **both** sides, so no whole-image byte golden exists;
//!   the `etf/term_to_binary_local_*` cases in [`super::etf`] mask exactly those 4 bytes and compare
//!   the rest of the image byte-for-byte.
//! * `[deterministic]` is served: a map's keys are sorted into canonical order on both runtimes
//!   (`term_to_binary(#{b => 2, a => 1}, [deterministic])` is the canonical `a`, `b` image), and
//!   the canonical flatmap key order is pinned by the plain-encoder cases in this group.
//! * `term_to_binary(make_ref())` *executes* on both runtimes (a `NEWER_REFERENCE_EXT` blob of 35
//!   bytes on each); the reference id's mask/hash bytes are random per creation, so the image is
//!   pinned by its stable shape — the tag byte, the length and the round-trip identity — in
//!   [`super::wire_ids`]'s `wire_ids/newer_reference_*` cases, never by its id bytes.
//! * `FUN_EXT` (115) is deliberately not a case: the reference decoder does not validate it
//!   (even a truncated blob decodes to the empty atom `''`), so any golden would pin an
//!   unspecified artifact rather than the wire semantics. The modern `NEW_FUN_EXT` (112) is
//!   the tag that matters for the distribution wire.

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The module behind `wire/fun_ext_multi_closure_roundtrip`: **two** closures in one module (so
/// the module's `make_fun3` fun table is a real entry the `NEW_FUN_EXT` index has to name) plus a
/// *nested* `fun`-returning-`fun` whose inner closure captures a variable (`A`) from its
/// enclosing fun's scope — the two shapes a fun image has to carry: `index`/`uniq` and the
/// free-variable environment.
///
/// The identity observations are hard `=` matches (a wrong `fun_info/2` item aborts the run as a
/// `badmatch`, so it can never be a silent pass) and the function answers with **one integer**
/// as the corpus requires of a module run: the two image byte sizes (stable for this compiled
/// module, and the only *classification* of a fun's identity bytes the case allows itself)
/// followed by a 3-bit flag mask that is `7` when all three round-trip relations hold.
const WIRE_FUN_ETF_SOURCE: &str = "-module(wire_fun_etf).\n\
-export([multi_closure_roundtrip/0]).\n\
mk_adder(N) -> fun(X) -> X + N end.\n\
mk_doubler() -> fun(X) -> X * 2 end.\n\
bit(true, B) -> B;\n\
bit(false, _) -> 0.\n\
multi_closure_roundtrip() ->\n\
F = mk_adder(3),\n\
D = mk_doubler(),\n\
B = term_to_binary(F),\n\
G = binary_to_term(B),\n\
Inner = (fun(A) -> fun(K) -> A * K end end)(6),\n\
{type, local} = erlang:fun_info(G, type),\n\
{module, wire_fun_etf} = erlang:fun_info(G, module),\n\
{arity, 1} = erlang:fun_info(G, arity),\n\
42 = F(39),\n\
42 = D(21),\n\
42 = Inner(7),\n\
Flags = bit(is_function(F, 1), 1) bor bit(is_function(G, 1), 2)\n\
bor bit(term_to_binary(G) =:= B, 4),\n\
byte_size(B) * 10000 + byte_size(term_to_binary(Inner)) * 100 + Flags.\n";

/// The module behind the three `wire/decoded_new_fun_ext_*` cases: what a **decoded**
/// `NEW_FUN_EXT` closure does when it is *applied*, and what the two broken images raise.
///
/// Every image is built in Erlang from the encoded bytes of a real local fun — no harness or C
/// support — by bit-syntax surgery on the `NEW_FUN_EXT` payload, whose layout is
/// `[131][112][Size:u32][Arity:u8][Uniq:16][Index:u32][NumFree:u32][module atom][old_index]…`:
/// `index` sits at bytes 23..27, the 16-byte module checksum at 7..23 and the module atom starts
/// at byte 31. The bad-index and bad-checksum images keep their byte length (so the declared
/// `Size` still holds), and the unloadable one swaps the 9-byte atom text for another 9-byte name
/// (`wire_nfdec` → `wire_nfnon`, which nothing can load).
///
/// Each function answers with **one integer** as a module run requires: the two application
/// results in the high digits plus a relation mask, and for the two raise cases a small
/// classification code per image. Nothing here is a volatile figure — no image byte, no path, no
/// pid.
const WIRE_NEW_FUN_DECODE_SOURCE: &str = "-module(wire_nfdec).\n\
-export([call_roundtrip/0, bad_images/0, unloadable_module/0]).\n\
bit(true, B) -> B;\n\
bit(false, _) -> 0.\n\
roundtrip(F) -> binary_to_term(term_to_binary(F)).\n\
call_roundtrip() -> call(6, 0).\n\
call(A, B) ->\n\
Id = fun(X) -> X end,\n\
Cap = fun(X) -> X * A + B end,\n\
GId = roundtrip(Id),\n\
GCap = roundtrip(Cap),\n\
Ans1 = GId(42),\n\
Ans2 = GCap(7),\n\
Flags = bit(is_function(GId, 1), 1) bor bit(is_function(GCap, 1), 2)\n\
bor bit(element(2, erlang:fun_info(GCap, arity)) =:= 1, 4)\n\
bor bit(length(element(2, erlang:fun_info(GCap, env))) =:= 2, 8),\n\
Ans1 * 100000 + Ans2 * 1000 + Flags.\n\
classify(Bin) ->\n\
Outcome = try binary_to_term(Bin) of\n\
G -> try (G)(1) of Val -> {applied, Val}\n\
catch Cls:Reason -> {raised, Cls, Reason} end\n\
catch Cls2:Reason2 -> {decode_raised, Cls2, Reason2} end,\n\
case Outcome of\n\
{applied, _} -> 0;\n\
{raised, error, {badfun, _}} -> 1;\n\
{raised, _, _} -> 2;\n\
{decode_raised, _, _} -> 3\n\
end.\n\
bad_images() ->\n\
F = fun(X) -> X + 1 end,\n\
Base = term_to_binary(F),\n\
<<Pre:23/binary, _Idx:32, Post/binary>> = Base,\n\
BadIndex = <<Pre/binary, 16#7FFFFFFF:32, Post/binary>>,\n\
<<Q:7/binary, _Uniq:16/binary, Rest/binary>> = Base,\n\
BadChecksum = <<Q/binary, (binary:copy(<<255>>, 16))/binary, Rest/binary>>,\n\
classify(BadIndex) * 10 + classify(BadChecksum).\n\
unloadable_module() ->\n\
F = fun(X) -> X + 1 end,\n\
Base = term_to_binary(F),\n\
<<Pre:31/binary, 119, Len:8, Name:Len/binary, Rest/binary>> = Base,\n\
true = (Len =:= byte_size(<<\"wire_nfnon\">>)),\n\
<<\"wire_nfdec\">> = Name,\n\
Bad = <<Pre/binary, 119, Len:8, \"wire_nfnon\", Rest/binary>>,\n\
Outcome = try binary_to_term(Bad) of\n\
G -> try (G)(7) of Val -> {applied, Val}\n\
catch Cls:Reason:St -> {raised, Cls, Reason, St} end\n\
catch Cls2:Reason2 -> {decode_raised, Cls2, Reason2} end,\n\
case Outcome of\n\
{raised, error, undef, [Top | _]} ->\n\
case Top of\n\
{Fun0, Args0, []} when is_function(Fun0, 1), Args0 =:= [7] -> 1;\n\
_ -> 2\n\
end;\n\
{raised, _, _, _} -> 3;\n\
{applied, _} -> 4;\n\
{decode_raised, _, _} -> 5\n\
end.\n";

/// The A.5 cases: the ETF codec used by the distribution wire.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- the original decode/encode cross-checks --------------------------------
        Case {
            id: "wire/decode_reference_tuple_blob",
            description: "an ETF blob produced by the reference runtime decodes to {ok,42}",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,119,2,111,107,97,42>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_atom_blob",
            description: "an ETF blob of an atom decodes to the atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_map_blob",
            description: "an ETF MAP_EXT blob decodes to the original map",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,116,0,0,0,1,119,1,97,97,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok #{a => 1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encode_atom_matches_reference",
            description: "encoding an atom to ETF reproduces the reference wire bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(foo)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,6,131,119,3,102,111,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/cross_roundtrip",
            description: "decoding the reference encoding of {ok,42} reproduces the term",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({ok,42}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {ok,42}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encoded_size",
            description: "the reference encoding of {ok,42} is 9 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "byte_size(term_to_binary({ok,42}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 9"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decode_reference_small_int_blob",
            description: "an ETF SMALL_INT_EXT blob decodes to the integer it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,97,42>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 42"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encode_map_matches_reference",
            description: "encoding a map reproduces the reference MAP_EXT wire bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{a => 1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/encoded_atom_size",
            description: "the reference encoding of the atom foo is 6 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "byte_size(term_to_binary(foo))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 6"),
                exit_code: 0,
            },
        },
        // -- numbers: NEW_FLOAT_EXT (70) --------------------------------------------
        Case {
            id: "wire/float_decode_reference",
            description: "a fixed NEW_FLOAT_EXT (70) blob decodes to the IEEE-754 double it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,70,64,9,30,184,81,235,133,31>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 3.14"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/float_encode_matches_reference",
            description: "encoding a float reproduces the reference NEW_FLOAT_EXT (70) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(-2.5)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,70,192,4,0,0,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/float_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a float",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(1.5))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 1.5"),
                exit_code: 0,
            },
        },
        // -- big integers: SMALL_BIG_EXT (110) / LARGE_BIG_EXT (111) ----------------
        Case {
            id: "wire/small_big_negative_decode_reference",
            description: "a fixed SMALL_BIG_EXT (110) blob with sign byte 1 decodes to a negative big",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,110,3,1,0,0,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -65536"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_big_encode_matches_reference",
            description: "a 7-digit big integer encodes as SMALL_BIG_EXT (110) with little-endian digits",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(65536*65536*65536)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,7,0,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_big_negative_encode_matches_reference",
            description: "a negative big integer encodes with the SMALL_BIG_EXT (110) sign byte set",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(-(65536*65536*65536))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,110,7,1,0,0,0,0,0,0,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_big_decode_reference",
            description: "a fixed LARGE_BIG_EXT (111) blob decodes to 2^64",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,111,0,0,0,9,0,0,0,0,0,0,0,0,0,1>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 18446744073709551616"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_big_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a 9-digit big integer",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(65536*65536*65536*65536))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 18446744073709551616"),
                exit_code: 0,
            },
        },
        // -- strings / lists: NIL_EXT (106), STRING_EXT (107), LIST_EXT (108) --------
        Case {
            id: "wire/nil_ext_decode_reference",
            description: "the NIL_EXT (106) empty-list blob decodes to []",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,106>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok []"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/string_ext_decode_reference",
            description: "a fixed STRING_EXT (107) blob decodes to the byte list it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,107,0,3,97,98,99>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [97,98,99]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/string_ext_encode_matches_reference",
            description: "a byte list encodes as STRING_EXT (107) with a 16-bit length",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(\"abc\")",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,3,97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_decode_reference",
            description: "a fixed LIST_EXT (108) blob with a NIL_EXT tail decodes to [1,2,3]",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,108,0,0,0,3,97,1,97,2,97,3,106>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_improper_tail",
            description: "an improper list encodes as LIST_EXT (108) whose tail is the raw element",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary([1|2])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,108,0,0,0,1,97,1,97,2>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/list_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a three-element list",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,2,3]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        // -- binaries: BINARY_EXT (109) ---------------------------------------------
        Case {
            id: "wire/binary_ext_encode_matches_reference",
            description: "a binary encodes as BINARY_EXT (109) with a 32-bit length",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(<<\"abc\">>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,3,97,98,99>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/binary_ext_decode_reference",
            description: "a fixed BINARY_EXT (109) blob decodes to the byte binary it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,109,0,0,0,3,97,98,99>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<97,98,99>>"),
                exit_code: 0,
            },
        },
        // -- atoms: SMALL_ATOM_UTF8_EXT (119) / ATOM_UTF8_EXT (118) ------------------
        Case {
            id: "wire/small_atom_utf8_ext_decode_reference",
            description: "a fixed SMALL_ATOM_UTF8_EXT (119) blob decodes to the atom it names",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/atom_utf8_ext_decode_reference",
            description: "a fixed ATOM_UTF8_EXT (118) blob with a 16-bit length decodes to the atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,118,0,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_atom_utf8_ext_unicode_encode",
            description: "a non-Latin-1 atom encodes as SMALL_ATOM_UTF8_EXT (119) with its UTF-8 bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(list_to_atom([945]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,2,206,177>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_atom_utf8_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a plain atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(hello))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok hello"),
                exit_code: 0,
            },
        },
        // -- external things: NEW_PID_EXT (88), NEWER_REFERENCE_EXT (90), ports -----
        Case {
            id: "wire/new_pid_ext_roundtrip",
            description: "a fixed NEW_PID_EXT (88) blob re-encodes to the identical PID bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,10,0,0,0,0,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,10,0,0,0,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/newer_reference_ext_roundtrip",
            description: "a fixed NEWER_REFERENCE_EXT (90) blob re-encodes to the identical reference bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,8,160,44,4,180>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_port_ext_upgrades_to_newer",
            description: "a NEW_PORT_EXT (89) blob re-encodes as the equivalent NEWER_PORT_EXT (120) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,89,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/newer_port_ext_roundtrip",
            description: "a fixed NEWER_PORT_EXT (120) blob re-encodes to the identical port bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/legacy_port_ext_width_roundtrip",
            description: "a legacy PORT_EXT (102) blob (32-bit id + 1-byte creation) decodes and re-encodes as the equivalent NEWER_PORT_EXT (120) image with the id and creation preserved",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // A *fixed external-node* port (node `other`, id 7, creation 1): every field
                // is byte-stable, so the decode -> re-encode image is pinnable.
                expr: "binary_to_term(<<131,102,100,0,5,111,116,104,101,114,0,0,0,7,1>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,1>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/legacy_port_ext_creation_zero_roundtrip",
            description: "a legacy PORT_EXT (102) blob with creation 0 decodes and re-encodes as the equivalent NEWER_PORT_EXT (120) image with the zero creation preserved",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,102,100,0,5,111,116,104,101,114,0,0,0,7,0>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,120,119,5,111,116,104,101,114,0,0,0,0,0,0,0,7,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        // -- funs: EXPORT_EXT (113) / NEW_FUN_EXT (112) -----------------------------
        Case {
            id: "wire/export_ext_encode",
            description: "an exported function fun encodes as EXPORT_EXT (113) (module/function/arity)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun lists:reverse/1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_fun_ext_encode",
            description: "a lambda encodes as NEW_FUN_EXT (112) with its module, index and free variables",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(fun() -> ok end)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,112,0,0,0,136,0,248,37,47,246,123,244,211,6,126,138,154,65,205,61,207,38,0,0,0,43,0,0,0,1,119,8,101,114,108,95,101,118,97,108,97,43,98,7,193,41,127,88,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,0,0,0,0,0,0,0,104,6,97,1,106,119,4,110,111,110,101,119,4,110,111,110,101,116,0,0,0,0,108,0,0,0,1,104,5,119,6,99,108,97,117,115,101,97,1,106,106,108,0,0,0,1,104,3,119,4,97,116,111,109,97,1,119,2,111,107,106,106>>"),
                exit_code: 0,
            },
        },
        // -- a *compiled* module's fun: the NEW_FUN_EXT form a `.beam` really produces ------
        //
        // See `WIRE_FUN_ETF_SOURCE` for the module. Nothing here is an identity byte: the module
        // asserts the round trip's *shape* (`is_function/2`, the `fun_info/1`
        // type/module/arity of the decoded fun, the re-encode identity `term_to_binary(G) =:= B`)
        // and the applications of the **original** funs (`F(39)`, `D(21)`) and of the nested
        // closure (`Inner(7)`) with hard matches, then classifies the two `NEW_FUN_EXT` image
        // byte sizes — stable integers for a fixed compiled module — plus a 3-bit relation mask
        // into one integer: `82*10000 + 80*100 + 7 = 828007`. The decoded fun is only
        // *identified* here; *applying* one is what the three `wire/decoded_new_fun_ext_*` cases
        // below pin.
        Case {
            id: "wire/fun_ext_multi_closure_roundtrip",
            description: "a capturing fun of a multi-closure compiled module round-trips through NEW_FUN_EXT (112): is_function, fun_info identity, image sizes and re-encode identity, classified into one integer",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Module(ModuleRun {
                source: WIRE_FUN_ETF_SOURCE,
                module: "wire_fun_etf",
                function: "multi_closure_roundtrip",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("828007"),
                exit_code: 0,
            },
        },
        // -- *calling* a decoded NEW_FUN_EXT closure: the three apply-side cases --------------
        //
        // See `WIRE_NEW_FUN_DECODE_SOURCE`. The decoded closure is a first-class fun on both
        // runtimes: applying it answers normally, and the two broken images raise the two
        // documented classes (`{badfun, F}` / `undef` with the applied fun as the innermost
        // stacktrace frame). Every image is built inside the module from the bytes of a real local
        // fun, so no harness or C support is involved.
        Case {
            id: "wire/decoded_new_fun_ext_closure_is_callable",
            description: "a local fun of a compiled module round-trips through NEW_FUN_EXT (112) and the decoded closure is applied: a plain fun and a capturing X*A+B closure answer their normal results, classified into one integer",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Module(ModuleRun {
                source: WIRE_NEW_FUN_DECODE_SOURCE,
                module: "wire_nfdec",
                function: "call_roundtrip",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("4242015"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decoded_new_fun_ext_bad_index_raises_badfun",
            description: "a NEW_FUN_EXT image whose index names no lambda of the module, and one whose 16-byte module checksum is corrupted, both still decode and applying them raises error:{badfun, F}, classified into one integer",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Module(ModuleRun {
                source: WIRE_NEW_FUN_DECODE_SOURCE,
                module: "wire_nfdec",
                function: "bad_images",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("11"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/decoded_new_fun_ext_unloadable_module_raises_undef",
            description: "a NEW_FUN_EXT image naming a module atom that is not loadable decodes and applying it raises error:undef whose innermost stacktrace item is the applied fun {Fun, Args, []}, classified into one integer",
            contracts: &[Contract::BeamAsm],
            kind: CaseKind::Module(ModuleRun {
                source: WIRE_NEW_FUN_DECODE_SOURCE,
                module: "wire_nfdec",
                function: "unloadable_module",
                arity: 0,
                args: &[],
                nif: None,
            }),
            golden: Golden {
                stdout: Some("1"),
                exit_code: 0,
            },
        },
        // -- compression / version framing ------------------------------------------
        Case {
            id: "wire/compressed_option_encode",
            description: "term_to_binary/2 with the compressed option encodes the same term under both runtimes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary([1,2,3], [compressed])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,107,0,3,1,2,3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_option_roundtrip",
            description: "term_to_binary/2 with the compressed option round-trips through binary_to_term/1",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary([1,2,3], [compressed]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [1,2,3]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/version_magic_first_byte",
            description: "every encoded term starts with VERSION_MAGIC (131)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary:first(term_to_binary(foo))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 131"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_2_map",
            description: "term_to_binary/2 with {minor_version,2} encodes a map as MAP_EXT",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(#{a => 1}, [{minor_version, 2}])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,1,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        // -- atom cache: ATOM_CACHE_REF (82) ----------------------------------------
        Case {
            id: "wire/atom_cache_ref_without_context",
            description: "a blob carrying ATOM_CACHE_REF (82) is refused identically without a cache context",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,104,2,82,0,119,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // -- ATOM_INTERNAL_REF2 (73) / ATOM_INTERNAL_REF3 (75) ----------------------
        // An atom named by its **atom-table index** rather than by its name
        // (`external.c:5177-5192`, `:3085-3100`); the index names an *existing* atom,
        // so the image carries no name and the `[safe]` flag does not apply (the
        // internal-ref arms have no `internal_tags` gate either — `external.c:6405-6412`).
        // The atom a given index resolves to depends on the node's atom-table bootstrap
        // order, so every assertion here is **index-seeding-independent**: a valid index
        // is pinned only as `is_atom/1`, an index past the table only as `badarg`.
        Case {
            id: "wire/atom_internal_ref2_materializes_the_indexed_atom",
            description: "an in-range ATOM_INTERNAL_REF2 (73) index decodes to the atom it names",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "is_atom(binary_to_term(<<131,73,0,5>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/atom_internal_ref3_materializes_the_indexed_atom",
            description: "an in-range ATOM_INTERNAL_REF3 (75) index decodes to the atom it names",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "is_atom(binary_to_term(<<131,75,0,0,5>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/atom_internal_ref2_index_out_of_range",
            description: "an ATOM_INTERNAL_REF2 (73) index at the 16-bit ceiling is past the atom table and badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,73,255,255>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "wire/atom_internal_ref3_index_out_of_range",
            description: "an ATOM_INTERNAL_REF3 (75) index at the 24-bit ceiling is past the atom table and badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,75,255,255,255>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "wire/atom_internal_ref_bad_index_is_badarg",
            description: "an ATOM_INTERNAL_REF3 (75) index just past the 16-bit range is still past the atom table and badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,75,1,0,0>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "wire/atom_internal_ref2_under_safe",
            description: "[safe] accepts an ATOM_INTERNAL_REF2 (73) image: the index names an existing atom, so nothing is interned",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "is_atom(binary_to_term(<<131,73,0,5>>, [safe]))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        // -- the `binary_to_term/2` `[safe]` surface (A.5) --------------------------
        // `[safe]` refuses only an image that would introduce a *new atom name*; the
        // `COMPRESSED` (80) framing and the index forms above are unaffected.
        Case {
            id: "wire/bin2term_safe_rejects_an_unknown_atom",
            description: "[safe] refuses an image whose SMALL_ATOM_UTF8_EXT (119) names a fresh atom",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,119,9,122,122,110,101,119,97,116,111,109>>, [safe])",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "wire/bin2term_safe_compressed",
            description: "[safe] accepts a genuinely COMPRESSED (80) image and decodes it back to the term",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "begin B = term_to_binary(lists:duplicate(300, ok), [compressed]), {binary:at(B,1), binary_to_term(B,[safe]) =:= lists:duplicate(300,ok)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {80,true}"),
                exit_code: 0,
            },
        },
        // -- NEW_REFERENCE_EXT (114): the compact reference form --------------------
        Case {
            id: "wire/new_reference_ext_is_reference",
            description: "a NEW_REFERENCE_EXT (114) blob decodes to a reference",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "is_reference(binary_to_term(<<131,114,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/new_reference_ext_roundtrip",
            description: "a NEW_REFERENCE_EXT (114) blob re-encodes as the equivalent NEWER_REFERENCE_EXT (90) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,114,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,2,35,202,52,184,0,0,0,1,0,0,0,2,0,0,0,3,0,0,0,4>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,3,119,13,110,111,110,111,100,101,64,110,111,104,111,115,116,0,0,0,0,0,0,0,0,2,35,202,52,184,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        // -- BIT_BINARY_EXT (77): bitstrings with a bit count -----------------------
        Case {
            id: "wire/bit_binary_ext_decode",
            description: "a fixed BIT_BINARY_EXT (77) blob decodes to the bitstring it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,77,0,0,0,1,3,96>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<3:3>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/bit_binary_ext_encode",
            description: "a 3-bit bitstring encodes as BIT_BINARY_EXT (77) with its bit count",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(<<3:3>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,77,0,0,0,1,3,96>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/bit_binary_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a bitstring",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(<<3:3>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<3:3>>"),
                exit_code: 0,
            },
        },
        // -- tuples: SMALL_TUPLE_EXT (104) / LARGE_TUPLE_EXT (105) -------------------
        Case {
            id: "wire/small_tuple_ext_encode",
            description: "a 2-tuple encodes as SMALL_TUPLE_EXT (104) with a 32-bit arity",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary({a,1})",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,1,97,97,1>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/small_tuple_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a tuple",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary({a,1}))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {a,1}"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_tuple_ext_encode_tag",
            description: "a 300-element tuple encodes with the LARGE_TUPLE_EXT (105) tag",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary:at(term_to_binary(list_to_tuple(lists:seq(1,300))), 1)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 105"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/large_tuple_ext_roundtrip",
            description: "binary_to_term/1 inverts term_to_binary/1 for a 300-element tuple",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "tuple_size(binary_to_term(term_to_binary(list_to_tuple(lists:seq(1,300)))))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 300"),
                exit_code: 0,
            },
        },
        // -- MAP_EXT (116): a multi-key map, compared order-independently ------------
        Case {
            id: "wire/map_ext_multi_key_roundtrip",
            description: "a multi-key map round-trips through MAP_EXT (116), compared as a sorted key list",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "lists:sort(maps:to_list(binary_to_term(term_to_binary(#{z => 1, a => 2, m => 3}))))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [{a,2},{m,3},{z,1}]"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/map_ext_noncanonical_key_order_canonicalizes",
            description: "a MAP_EXT (116) flatmap whose keys are on the wire in non-canonical order decodes and re-encodes in canonical sorted-key order",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // `#{2 => 20, 1 => 10}` on the wire as `2` then `1`.
                expr: "binary_to_term(<<131,116,0,0,0,2,97,2,97,20,97,1,97,10>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,116,0,0,0,2,97,1,97,10,97,2,97,20>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/map_ext_duplicate_key_badarg",
            description: "a MAP_EXT (116) blob that repeats a key is rejected with badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,116,0,0,0,2,97,1,97,10,97,1,97,20>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        Case {
            id: "wire/map_ext_canonical_key_order_float_list_binary",
            description: "term_to_binary/1 of a map with a float, a list and a binary key emits the canonical term-order flatmap image byte-exactly",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "#{1.5 => a, [1,2] => b, <<\"z\">> => c}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,116,0,0,0,3,70,63,248,0,0,0,0,0,0,119,1,97,107,0,2,1,2,119,1,98,109,0,0,0,1,122,119,1,99>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/map_ext_binary_key_order",
            description: "term_to_binary/1 orders several binary keys canonically in the flatmap image",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "#{<<\"b\">> => 1, <<\"a\">> => 2, <<\"c\">> => 3, <<\"aa\">> => 4}",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,116,0,0,0,4,109,0,0,0,1,97,97,2,109,0,0,0,2,97,97,97,4,109,0,0,0,1,98,97,1,109,0,0,0,1,99,97,3>>",
                ),
                exit_code: 0,
            },
        },
        // -- legacy integer / atom forms: INTEGER_EXT (98), ATOM_EXT (100) -----------
        Case {
            id: "wire/integer_ext_legacy_decode",
            description: "a fixed legacy INTEGER_EXT (98) blob decodes to the 32-bit integer it carries",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,98,0,0,1,44>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 300"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/atom_ext_legacy_decode",
            description: "a fixed legacy ATOM_EXT (100) blob decodes to the atom it names",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(<<131,100,0,3,102,111,111>>)",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok foo"),
                exit_code: 0,
            },
        },
        // -- legacy external forms: PID_EXT (103) / REFERENCE_EXT (101) --------------
        Case {
            id: "wire/pid_ext_upgrades_to_new_pid",
            description: "a legacy PID_EXT (103) blob re-encodes as the equivalent NEW_PID_EXT (88) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,103,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,88,119,7,102,111,111,64,98,97,114,0,0,0,1,0,0,0,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/reference_ext_upgrades_to_newer",
            description: "a legacy REFERENCE_EXT (101) blob re-encodes as the equivalent NEWER_REFERENCE_EXT (90) bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,101,119,7,102,111,111,64,98,97,114,0,0,0,1,0>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,90,0,1,119,7,102,111,111,64,98,97,114,0,0,0,0,0,0,0,1>>",
                ),
                exit_code: 0,
            },
        },
        // -- EXPORT_EXT (113): the decode direction of an exported fun --------------
        Case {
            id: "wire/export_ext_decode_roundtrip",
            description: "a fixed EXPORT_EXT (113) blob decodes to the exported fun and re-encodes identically",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>))",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>",
                ),
                exit_code: 0,
            },
        },
        // -- `{minor_version, _}`: the wire dialect of the encode ---------------------
        // `N < 2` selects the *legacy* `ATOM_EXT` (100) / `FLOAT_EXT` (99) forms; both
        // runtimes encode them identically, so these cases `agree`.
        Case {
            id: "wire/minor_version_0_atom_legacy_encode",
            description: "term_to_binary/2 with {minor_version,0} encodes an atom as the legacy ATOM_EXT (100)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(hello, [{minor_version, 0}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,9,131,100,0,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_1_atom_legacy_encode",
            description: "term_to_binary/2 with {minor_version,1} still encodes an atom as the legacy ATOM_EXT (100)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(hello, [{minor_version, 1}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,9,131,100,0,5,104,101,108,108,111>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_0_float_legacy_encode",
            description: "term_to_binary/2 with {minor_version,0} encodes a float as the legacy ASCII FLOAT_EXT (99)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(1.5, [{minor_version, 0}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,109,0,0,0,33,131,99,49,46,53,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,48,101,43,48,48,0,0,0,0,0>>",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/minor_version_1_float_encode",
            description: "term_to_binary/2 with {minor_version,1} encodes a float as NEW_FLOAT_EXT (70)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(1.5, [{minor_version, 1}])",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,109,0,0,0,10,131,70,63,248,0,0,0,0,0,0>>"),
                exit_code: 0,
            },
        },
        // -- `compressed` (80) DECODE -------------------------------------------------
        Case {
            id: "wire/compressed_decode_roundtrip_large",
            description: "a large term round-trips through term_to_binary/2 with the compressed option and binary_to_term/1",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "binary_to_term(term_to_binary(lists:seq(1,1000), [compressed])) =:= lists:seq(1,1000)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,119,4,116,114,117,101>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_decode_trailing_bytes_ignored",
            description: "a fixed COMPRESSED (80) blob whose inflated body carries trailing bytes after a complete term decodes to that term",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // Compressed body = version-magic-stripped external {ok,42} ++ <<1,2,3,4,5>>.
                expr: "binary_to_term(<<131,80,0,0,0,13,120,156,203,96,42,103,202,207,78,212,98,100,98,102,97,5,0,21,163,2,88>>)",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("ok <<131,104,2,119,2,111,107,97,42>>"),
                exit_code: 0,
            },
        },
        // -- COMPRESSED (80) declared-inner-size bound --------------------------------
        Case {
            id: "wire/compressed_declared_size_bound",
            description: "a COMPRESSED (80) image declared with its true uncompressed size round-trips",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                // Rebuild the blob from its own declared size so the bound is exactly met.
                expr: "begin T = binary:copy(<<\"abcdabcd\">>,400), Comp = term_to_binary(T,[compressed]), <<131,80,Sz:32,Tail/binary>> = Comp, binary_to_term(<<131,80,Sz:32,Tail/binary>>) =:= T end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok true"),
                exit_code: 0,
            },
        },
        Case {
            id: "wire/compressed_declared_size_mismatch_badarg",
            description: "a COMPRESSED (80) image whose declared uncompressed size is one byte larger than the true size is rejected with badarg",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "begin T = binary:copy(<<\"abcdabcd\">>,400), Comp = term_to_binary(T,[compressed]), <<131,80,Sz:32,Tail/binary>> = Comp, binary_to_term(<<131,80,(Sz+1):32,Tail/binary>>) end",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some("exception error badarg"),
                exit_code: 1,
            },
        },
        // -- RECORD_EXT (67): the nominal-record instance form ------------------------
        Case {
            id: "wire/record_ext_roundtrip",
            description: "a fixed RECORD_EXT (67) blob decodes to the record instance and re-encodes to identical bytes",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Eval {
                expr: "term_to_binary(binary_to_term(<<131,67,0,0,0,2,0,119,5,109,121,109,111,100,119,6,112,101,114,115,111,110,119,4,110,97,109,101,119,3,97,103,101,119,3,106,111,101,97,21>>))",
                extra_argv: &[],
                probe: Probe::TermToBinary,
            },
            golden: Golden {
                stdout: Some(
                    "ok <<131,109,0,0,0,40,131,67,0,0,0,2,0,119,5,109,121,109,111,100,119,6,112,101,114,115,111,110,119,4,110,97,109,101,119,3,97,103,101,119,3,106,111,101,97,21>>",
                ),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, Contract};

    #[test]
    fn new_wire_cases_are_registered_under_the_distribution_contract() {
        let cases = group();
        for id in [
            "wire/minor_version_0_atom_legacy_encode",
            "wire/minor_version_1_atom_legacy_encode",
            "wire/minor_version_0_float_legacy_encode",
            "wire/minor_version_1_float_encode",
            "wire/compressed_decode_roundtrip_large",
            "wire/compressed_decode_trailing_bytes_ignored",
            "wire/record_ext_roundtrip",
            "wire/legacy_port_ext_width_roundtrip",
            "wire/legacy_port_ext_creation_zero_roundtrip",
            "wire/map_ext_noncanonical_key_order_canonicalizes",
            "wire/map_ext_canonical_key_order_float_list_binary",
            "wire/map_ext_binary_key_order",
            "wire/compressed_declared_size_bound",
            "wire/atom_internal_ref2_materializes_the_indexed_atom",
            "wire/atom_internal_ref3_materializes_the_indexed_atom",
            "wire/atom_internal_ref2_under_safe",
            "wire/bin2term_safe_compressed",
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::Distribution], "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
    }

    /// The one `NEW_FUN_EXT`-image case that is a **module run** (a compiled module's local fun,
    /// whose image both runtimes encode identically): it evidences contract A.2 — the
    /// `.beam`/ETF byte contract — because a module-run case may only claim the contracts it
    /// evidences directly, so it is not one of the `Distribution`-claiming eval cases above.
    #[test]
    fn the_compiled_fun_image_case_is_a_module_run_under_the_beam_asm_contract() {
        let cases = group();
        let case = cases
            .iter()
            .find(|c| c.id == "wire/fun_ext_multi_closure_roundtrip")
            .expect("registered");
        assert_eq!(case.contracts, &[Contract::BeamAsm]);
        assert_eq!(case.kind_label(), "module-run");
        let CaseKind::Module(run) = &case.kind else {
            panic!("the compiled-fun case runs a compiled module");
        };
        assert_eq!(run.module, "wire_fun_etf");
        assert_eq!(run.function, "multi_closure_roundtrip");
        assert_eq!(run.arity, 0);
        assert!(run.args.is_empty());
        assert!(run.nif.is_none());
        // The golden is the oracle's integer line, never an identity byte: the two image sizes
        // (82 bytes for the capturing closure, 80 for the nested one) are stable integers for a
        // fixed compiled module and the 3-bit mask (7) says every round-trip relation holds. The
        // decoded fun is only *identified* (`is_function/2` + `fun_info/1`) here; *applying* a
        // decoded closure is what the three `wire/decoded_new_fun_ext_*` cases pin.
        assert_eq!(case.golden.stdout, Some("828007"));
        assert_eq!(case.golden.exit_code, 0);
    }

    /// The three **apply-side** `NEW_FUN_EXT` cases: each is a module run over one fixture module
    /// (so it uses the `run` action, not `-eval`) and each claims A.2 — the `.beam`/ETF byte
    /// contract — because that is the contract it evidences directly. Every golden is the
    /// oracle's integer line: two application results plus a relation mask for the callable case,
    /// and a per-image classification code for the two raise cases. No image byte and no volatile
    /// figure is pinned.
    #[test]
    fn the_decoded_new_fun_ext_cases_apply_the_closure_under_the_beam_asm_contract() {
        let cases = group();
        for (id, function, golden) in [
            (
                "wire/decoded_new_fun_ext_closure_is_callable",
                "call_roundtrip",
                "4242015",
            ),
            (
                "wire/decoded_new_fun_ext_bad_index_raises_badfun",
                "bad_images",
                "11",
            ),
            (
                "wire/decoded_new_fun_ext_unloadable_module_raises_undef",
                "unloadable_module",
                "1",
            ),
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::BeamAsm], "{id}");
            assert_eq!(case.kind_label(), "module-run", "{id}");
            let CaseKind::Module(run) = &case.kind else {
                panic!("{id}: the decoded-fun cases run a compiled module");
            };
            assert_eq!(run.module, "wire_nfdec", "{id}");
            assert_eq!(run.function, function, "{id}");
            assert_eq!(run.arity, 0, "{id}");
            assert!(run.args.is_empty(), "{id}");
            assert!(run.nif.is_none(), "{id}");
            assert_eq!(case.golden.stdout, Some(golden), "{id}");
            assert_eq!(case.golden.exit_code, 0, "{id}");
        }
        // The fixture builds every broken image from the encoded bytes of a real fun — bit-syntax
        // surgery on the `NEW_FUN_EXT` payload — so the three cases need no harness or C support.
        let fixture = cases
            .iter()
            .find_map(|case| match &case.kind {
                CaseKind::Module(run) if run.module == "wire_nfdec" => Some(run.source),
                _ => None,
            })
            .expect("the fixture module is registered");
        assert!(fixture.contains("term_to_binary(F)"), "the images come from a real fun");
        assert!(fixture.contains("16#7FFFFFFF"), "the bad index is written into the image");
        assert!(fixture.contains("binary:copy(<<255>>, 16)"), "the checksum is corrupted");
        assert!(fixture.contains("wire_nfnon"), "the module atom is swapped for an unloadable one");
        assert!(fixture.contains("Args0 =:= [7]"), "the innermost frame is the applied fun");
    }

    #[test]
    fn exception_wire_cases_raise_badarg_under_the_distribution_contract() {        let cases = group();
        for id in [
            "wire/map_ext_duplicate_key_badarg",
            "wire/compressed_declared_size_mismatch_badarg",
            "wire/atom_internal_ref2_index_out_of_range",
            "wire/atom_internal_ref3_index_out_of_range",
            "wire/atom_internal_ref_bad_index_is_badarg",
            "wire/bin2term_safe_rejects_an_unknown_atom",
        ] {
            let case = cases.iter().find(|c| c.id == id).expect("registered");
            assert_eq!(case.contracts, &[Contract::Distribution], "{id}");
            assert_eq!(case.golden.stdout, Some("exception error badarg"), "{id}");
            assert_eq!(case.golden.exit_code, 1, "{id}");
        }
    }
}
