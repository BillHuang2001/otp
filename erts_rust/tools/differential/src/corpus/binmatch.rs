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

//! Two contract surfaces live in this file: **A.7** — the `binary` module's
//! compiled-pattern surface — and **A.2**/**A.1** — the compiled UTF-32 bit-string family
//! (`binary/utf32_*`, described further down). Each case declares its own `contracts`, and
//! the two families do not share them: the first is A.7 (OTP application contracts), the
//! second is A.2 (a compiled `.beam` executes) plus A.1 (the bit-string semantics it pins).
//!
//! `binary:compile_pattern/1` returns an *opaque* handle whose tag encodes the search
//! strategy the runtime chose (`{bm,_}` for a one-word pattern, `{ac,_}` for a multi-word
//! one), and `match/2,3` / `matches/2,3` / `split/2,3` dispatch on it. That tag is
//! observable, so the shape is part of the compatibility surface — and the handle itself is
//! a `Ref` whose id varies per VM start, so the cases assert the **shape**
//! (`element(1, P)`, `is_reference(...)`), never the raw handle.
//!
//! The shape rule (oracle-verified, mirrored from the C `maybe_binary_match_compile` via
//! `../../crates/beam_bifs`): exactly one "word" of pattern — a single binary, **or a
//! one-element list of binaries** — compiles to `{bm, Ref}`; more than one word compiles to
//! `{ac, Ref}`. There is no `am` tag. `badarg` covers an empty binary, an empty list, a
//! non-byte-aligned bitstring, an improper list, and any element that is not a byte-aligned
//! binary (including an integer — bytes are not accepted here).
//!
//! Also pinned here: **bignum values in bit-string construction** (`bs_create_bin`/`bs_init`
//! segments). An explicit-width integer segment takes the value's low `Size` bits, so a bignum
//! is *truncated* rather than rejected (`<<(2 bsl 64):8>>` is `<<0>>`; even `<<256>>` is
//! `<<0>>`), in the signed/unsigned and big-/little-endian forms. Derived sizing goes through
//! `bit_size(binary:encode_unsigned(N))` (`size/1` rejects an integer), and the
//! `binary:decode_unsigned/1,2` counterparts close the round trip.
//!
//! Also here: `iolist_size/1` of a size that outgrows the 60-bit small range. That branch is
//! **not expressible through an observable program** and is deliberately not pinned — see
//! the note below.
//!
//! The file's **other** contract surface — A.2/A.1, not A.7 — is **compiled UTF-32
//! bit-string construction and matching**, the `binary/utf32_*` cases. They are
//! [`CaseKind::Module`] cases — the embedded
//! [`UTF32_SOURCE`] module is `erlc`-compiled and run by both runtimes, so the engine's
//! lowered `bs_create_bin`/`bs_match` UTF-32 arms are what they exercise; the `-eval`
//! evaluator, which is what the `bits/construct_utf32_segments` and
//! `bits/match_utf32_code_points` cases in [`super::bits`] cover, is not involved at all (a
//! UTF-32 defect that lived in the *compiled* opcode path once survived exactly because its
//! only coverage went through the tree-walker).
//!
//! A module-run golden is a plain **integer** line (the corpus-wide shape), so each function
//! returns one integer that carries the bytes it produced: `pack/1` folds a four-byte
//! construction into an integer most-significant byte first (`<<4660/utf32-little>>` is
//! `<<52,18,0,0>>` ⇒ `873594880`; `<<4660/utf32>>` is `<<0,0,18,52>>` ⇒ `4660`), and the two
//! comprehension cases read their twelve produced bytes with `binary:decode_unsigned/1`
//! (`309485013573406512572731392` = `01 00 00 00 34 12 00 00 FF FF 10 00`). A wrong byte or a
//! wrong byte *order* therefore changes the golden; nothing is softened to a length or a
//! shape. The scalar-range case reports one bit per probed value (all four rejects plus an
//! accepted `16#10FFFF` control), and the **native** cases pin an endian-*invariant*
//! relation instead of a byte order, because `utf32-native` depends on the build host:
//! `binary/utf32_native_construction` compares the native segment with the explicit
//! little-/big-endian construction `erlang:system_info(endian)` calls for (⇒ `1` here), and
//! `binary/utf32_native_match` round trips `16#20AC` through a native construction and a
//! native match (⇒ `8364` on either endianness). The module keeps every literal subject and
//! generator behind `id/1`, so the compiler cannot constant-fold a construction or a `case`
//! match into a literal and leave the opcode path unexercised.
//!
//! # Deliberately NOT pinned (handoffs to the runtime nodes)
//!
//! Each of these produces a *plain value/reason* (not a capability-gap signal), so a case
//! would be a `mismatch`; they are recorded here instead.
//!
//! * **`iolist_size/1` beyond the 60-bit small range** — unreachable: an iolist's size is the
//!   sum of the *actual* byte lengths of the terms it references, so any size ≥ 2^59 needs at
//!   least 2^59 byte-references (≥ 2^63 bytes of cons cells). The C source says the same
//!   (`erts/emulator/beam/erl_iolist.c:938-948`: "the only way to reach it is to create
//!   a large binary and reference it in absurdum in the same iolist"). The small-range path
//!   agrees (e.g. `iolist_size([<<0>> || _ <- lists:seq(1,10000)])` ⇒ `10000`); a 200 000-
//!   element shared-binary list — still nine orders of magnitude short of the boundary —
//!   already exceeds the Rust evaluator's memory.

use super::{Case, CaseKind, Contract, Golden, ModuleRun, Probe};

/// The module both runtimes execute for the compiled UTF-32 cases.
///
/// Every exported function returns **one integer** (the corpus-wide module-run golden
/// shape), folding the bytes it produced into that integer most-*significant* byte first:
/// `pack/1` over a four-byte construction, `binary:decode_unsigned/1` over the twelve bytes
/// the comprehension cases produce. So a single wrong byte (or a wrong byte *order*) changes
/// the golden, and the `utf32`-specific behaviour is what the integer carries.
///
/// `id/1` keeps every literal subject and generator **opaque**: the Erlang compiler must not
/// constant-fold a construction or a `case` match into a literal, because the lowered
/// `bs_create_bin`/`bs_match` arms are exactly what these cases exist to exercise.
pub(crate) const UTF32_SOURCE: &str = "-module(diff_utf32).\n\
     -export([little/1, big/1, native_relation/1, native_roundtrip/1,\n\
     \x20        scalar_range/0, little_comprehension/0,\n\
     \x20        little_binary_comprehension/0, little_match/0, big_match/0]).\n\
     \n\
     %% pack/1 folds a produced four-byte image into one integer, most significant byte\n\
     %% first, so a golden pins the byte order exactly (the bytes 52,18,0,0 of\n\
     %% <<4660/utf32-little>> pack to 873594880).\n\
     pack(<<A, B, C, D>>) -> ((A * 256 + B) * 256 + C) * 256 + D.\n\
     \n\
     %% id/1 keeps a literal subject or generator opaque, so the compiler cannot\n\
     %% constant-fold the construction/match away: the lowered opcodes are the point.\n\
     id(X) -> X.\n\
     \n\
     little(E) -> pack(<<E/utf32-little>>).\n\
     big(E) -> pack(<<E/utf32>>).\n\
     \n\
     native_relation(E) ->\n\
     \x20   Native = <<E/utf32-native>>,\n\
     \x20   Expected = case erlang:system_info(endian) of\n\
     \x20                  little -> <<E/utf32-little>>;\n\
     \x20                  big -> <<E/utf32>>\n\
     \x20              end,\n\
     \x20   flag(Native =:= Expected).\n\
     \n\
     native_roundtrip(E) ->\n\
     \x20   case id(<<E/utf32-native>>) of\n\
     \x20       <<X/utf32-native>> -> X\n\
     \x20   end.\n\
     \n\
     flag(true) -> 1;\n\
     flag(false) -> 0.\n\
     \n\
     %% One bit per value: 1 when the construction raised badarg, 0 when it produced a\n\
     %% 32-bit segment, 2 when it silently produced something else (never expected).\n\
     badarg_flag(V) ->\n\
     \x20   try bit_size(<<V/utf32>>) of\n\
     \x20       32 -> 0;\n\
     \x20       _ -> 2\n\
     \x20   catch error:_ -> 1 end.\n\
     \n\
     scalar_range() ->\n\
     \x20   (badarg_flag(16#D800) bsl 4)\n\
     \x20   bor (badarg_flag(16#DFFF) bsl 3)\n\
     \x20   bor (badarg_flag(16#110000) bsl 2)\n\
     \x20   bor (badarg_flag(-1) bsl 1)\n\
     \x20   bor (1 - badarg_flag(16#10FFFF)).\n\
     \n\
     little_comprehension() ->\n\
     \x20   binary:decode_unsigned(iolist_to_binary([<<X/utf32-little>> || X <- id([1, 4660, 16#10FFFF])])).\n\
     \n\
     little_binary_comprehension() ->\n\
     \x20   binary:decode_unsigned(<< <<X/utf32-little>> || X <- id([1, 4660, 16#10FFFF]) >>).\n\
     \n\
     little_match() ->\n\
     \x20   case id(<<172, 32, 0, 0>>) of\n\
     \x20       <<X/utf32-little>> -> X\n\
     \x20   end.\n\
     \n\
     big_match() ->\n\
     \x20   case id(<<0, 0, 32, 172>>) of\n\
     \x20       <<X/utf32>> -> X\n\
     \x20   end.\n";

/// Build one compiled UTF-32 module-run case over [`UTF32_SOURCE`]; `golden` is the oracle's
/// own `~w` rendering of the returned integer.
fn utf32_case(
    id: &'static str,
    description: &'static str,
    function: &'static str,
    arity: u32,
    args: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        // A compiled `.beam` executes (A.2) *and* the language's bit-string semantics are
        // what the case pins (A.1); the registry requires the `.beam` claim of a
        // module-run case.
        contracts: &[Contract::BeamAsm, Contract::LanguageSemantics],
        kind: CaseKind::Module(ModuleRun {
            source: UTF32_SOURCE,
            module: "diff_utf32",
            function,
            arity,
            args,
            nif: None,
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The file's two halves: the A.7 `binary` compiled-pattern cases and the A.2/A.1 compiled
/// UTF-32 bit-string cases.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "binary/compile_pattern_bm",
            description: "compile_pattern/1 of a single byte-aligned binary is a {bm, Ref} handle",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<1,2,3>>), {element(1,P), is_reference(element(2,P)), tuple_size(P)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {bm,true,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/compile_pattern_ac",
            description: "compile_pattern/1 of a one-element list stays {bm, _}; more than one pattern is {ac, _}",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern([<<1,2,3>>]), P2 = binary:compile_pattern([<<1,2,3>>,<<4,5,6>>]), {element(1,P), element(1,P2), is_reference(element(2,P)), is_reference(element(2,P2)), tuple_size(P2)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {bm,ac,true,true,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/compile_pattern_badarg",
            description: "the rejected compile_pattern/1 arguments (empty, non-binary, improper, non-byte-aligned, integer) are badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Only the **reason** is observed: an uncaught `catch` would carry a stack
                // trace, whose frames are volatile.
                expr: "[begin try binary:compile_pattern(A) catch error:Reason -> Reason end end || A <- [<<>>, [], [<<1>>,65], [<<1,2>>|<<3,4>>], 65, [<<>>], <<1:3>>, [[<<1>>]], [65,66]]]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg]"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/match_compiled",
            description: "match/2,3 dispatch on a compiled {bm,_} and {ac,_} handle, including the {scope,_} option",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<\"bc\">>), P2 = binary:compile_pattern([<<\"bc\">>,<<\"ca\">>]), {binary:match(<<\"abcabc\">>, P), binary:match(<<\"abcabc\">>, P2), binary:match(<<\"abcabc\">>, P, [{scope,{2,4}}])} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{1,2},{1,2},{4,2}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/match_nomatch",
            description: "the no-match path of match/2 and matches/2 returns nomatch / the empty list, for a raw, a compiled and a pattern-list subject",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:match(<<\"abc\">>, <<\"z\">>), binary:match(<<\"abc\">>, binary:compile_pattern(<<\"z\">>)), binary:match(<<\"abc\">>, [<<\"z\">>]), binary:matches(<<\"abc\">>, <<\"z\">>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {nomatch,nomatch,nomatch,[]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/matches_compiled",
            description: "matches/2 on a compiled pattern yields every non-overlapping match, left to right",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<\"bc\">>), P2 = binary:compile_pattern([<<\"bc\">>,<<\"abc\">>]), {binary:matches(<<\"abcabc\">>, P), binary:matches(<<\"abcabc\">>, P2), binary:matches(<<>>, P)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[{1,2},{4,2}],[{0,3},{3,3}],[]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/split_compiled",
            description: "split/2,3 on a compiled {bm,_}/{ac,_} handle: first-only by default, global/trim via options, subject unchanged when there is no match",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin P = binary:compile_pattern(<<\",\">>), P2 = binary:compile_pattern([<<\",\">>,<<\"b\">>]), {binary:split(<<\"a,b,c\">>, P), binary:split(<<\"a,b,c\">>, P, [global]), binary:split(<<\"a,b,c\">>, P, [global,trim]), binary:split(<<\"a,b,c\">>, P2, [global]), binary:split(<<\"abc\">>, P)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>],[<<97,98,99>>]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/split_plain",
            description: "the plain (uncompiled) split/2,3 path agrees with the compiled one for a binary and for a pattern list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:split(<<\"a,b,c\">>, <<\",\">>), binary:split(<<\"a,b,c\">>, <<\",\">>, [global]), binary:split(<<\"a,b,c\">>, [<<\",\">>,<<\"b\">>], [global])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>]}"),
                exit_code: 0,
            },
        },
        // --- the plain `binary` module surface (encoding, slicing, hex, sub-binaries) --------
        Case {
            id: "binary/encode_hex_roundtrip",
            description: "encode_hex/1,2 (upper/lower) and decode_hex/1 round trip byte-for-byte",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:encode_hex(<<1,2,255>>), binary:encode_hex(<<1,2,255>>, lowercase), binary:decode_hex(<<48,49,48,50,102,102>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<48,49,48,50,70,70>>,<<48,49,48,50,102,102>>,<<1,2,255>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/at_first_last",
            description: "binary:at/2, first/1 and last/1 index the documented ends",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:at(<<1,2,3>>, 1), binary:last(<<1,2,9>>), binary:first(<<8,9,10>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,9,8}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/copy_and_bitsize",
            description: "binary:copy/2 repeats a subject (count 0 is empty) and bit_size/1 counts bits",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // `bit_size/1` is a BIF that counts **bits** (24 here), not bytes.
                expr: "{binary:copy(<<>>, 5), binary:copy(<<1,2>>, 3), bit_size(<<1,2,3>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<>>,<<1,2,1,2,1,2>>,24}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/part_split_list_to_bin",
            description: "binary:part/3 slicing, list_to_bin/1 and bin_to_list/1,3 agree",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{{binary:part(<<1,2,3,4>>, 0, 2), binary:part(<<1,2,3,4>>, 2, 2)}, binary:list_to_bin([1,2,3]), binary:bin_to_list(<<1,2,3>>), binary:bin_to_list(<<1,2,3>>, 1, 2)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{<<1,2>>,<<3,4>>},<<1,2,3>>,[1,2,3],[2,3]}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/longest_common",
            description: "binary:longest_common_prefix/1 and longest_common_suffix/1 agree",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:longest_common_prefix([<<1,2,3>>, <<1,2,4>>]), binary:longest_common_suffix([<<1,2,3>>, <<0,2,3>>])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {2,2}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/sub_binary_patterns",
            description: "the sub-binary match patterns <<B:N/binary, Rest/binary>> agree at two sizes",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // A sub-binary pattern yields a *reference into* the same binary; the shapes
                // (and the tail split) are what an observable program depends on.
                expr: "{begin <<A:4/binary, B/binary>> = <<1,2,3,4,5,6>>, {A, B} end, begin <<D:2/binary, E/binary>> = <<2,3,4>>, {D, E} end}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {{<<1,2,3,4>>,<<5,6>>},{<<2,3>>,<<4>>}}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/replace_family",
            description: "binary:replace/3,4: first-only by default, [global], a literal with no match, and a pattern list",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:replace(<<\"abcabc\">>, <<\"b\">>, <<\"X\">>), binary:replace(<<\"abcabc\">>, <<\"b\">>, <<\"X\">>, [global]), binary:replace(<<\"abc\">>, <<\"z\">>, <<\"X\">>), binary:replace(<<\"abcabc\">>, [<<\"b\">>,<<\"c\">>], <<\"X\">>, [global])}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<97,88,99,97,98,99>>,<<97,88,99,97,88,99>>,<<97,98,99>>,<<97,88,88,97,88,88>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/badarg_surface",
            description: "the rejected binary-argument shapes (out-of-range index, over-long part, odd hex, non-byte-aligned, negative count, non-binary subject) are badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Only the **reason** is observed for each call (a bare `catch` would embed a
                // volatile stack trace).
                expr: "[begin try binary:at(<<1,2>>, 5) catch error:R1 -> R1 end end, begin try binary:part(<<1,2>>, 0, 3) catch error:R2 -> R2 end end, begin try binary:decode_hex(<<48,49,48>>) catch error:R3 -> R3 end end, begin try binary:encode_hex(<<1:3>>) catch error:R4 -> R4 end end, begin try binary:copy(<<1,2>>, -1) catch error:R5 -> R5 end end, begin try binary:at(foo, 0) catch error:R6 -> R6 end end]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [badarg,badarg,badarg,badarg,badarg,badarg]"),
                exit_code: 0,
            },
        },
        // --- bignum values in bit-string construction (`bs_create_bin`/`bs_init` segments) ---
        // An integer segment of an explicit width takes the value's **low `Size` bits** — the
        // oracle truncates rather than raising, even for a bignum far outside the segment and
        // even for `unsigned` with a negative value (`<<256>>` is `<<0>>`). The only
        // `badarg`s here come from an invalid *size* (negative), a non-byte-aligned
        // `decode_unsigned/1` subject and a negative `encode_unsigned/1` value; the
        // `binary/bignum_segment_size_badarg` case pins those. `size/1` cannot size such a
        // segment — it rejects an integer (`size(1 bsl 100)` is `badarg`, which escapes the
        // construction as `badarg`; see `binary/bignum_segment_sized_by_size_is_badarg`), so the
        // derived sizing uses `bit_size(binary:encode_unsigned(N))` instead.
        Case {
            id: "binary/bignum_truncated_into_byte_segment",
            description: "a bignum value truncates to the low 8 bits of an 8-bit segment (2 bsl 64 lands as 0)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin <<X:8>> = <<(2 bsl 64):8>>, X end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 0"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_truncated_into_word_segment",
            description: "a bignum value truncates to the low 32 bits of a 32-bit segment (5 bsl 100 lands as 0)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin <<X:32>> = <<(5 bsl 100):32>>, X end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok 0"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_little_endian_truncation",
            description: "a bignum in a 16-bit little-endian segment keeps only the low 16 bits, least-significant byte first",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "<<(256 bsl 200):16/little>>",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<0,0>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_little_endian_low_bits_kept",
            description: "the low 16 bits of a bignum survive a little-endian segment (0xABCD -> <<205,171>>)",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "<<((256 bsl 200) bor 16#ABCD):16/little>>",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok <<205,171>>"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/negative_and_bignum_signed_unsigned_bytes",
            description: "signed/unsigned 8-bit segments fed by -1 and by big-shifted bignums all reduce to a single byte",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{<<(-1):8/signed>>, <<(255 bsl 64):8/unsigned>>, <<(-(1 bsl 100)):8/signed>>}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<255>>,<<0>>,<<0>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/negative_bignum_signed_little_segment",
            description: "a negative bignum in a signed little-endian 16-bit segment decodes back to -1",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin <<X:16/signed-little>> = <<(-(1 bsl 100)-1):16/signed-little>>, X end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok -1"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_into_wide_segment",
            description: "an in-range bignum fills a 72-bit segment exactly, and decode_unsigned/1 recovers it",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{<<(2 bsl 64):72>>, binary:decode_unsigned(<<(2 bsl 64):72>>)}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<2,0,0,0,0,0,0,0,0>>,36893488147419103232}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_segment_sized_by_derived_bit_size",
            description: "a segment sized by bit_size(binary:encode_unsigned(N)) of a 101-bit bignum holds N byte-for-byte",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin N = 1 bsl 100, {<<N:(bit_size(binary:encode_unsigned(N)))>>, binary:encode_unsigned(N)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<16,0,0,0,0,0,0,0,0,0,0,0,0>>,<<16,0,0,0,0,0,0,0,0,0,0,0,0>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_truncated_to_sub_byte_segments",
            description: "a bignum truncated to one bit, a 65-bit segment sized by a bignum, and an unsized 256 all agree",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{<<(2 bsl 64):1>>, bit_size(<<(1 bsl 100):65>>), <<256>>}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {<<0:1>>,65,<<0>>}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_masked_segment_roundtrip",
            description: "a bignum OR-ed with a 32-bit mask in a 64-bit segment round trips big- and little-endian",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "begin B = <<((2 bsl 64) bor 16#DEADBEEF):64>>, {binary:decode_unsigned(B), binary:decode_unsigned(B, little)} end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {3735928559,17275436389634146304}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/decode_unsigned_of_bignum_derived_binaries",
            description: "decode_unsigned/1,2 of a 64-bit all-ones image and of binary:encode_unsigned/1 of a 101-bit bignum",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                expr: "{binary:decode_unsigned(<<(-1):64/signed-little>>), binary:decode_unsigned(<<(-1):64/signed-little>>, little), binary:decode_unsigned(binary:encode_unsigned(1 bsl 100))}",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok {18446744073709551615,18446744073709551615,1267650600228229401496703205376}"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_segment_size_badarg",
            description: "a negative segment size, a non-byte-aligned decode_unsigned subject and a negative encode_unsigned value are badarg",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // Only the **reason** is observed for each call (a bare `catch` would embed a
                // volatile stack trace).
                expr: "[begin try <<(1 bsl 100):(-1)>> catch error:ReasonA -> ReasonA end end, begin try binary:decode_unsigned(<<1:1>>) catch error:ReasonB -> ReasonB end end, begin try binary:encode_unsigned(-1) catch error:ReasonC -> ReasonC end end]",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok [badarg,badarg,badarg]"),
                exit_code: 0,
            },
        },
        Case {
            id: "binary/bignum_segment_sized_by_size_is_badarg",
            description: "sizing a segment with size/1 of a bignum raises badarg, since size/1 rejects an integer",
            contracts: &[Contract::OtpApps],
            kind: CaseKind::Eval {
                // The segment-size expression is evaluated as part of the construction, so
                // `size/1`'s own badarg escapes as the construction's badarg.
                expr: "begin try <<(1 bsl 100):(size(1 bsl 100))>> catch error:ReasonA -> ReasonA end end",
                extra_argv: &[],
                probe: Probe::Value,
            },
            golden: Golden {
                stdout: Some("ok badarg"),
                exit_code: 0,
            },
        },
        // --- compiled UTF-32 bit-string construction and matching (`binary/utf32_*`) --------
        // These are `CaseKind::Module` cases: the module above is `erlc`-compiled and run by
        // both runtimes, so the engine's lowered `bs_create_bin`/`bs_match` UTF-32 arms are
        // exercised — not the `-eval` evaluator the `bits/` group covers. The golden is the
        // oracle's `~w` rendering of the returned integer; `pack/1` folds the produced four
        // bytes into it most-significant byte first, so `<<4660/utf32-little>>`
        // (= `<<52,18,0,0>>`) is `873594880` and `<<4660/utf32>>` (= `<<0,0,18,52>>`) is
        // `4660`.
        utf32_case(
            "binary/utf32_little_construction",
            "a compiled <<E/utf32-little>> segment puts 16#1234 out as the bytes 52,18,0,0 (packed into 873594880)",
            "little",
            1,
            &[0x1234],
            "873594880",
        ),
        utf32_case(
            "binary/utf32_big_construction",
            "a compiled <<E/utf32>> (big-endian) segment puts 16#1234 out as the bytes 0,0,18,52 (packed back into 4660)",
            "big",
            1,
            &[0x1234],
            "4660",
        ),
        utf32_case(
            "binary/utf32_native_construction",
            "a compiled <<E/utf32-native>> segment equals the endian-appropriate explicit construction on this host (the relation is pinned, not a byte order)",
            "native_relation",
            1,
            &[0x1234],
            "1",
        ),
        utf32_case(
            "binary/utf32_scalar_range_badarg",
            "a compiled utf32 construction rejects the surrogates 16#D800/16#DFFF, the out-of-range 16#110000 and the negative -1 with badarg, and still accepts 16#10FFFF",
            "scalar_range",
            0,
            &[],
            "31",
        ),
        utf32_case(
            "binary/utf32_little_comprehension",
            "a compiled list comprehension of <<X/utf32-little>> over 1, 16#1234 and 16#10FFFF produces the 12-byte image 01 00 00 00 34 12 00 00 FF FF 10 00",
            "little_comprehension",
            0,
            &[],
            "309485013573406512572731392",
        ),
        utf32_case(
            "binary/utf32_little_binary_comprehension",
            "the same three segments through a compiled binary comprehension (the writable accumulator) produce the same 12-byte image as the list comprehension",
            "little_binary_comprehension",
            0,
            &[],
            "309485013573406512572731392",
        ),
        utf32_case(
            "binary/utf32_little_match",
            "a compiled case match <<X/utf32-little>> over the bytes 172,32,0,0 reads back 16#20AC",
            "little_match",
            0,
            &[],
            "8364",
        ),
        utf32_case(
            "binary/utf32_big_match",
            "a compiled case match <<X/utf32>> over the bytes 0,0,32,172 reads back 16#20AC",
            "big_match",
            0,
            &[],
            "8364",
        ),
        utf32_case(
            "binary/utf32_native_match",
            "a compiled <<E/utf32-native>> construction matched back with <<X/utf32-native>> round trips the code point on either endianness",
            "native_roundtrip",
            1,
            &[0x20AC],
            "8364",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the `binary` group: every *eval* case is a `value` case that observes the
    //! `binary` module through a golden the oracle printed, every *module* case is a compiled
    //! UTF-32 case whose integer golden is the byte image it produced, and **no** case ever
    //! renders a per-VM-start handle.
    use super::{group, CaseKind, Contract, Golden, Probe, UTF32_SOURCE};

    #[test]
    fn every_eval_case_is_a_binary_case_exposing_the_otp_app_contract() {
        let mut eval_cases = 0;
        for case in group() {
            let CaseKind::Eval {
                expr,
                extra_argv,
                probe,
            } = case.kind
            else {
                continue;
            };
            eval_cases += 1;
            assert_eq!(case.contracts, &[Contract::OtpApps], "case {}", case.id);
            assert!(
                case.id.starts_with("binary/"),
                "case {} is outside the binary group",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert_eq!(probe, Probe::Value, "case {}", case.id);
            assert!(
                extra_argv.is_empty(),
                "case {} takes no extra argv",
                case.id
            );
            assert!(!expr.is_empty() && !expr.contains('\n'), "case {}", case.id);
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert!(eval_cases > 0, "the group lost its eval cases");
    }

    #[test]
    fn every_module_case_is_a_compiled_utf32_case_exposing_language_semantics() {
        let mut module_cases = 0;
        for case in group() {
            let CaseKind::Module(run) = &case.kind else {
                continue;
            };
            module_cases += 1;
            assert!(
                case.id.starts_with("binary/utf32_"),
                "case {}: a module-run case here is a compiled UTF-32 case",
                case.id
            );
            assert_eq!(
                case.contracts,
                &[Contract::BeamAsm, Contract::LanguageSemantics],
                "case {}",
                case.id
            );
            assert!(case.description.contains("compiled"), "case {}", case.id);
            assert_eq!(run.module, "diff_utf32", "case {}", case.id);
            assert_eq!(run.source, UTF32_SOURCE, "case {}", case.id);
            assert!(run.nif.is_none(), "case {}", case.id);
            assert_eq!(case.kind_label(), "module-run", "case {}", case.id);
            assert_eq!(case.expr(), None, "case {}", case.id);
            assert!(case.extra_argv().is_empty(), "case {}", case.id);
            assert_eq!(
                run.arity as usize,
                run.args.len(),
                "case {}: arity must match the argument count",
                case.id
            );
            // A module-run golden is a plain integer line, and every UTF-32 case's integer is
            // the byte image it produced (see `the_utf32_goldens_are_the_documented_byte_images`).
            let golden = case.golden.stdout.expect("a golden");
            assert!(
                golden.chars().all(|c| c.is_ascii_digit()),
                "case {}: the golden is a plain integer line, got {golden:?}",
                case.id
            );
            assert_eq!(golden, golden.trim_start_matches('0'), "case {}", case.id);
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert_eq!(module_cases, 9, "the compiled UTF-32 case count changed");
    }

    #[test]
    fn the_utf32_goldens_are_the_documented_byte_images() {
        // The integer goldens are the oracle's own `~w` lines, and each one is the produced
        // bytes read most-significant byte first — so the byte order a case pins is checkable
        // in arithmetic instead of by eye.
        for (id, golden) in [
            ("binary/utf32_little_construction", 873_594_880u128),
            ("binary/utf32_big_construction", 4_660),
            ("binary/utf32_native_construction", 1),
            ("binary/utf32_scalar_range_badarg", 31),
            (
                "binary/utf32_little_comprehension",
                309_485_013_573_406_512_572_731_392,
            ),
            (
                "binary/utf32_little_binary_comprehension",
                309_485_013_573_406_512_572_731_392,
            ),
            ("binary/utf32_little_match", 8_364),
            ("binary/utf32_big_match", 8_364),
            ("binary/utf32_native_match", 8_364),
        ] {
            let case = group()
                .into_iter()
                .find(|c| c.id == id)
                .expect("registered");
            assert_eq!(
                case.golden.stdout.and_then(|g| g.parse::<u128>().ok()),
                Some(golden),
                "{id}"
            );
        }
        // `<<4660/utf32-little>>` is `<<52,18,0,0>>`, i.e. `16#34120000`; the big-endian
        // segment `<<0,0,18,52>>` packs back to the value itself.
        assert_eq!(
            873_594_880u128,
            u128::from(u32::from_be_bytes([52, 18, 0, 0]))
        );
        assert_eq!(4_660u128, u128::from(u32::from_be_bytes([0, 0, 18, 52])));
        assert_eq!(4_660u128, u128::from(0x1234u32));
        // Both comprehension shapes produce the same twelve bytes — `1`, `16#1234` and
        // `16#10FFFF`, each as a little-endian utf32 segment: 01 00 00 00 34 12 00 00 FF FF 10 00.
        assert_eq!(
            309_485_013_573_406_512_572_731_392u128,
            u128::from_be_bytes([0, 0, 0, 0, 1, 0, 0, 0, 52, 18, 0, 0, 255, 255, 16, 0])
        );
        // `16#20AC` is the code point every match case reads back.
        assert_eq!(8_364u128, u128::from(0x20ACu32));
        // The scalar-range bitmask: the four rejects (16#D800, 16#DFFF, 16#110000, -1) plus the
        // accepted `16#10FFFF` control.
        assert_eq!(31u128, 0b1_1111);
    }

    #[test]
    fn the_utf32_source_keeps_every_construction_and_match_shape() {
        for shape in [
            "<<E/utf32-little>>",
            "<<E/utf32>>",
            "<<E/utf32-native>>",
            "<<X/utf32-little>>",
            "<<X/utf32>>",
            "<<X/utf32-native>>",
            "[<<X/utf32-little>> || X <-",
            "<< <<X/utf32-little>> || X <-",
            "erlang:system_info(endian)",
            "16#D800",
            "16#DFFF",
            "16#110000",
            "badarg_flag(-1)",
            "id([1, 4660, 16#10FFFF])",
        ] {
            assert!(UTF32_SOURCE.contains(shape), "the source lost {shape:?}");
        }
        // The literal subjects and generators stay behind `id/1`: the compiler must not
        // constant-fold a construction or a `case` match into a literal, which would leave the
        // lowered opcode path unexercised while the case stayed green.
        assert!(UTF32_SOURCE.contains("id(X) -> X."));
        for subject in [
            "case id(<<E/utf32-native>>)",
            "case id(<<172, 32, 0, 0>>)",
            "case id(<<0, 0, 32, 172>>)",
        ] {
            assert!(
                UTF32_SOURCE.contains(subject),
                "the match subject is no longer opaque: {subject:?}"
            );
        }
    }

    #[test]
    fn the_compiled_handle_is_always_reduced_to_its_shape() {
        // The handle is a `Ref` whose id varies per VM start, so a golden that rendered it
        // would drift on the next run. Every compile_pattern case therefore projects it:
        // the tag with `element(1, …)` and the id with `is_reference/1`.
        for case in group() {
            let Some(golden) = case.golden.stdout else {
                panic!("case {} pins no golden", case.id);
            };
            assert!(
                !golden.contains("#Ref<"),
                "case {}: a raw handle leaked into the golden {golden:?}",
                case.id
            );
        }
        for id in ["binary/compile_pattern_bm", "binary/compile_pattern_ac"] {
            let case = group()
                .into_iter()
                .find(|c| c.id == id)
                .expect("registered");
            let expr = case.expr().expect("eval case");
            assert!(
                expr.contains("element(1,") && expr.contains("is_reference(element(2,"),
                "case {id}: the handle must be observed by shape, got {expr}"
            );
        }
    }

    #[test]
    fn the_group_pins_the_documented_tag_rule_and_badarg_set() {
        for (id, stdout) in [
            ("binary/compile_pattern_bm", "ok {bm,true,2}"),
            ("binary/compile_pattern_ac", "ok {bm,ac,true,true,2}"),
            (
                "binary/compile_pattern_badarg",
                "ok [badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg]",
            ),
            ("binary/match_compiled", "ok {{1,2},{1,2},{4,2}}"),
            (
                "binary/matches_compiled",
                "ok {[{1,2},{4,2}],[{0,3},{3,3}],[]}",
            ),
            (
                "binary/split_compiled",
                "ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>],[<<97,98,99>>]}",
            ),
            (
                "binary/split_plain",
                "ok {[<<97>>,<<98,44,99>>],[<<97>>,<<98>>,<<99>>],[<<97>>,<<>>,<<>>,<<99>>]}",
            ),
        ] {
            let case = group().into_iter().find(|c| c.id == id).expect("registered");
            assert_eq!(
                case.golden,
                Golden {
                    stdout: Some(stdout),
                    exit_code: 0
                },
                "{id}"
            );
        }
        // The `badarg` case really enumerates the rejected shapes: an empty binary, an empty
        // list, a non-binary element, an improper list, a bare integer, an empty element, a
        // non-byte-aligned bitstring, a nested list and a list of integers.
        let badarg = group()
            .into_iter()
            .find(|c| c.id == "binary/compile_pattern_badarg")
            .expect("registered");
        let expr = badarg.expr().expect("eval case");
        for rejected in [
            "<<>>",
            "[]",
            "[<<1>>,65]",
            "[<<1,2>>|<<3,4>>]",
            "65",
            "[<<>>]",
            "<<1:3>>",
            "[[<<1>>]]",
            "[65,66]",
        ] {
            assert!(
                expr.contains(rejected),
                "the badarg case no longer rejects {rejected:?}"
            );
        }
    }

    #[test]
    fn the_no_match_paths_are_pinned() {
        // Every no-match shape agrees with the oracle, so each is pinned directly: `nomatch`
        // for `match/2` (raw, compiled and pattern-list subjects) and the empty list for
        // `matches/2` on a compiled pattern and for `split/2` (subject unchanged).
        let cases = group();
        let nomatch = cases
            .iter()
            .find(|c| c.id == "binary/match_nomatch")
            .expect("registered");
        assert_eq!(
            nomatch.golden,
            Golden {
                stdout: Some("ok {nomatch,nomatch,nomatch,[]}"),
                exit_code: 0
            }
        );
        let matches = cases
            .iter()
            .find(|c| c.id == "binary/matches_compiled")
            .expect("registered");
        assert!(matches.golden.stdout.is_some_and(|g| g.ends_with(",[]}")));
        let split = cases
            .iter()
            .find(|c| c.id == "binary/split_compiled")
            .expect("registered");
        assert!(split
            .golden
            .stdout
            .is_some_and(|g| g.ends_with("[<<97,98,99>>]}")));
    }
}
