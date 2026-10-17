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
//! Contract **A.1/A.7** — the `-eval` route's bit-string surface.
//!
//! `beam_rt`'s `-eval` evaluator builds and matches bit strings itself
//! (`../../crates/beam_rt/src/eval/{bits,bits_decode,bits_match}.rs`) instead of going through
//! the compiler, so the family is pinned here as the *observable* image of a constructed or
//! matched segment: the bit string's own bytes and the matched field values, rendered with the
//! value probe (`~w`). The rows are those modules' own oracle tables, promoted out of
//! `#[cfg(test)]` so the **gate** (not only the crate's unit tests) pins them.
//!
//! Three shapes recur:
//!
//! * **construction** — an integer segment of any width in two's complement, big- and
//!   little-endian (including the byte-wise little-endian image whose trailing partial field
//!   sits *above* the whole bytes: `<<-300:12/little>>` is `<<212,14:4>>`), a bignum segment
//!   past 60 bits, an IEEE `float` at 16/32/64 bits (with the half-to-even rounding), a
//!   `utf8`/`utf16`/`utf32` segment (incl. a utf16 surrogate pair), a string literal, an
//!   unsized `binary`/`bitstring` segment, and a non-byte-aligned total size;
//! * **matching** — the same families read back, incl. `signed` sign extension *within* the
//!   segment width and the little-endian partial-byte reader;
//! * **the refusal rule** — `badarg` for a construction segment that cannot be encoded (a
//!   float width other than 16/32/64, a surrogate or an out-of-range code point, a non-number),
//!   and, for a **pattern**, the absolute rule that an invalid/truncated/out-of-range field is
//!   a **match failure** (`nomatch`), never a raise, on both sides.
//!
//! Two harness constraints shape every expression below:
//!
//! 1. the probe wrapper is `try (EXPR) of V -> …`, and the **oracle's** `erl_eval` cannot
//!    evaluate a `case` or a `<<…>>` pattern directly inside that `try` body (it exits 1 with
//!    empty stdout), so an expression carrying a pattern is an immediately-applied fun
//!    `(fun() -> … end)()` — and a multi-row case is one such fun returning a list of applied
//!    inner funs, one per row (`[F() || F <- [fun() -> … end, …]]`);
//! 2. the wrapper binds `V`, `C` and `R`, so no expression may bind those names (the caught
//!    reasons below are `Reason`/`W1`/`U1`/…).
//!
//! Every golden is a plain integer/binary/float/atom rendering — no pid/ref/port, no map, no
//! clock — so nothing is order- or VM-start-dependent.
use super::{Case, CaseKind, Contract, Golden, Probe};

/// One `bits/*` case: evaluate `expr` under both runtimes and pin the oracle's `~w` rendering.
fn case(
    id: &'static str,
    description: &'static str,
    contracts: &'static [Contract],
    expr: &'static str,
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts,
        kind: CaseKind::Eval {
            expr,
            extra_argv: &[],
            probe: Probe::Value,
        },
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The `bits/*` cases: construction, matching and the bit-op/size surface.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // -- little-endian integer construction -------------------------------------------
        case(
            "bits/construct_little_endian_partial_bytes",
            "a little-endian segment writes its whole bytes first and the trailing partial field above them",
            &[Contract::LanguageSemantics],
            "[<<-300:12/little>>, <<16#1234:12/little>>, <<256:9/little>>, <<255:12/little>>]",
            "ok [<<212,14:4>>,<<52,2:4>>,<<0,1:1>>,<<255,0:4>>]",
        ),
        case(
            "bits/construct_little_endian_byte_aligned",
            "the byte-aligned little-endian forms (explicit size, unit:8 and 16/32-bit) are the byte reversal",
            &[Contract::LanguageSemantics],
            "[<<16#1234:2/little-unit:8>>, <<1:2/unit:8>>, <<256:2/unit:8>>, <<256:16/little>>, <<16#1234:16/little>>, <<16#1234:32/little>>]",
            "ok [<<52,18>>,<<0,1>>,<<1,0>>,<<0,1>>,<<52,18>>,<<52,18,0,0>>]",
        ),
        case(
            "bits/construct_little_endian_narrow_fields",
            "a little-endian field narrower than a byte, and one starting at a bit offset, keep the field's low bits",
            &[Contract::LanguageSemantics],
            "[<<16#1234:4/little>>, <<16#1234:5/little>>, <<1:1,16#1234:12/little>>, <<-1:12/little>>]",
            "ok [<<4:4>>,<<20:5>>,<<154,2:5>>,<<255,15:4>>]",
        ),
        case(
            "bits/construct_bignum_segment_little_endian",
            "a little-endian segment past 60 bits is a bignum write: its low bits land first, the rest above",
            &[Contract::LanguageSemantics],
            "[<<(1 bsl 60):60/little>>, <<(1 bsl 64):72/little>>]",
            "ok [<<0,0,0,0,0,0,0,0:4>>,<<0,0,0,0,0,0,0,0,1>>]",
        ),
        case(
            "bits/construct_two_complement_integers",
            "a big-endian integer segment truncates to its low Size bits in two's complement, at any width",
            &[Contract::LanguageSemantics],
            "[<<-1:8>>, <<-1:12>>, <<-300:16>>, <<255:8>>, <<(1 bsl 63):64>>, <<-1:64>>, <<(1 bsl 100):112>>]",
            "ok [<<255>>,<<255,15:4>>,<<254,212>>,<<255>>,<<128,0,0,0,0,0,0,0>>,<<255,255,255,255,255,255,255,255>>,<<0,16,0,0,0,0,0,0,0,0,0,0,0,0>>]",
        ),
        // -- float segments ---------------------------------------------------------------
        case(
            "bits/construct_float_binary16_rounds_half_to_even",
            "a binary16 segment rounds an exact tie to the even mantissa, in both directions",
            &[Contract::LanguageSemantics],
            "[<<(1.0 + 1.0/2048):16/float>>, <<(1.0 + 3.0/2048):16/float>>, <<(1.0 + 1.0/2048):32/float>>]",
            "ok [<<60,0>>,<<60,2>>,<<63,128,16,0>>]",
        ),
        case(
            "bits/construct_float_binary16_extremes",
            "the binary16 segment's subnormal, overflow-to-infinity and negative-zero images",
            &[Contract::LanguageSemantics],
            "[<<0.1:16/float>>, <<3.14159:16/float>>, <<70000.0:16/float>>, <<1.0e10:16/float>>, <<5.0e-8:16/float>>, <<-0.0:16/float>>]",
            "ok [<<46,102>>,<<66,72>>,<<124,0>>,<<124,0>>,<<0,1>>,<<128,0>>]",
        ),
        case(
            "bits/construct_float_binary32_and_binary64",
            "the binary32/binary64 float segments, big- and little-endian, from a float and an integer value",
            &[Contract::LanguageSemantics],
            "[<<1.5:32/float>>, <<1.5:32/float-little>>, <<1.0e40:64/float>>, <<1.0e40:32/float>>, <<3.14159:32/float>>, <<(1 bsl 64):64/float>>, <<1.5/float>>, <<1.5/float-little>>]",
            "ok [<<63,192,0,0>>,<<0,0,192,63>>,<<72,61,99,41,241,195,92,165>>,<<127,128,0,0>>,<<64,73,15,208>>,<<67,240,0,0,0,0,0,0>>,<<63,248,0,0,0,0,0,0>>,<<0,0,0,0,0,0,248,63>>]",
        ),
        case(
            "bits/construct_float_width_must_be_16_32_or_64",
            "a float segment whose width is not 16/32/64 bits (or whose value is not a number) is badarg",
            &[Contract::LanguageSemantics],
            "[begin try <<1.5:8/float>> catch error:W1 -> W1 end end, begin try <<1.5:24/float>> catch error:W2 -> W2 end end, begin try <<1.5:128/float>> catch error:W3 -> W3 end end, begin try <<foo/float>> catch error:W4 -> W4 end end]",
            "ok [badarg,badarg,badarg,badarg]",
        ),
        // -- utf segments -----------------------------------------------------------------
        case(
            "bits/construct_utf8_segments",
            "a utf8 segment encodes 1..4 bytes by code-point range",
            &[Contract::LanguageSemantics],
            "[<<16#41/utf8>>, <<16#7FF/utf8>>, <<16#20AC/utf8>>, <<16#10FFFF/utf8>>, <<16#10000/utf8>>]",
            "ok [<<65>>,<<223,191>>,<<226,130,172>>,<<244,143,191,191>>,<<240,144,128,128>>]",
        ),
        case(
            "bits/construct_utf16_surrogate_pairs",
            "a utf16 segment emits a surrogate pair past 16#FFFF (big- and little-endian, native)",
            &[Contract::LanguageSemantics],
            "[<<16#41/utf16>>, <<16#41/utf16-little>>, <<16#41/utf16-native>>, <<16#7FF/utf16>>, <<16#10000/utf16>>, <<16#10FFFF/utf16>>, <<16#10000/utf16-little>>]",
            "ok [<<0,65>>,<<65,0>>,<<65,0>>,<<7,255>>,<<216,0,220,0>>,<<219,255,223,255>>,<<0,216,0,220>>]",
        ),
        case(
            "bits/construct_utf32_segments",
            "a utf32 segment is one 4-byte code point in either byte order",
            &[Contract::LanguageSemantics],
            "[<<1234/utf32>>, <<1234/utf32-little>>, <<16#10000/utf32>>, <<16#10FFFF/utf32>>, <<0/utf32>>, <<16#10000/utf32-little>>]",
            "ok [<<0,0,4,210>>,<<210,4,0,0>>,<<0,1,0,0>>,<<0,16,255,255>>,<<0,0,0,0>>,<<0,0,1,0>>]",
        ),
        case(
            "bits/construct_utf_out_of_range_is_badarg",
            "a surrogate, an out-of-range or negative code point, and a non-integer value are badarg on every utf segment",
            &[Contract::LanguageSemantics],
            "[begin try <<16#D800/utf16>> catch error:U1 -> U1 end end, begin try <<16#110000/utf16>> catch error:U2 -> U2 end end, begin try <<16#D800/utf32>> catch error:U3 -> U3 end end, begin try <<16#110000/utf32>> catch error:U4 -> U4 end end, begin try <<-1/utf8>> catch error:U5 -> U5 end end, begin try <<16#D800/utf8>> catch error:U6 -> U6 end end, begin try <<atom/utf16>> catch error:U7 -> U7 end end, begin try <<[1040]/utf16>> catch error:U8 -> U8 end end]",
            "ok [badarg,badarg,badarg,badarg,badarg,badarg,badarg,badarg]",
        ),
        // -- string / binary / sized segments ---------------------------------------------
        case(
            "bits/construct_string_literal_segments",
            "a string literal is one segment per character, and the utf types encode each character",
            &[Contract::LanguageSemantics],
            "[<<\"abc\">>, <<\"abc\":16>>, <<\"abc\"/utf8>>, <<\"abc\"/utf16>>, <<\"abc\"/utf16-little>>, <<\"abc\"/utf32>>, <<>>]",
            "ok [<<97,98,99>>,<<0,97,0,98,0,99>>,<<97,98,99>>,<<0,97,0,98,0,99>>,<<97,0,98,0,99,0>>,<<0,0,0,97,0,0,0,98,0,0,0,99>>,<<>>]",
        ),
        case(
            "bits/construct_unsized_binary_and_bitstring_segments",
            "an unsized binary/bitstring segment splices its value's bits in place, at any alignment",
            &[Contract::LanguageSemantics],
            "[<<(<<1,2,3>>)/binary>>, <<1,(<<2,3>>)/binary>>, <<(<<1:1>>)/bitstring,(<<0:7>>)/bitstring>>, <<(<<1:4>>)/bitstring,2:4>>]",
            "ok [<<1,2,3>>,<<1,2,3>>,<<128>>,<<18>>]",
        ),
        case(
            "bits/construct_non_byte_aligned_sizes",
            "a total size that is not a whole number of bytes is a non-byte-aligned bit string",
            &[Contract::LanguageSemantics, Contract::OtpApps],
            "[<<1,2,3:5>>, bit_size(<<1,2,3:5>>), <<1:4,2:8,3:4>>, <<1:3,2:7>>]",
            "ok [<<1,2,3:5>>,21,<<16,35>>,<<32,2:2>>]",
        ),
        // -- integer matching -------------------------------------------------------------
        case(
            "bits/match_integer_wide_segments",
            "an integer pattern segment reads any width exactly, incl. 72/120-bit bignum fields",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<255,255,255,255,255,255,255,255>> of <<A:64>> -> A end end, fun() -> case <<255,255,255,255,255,255,255,255>> of <<A:64/little>> -> A end end, fun() -> case <<1,2,3,4,5,6,7,8>> of <<A:64/little>> -> A end end, fun() -> case <<255,255,255,255,255,255,255,255,255>> of <<A:72>> -> A end end, fun() -> case <<0,0,0,0,0,3,141,0,0,0,0,0,0,0,0>> of <<A:120>> -> A end end, fun() -> case <<1,2,3,4,5,6,7,8,9>> of <<A:72/little>> -> A end end]] end)()",
            "ok [18446744073709551615,18446744073709551615,578437695752307201,4722366482869645213695,16768090363001982418944,166599134359138271745]",
        ),
        case(
            "bits/match_integer_signed_extension",
            "signed sign-extends within the segment width (12, 60 and 64 bits) and leaves the unsigned read alone",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<255,255,255,255,255,255,255,255>> of <<A:64/signed>> -> A end end, fun() -> case <<16#80,0,0,0,0,0,0,0>> of <<A:64/signed>> -> A end end, fun() -> case <<(1 bsl 59):60>> of <<A:60/signed>> -> A end end, fun() -> case <<(1 bsl 59):60>> of <<A:60>> -> A end end, fun() -> case <<255,15:4>> of <<A:12/signed>> -> A end end, fun() -> case <<2,2:4>> of <<A:12/signed>> -> A end end, fun() -> case <<16#7FF:12>> of <<A:12/signed>> -> A end end]] end)()",
            "ok [-1,-9223372036854775808,-576460752303423488,576460752303423488,-1,34,2047]",
        ),
        case(
            "bits/match_integer_little_endian_partial",
            "a little-endian pattern segment reads the whole bytes first and the trailing partial field above them",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<212,14:4>> of <<A:12/little>> -> A end end, fun() -> case <<212,14:4>> of <<A:12/signed-little>> -> A end end, fun() -> case <<0,1:1>> of <<A:9/little>> -> A end end, fun() -> case <<1,2,3>> of <<A:24/little>> -> A end end, fun() -> case <<255,255>> of <<A:16/signed-little>> -> A end end, fun() -> case <<10:4>> of <<A:4/little>> -> A end end, fun() -> case <<-300:12/little>> of <<A:12/little>> -> A end end]] end)()",
            "ok [3796,-300,256,197121,-1,10,3796]",
        ),
        case(
            "bits/match_integer_little_endian_past_a_word",
            "the little-endian reader is exact past a word and off byte alignment, signed and unsigned",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1,2,3,4,5,6,7,8,9,10>> of <<_:3,A:77/little>> -> A end end, fun() -> case <<1,2,3,4,5,6,7,8,9,10>> of <<_:3,A:77/signed-little>> -> A end end, fun() -> case <<255,255,255,255,255,255,255,255,255,255>> of <<_:3,A:77/signed-little>> -> A end end, fun() -> case <<1,2,3,4,5,6,7,8,9,10>> of <<_:5,A:75>> -> A end end, fun() -> case <<1,2,3,4,5,6,7,8,9,10>> of <<_:5,A:75/signed>> -> A end end, fun() -> case <<255,255,255,255,255,255,255,255,255,255>> of <<_:1,A:79/signed>> -> A end end]] end)()",
            "ok [48556457903569558310920,48556457903569558310920,-1,4759477275222530853130,4759477275222530853130,-1]",
        ),
        case(
            "bits/match_integer_zero_width_and_bit_offsets",
            "a zero-width field matches empty, and fields read correctly from a non-byte-aligned subject",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<>> of <<A:0>> -> A end end, fun() -> case <<1:1,16#12,16#34>> of <<_:1,A:16>> -> A end end, fun() -> case <<1:1,16#12,16#34>> of <<_:1,A:8,Rest/binary>> -> {A,Rest} end end, fun() -> case <<0:4,(<<255:12/little>>)/bitstring>> of <<_:4,A:12/little>> -> A end end, fun() -> case <<0:4,(<<3796:12>>)/bitstring>> of <<_:4,A:12/little>> -> A; _ -> nomatch end end, fun() -> case <<1:3,16#12,16#34>> of <<_:1,A:2,_/bitstring>> -> A end end]] end)()",
            "ok [0,4660,{18,<<52>>},255,1261,1]",
        ),
        // -- float matching ---------------------------------------------------------------
        case(
            "bits/match_float_widths",
            "a float pattern segment decodes the IEEE field at 16/32/64 bits, reversed by little/native",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1.5:16/float>> of <<F:16/float>> -> F end end, fun() -> case <<0,62>> of <<F:16/float>> -> F end end, fun() -> case <<1.5:32/float>> of <<F:32/float>> -> F end end, fun() -> case <<1.5:32/float>> of <<F:32/float-little>> -> F end end, fun() -> case <<1,2,3,4,5,6,7,8>> of <<F:16/float,Rest/binary>> -> {F,Rest} end end, fun() -> case <<1.5/float>> of <<F/float>> -> F end end]] end)()",
            "ok [1.5,3.6954879760742188e-6,1.5,6.896490392174587e-41,{1.537799835205078e-5,<<3,4,5,6,7,8>>},1.5]",
        ),
        case(
            "bits/match_float_width_outside_16_32_64_is_a_match_failure",
            "a float pattern width that is not 16/32/64 bits is a match failure, never a raise",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1.5:64/float>> of <<F:16/float>> -> F; _ -> nomatch end end, fun() -> case <<0,0,0,0,0,0,0,0>> of <<F:24/float,Rest/binary>> -> {F,Rest}; _ -> nomatch end end, fun() -> case <<0,0,0,0>> of <<F:24/float>> -> F; _ -> nomatch end end, fun() -> case <<0,0,0>> of <<F:32/float>> -> F; _ -> nomatch end end]] end)()",
            "ok [nomatch,nomatch,nomatch,nomatch]",
        ),
        // -- utf matching -----------------------------------------------------------------
        case(
            "bits/match_utf8_code_points",
            "a utf8 pattern segment decodes 1..4 byte sequences and leaves the rest of the subject",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<16#41/utf8>> of <<C/utf8>> -> C end end, fun() -> case <<16#E0,16#A0,16#80>> of <<C/utf8>> -> C end end, fun() -> case <<16#10FFFF/utf8>> of <<C/utf8>> -> C end end, fun() -> case <<16#E2,16#82,16#AC,65>> of <<C/utf8,Rest/binary>> -> {C,Rest} end end, fun() -> case <<16#F0,16#90,16#80,16#80>> of <<C/utf8>> -> C end end]] end)()",
            "ok [65,2048,1114111,{8364,<<65>>},65536]",
        ),
        case(
            "bits/match_malformed_utf8_is_a_match_failure",
            "an overlong, surrogate, out-of-range or truncated UTF-8 sequence is a match failure",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<16#C0,16#80>> of <<C/utf8>> -> C; _ -> nomatch end end, fun() -> case <<16#FF>> of <<C/utf8>> -> C; _ -> nomatch end end, fun() -> case <<16#ED,16#A0,16#80>> of <<C/utf8>> -> C; _ -> nomatch end end, fun() -> case <<16#E2,16#82>> of <<C/utf8>> -> C; _ -> nomatch end end, fun() -> case <<16#E2,16#41,16#AC>> of <<C/utf8>> -> C; _ -> nomatch end end, fun() -> case <<16#F4,16#90,16#80,16#80>> of <<C/utf8>> -> C; _ -> nomatch end end]] end)()",
            "ok [nomatch,nomatch,nomatch,nomatch,nomatch,nomatch]",
        ),
        case(
            "bits/match_utf16_surrogate_pairs",
            "a utf16 pattern segment decodes one unit or a surrogate pair, in either byte order",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<16#10000/utf16>> of <<C/utf16>> -> C end end, fun() -> case <<16#10FFFF/utf16-little>> of <<C/utf16-little>> -> C end end, fun() -> case <<255,219,255,223>> of <<C/utf16-little>> -> C end end, fun() -> case <<16#41/utf16>> of <<C/utf16-little>> -> C end end, fun() -> case <<16#41/utf16,65>> of <<C/utf16,Rest/binary>> -> {C,Rest} end end, fun() -> case <<16#41/utf16-native>> of <<C/utf16-native>> -> C end end]] end)()",
            "ok [65536,1114111,1114111,16640,{65,<<65>>},65]",
        ),
        case(
            "bits/match_lone_utf16_surrogate_is_a_match_failure",
            "a lone high/low surrogate and a mis-ordered pair are a match failure, never a raise",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<16#D8,0>> of <<C/utf16>> -> C; _ -> nomatch end end, fun() -> case <<16#DC,0>> of <<C/utf16>> -> C; _ -> nomatch end end, fun() -> case <<16#D8,0,16#41,0>> of <<C/utf16>> -> C; _ -> nomatch end end, fun() -> case <<255,255,219,255>> of <<C/utf16-little>> -> C; _ -> nomatch end end, fun() -> case <<255,223,255,219>> of <<C/utf16-little>> -> C; _ -> nomatch end end]] end)()",
            "ok [nomatch,nomatch,nomatch,nomatch,nomatch]",
        ),
        case(
            "bits/match_utf32_code_points",
            "a utf32 pattern segment decodes one 4-byte code point and leaves the rest of the subject",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1234/utf32>> of <<C/utf32>> -> C end end, fun() -> case <<16#FF,16#10,0,0>> of <<C/utf32-little>> -> C end end, fun() -> case <<0,0,0,65,255>> of <<C/utf32,Rest/binary>> -> {C,Rest} end end, fun() -> case <<0,0,0,0>> of <<C/utf32>> -> C end end, fun() -> case <<1234/utf32>> of <<C/utf32-signed>> -> C end end]] end)()",
            "ok [1234,4351,{65,<<255>>},0,1234]",
        ),
        case(
            "bits/match_utf32_out_of_range_is_a_match_failure",
            "a utf32 surrogate or a wrong byte order is a match failure, never a raise",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<0,0,16#D8,0>> of <<C/utf32>> -> C; _ -> nomatch end end, fun() -> case <<0,0,16#10,16#FF>> of <<C/utf32-little>> -> C; _ -> nomatch end end, fun() -> case <<255,255,255,255>> of <<C/utf32>> -> C; _ -> nomatch end end, fun() -> case <<1234/utf32>> of <<C/utf32>> -> C end end]] end)()",
            "ok [nomatch,nomatch,nomatch,1234]",
        ),
        // -- pattern size / specifiers ----------------------------------------------------
        case(
            "bits/match_truncated_pattern_is_a_match_failure",
            "a pattern field needing more bits than the subject holds is a match failure, never badarg",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1,2,3>> of <<A:32>> -> A; _ -> nomatch end end, fun() -> case <<1,2,3>> of <<A:24>> -> A end end, fun() -> case <<1,2,3:5>> of <<A:24>> -> A; _ -> nomatch end end, fun() -> case <<1,2,3,4>> of <<A:5/binary>> -> A; _ -> nomatch end end]] end)()",
            "ok [nomatch,66051,nomatch,nomatch]",
        ),
        case(
            "bits/match_unit_specifiers",
            "unit: resolves to size*unit bits on the read side, byte-aligned and little-endian",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<16#12,16#34>> of <<A:2/unit:8>> -> A end end, fun() -> case <<16#12,16#34>> of <<A:1/little-unit:8>> -> A; _ -> nomatch end end, fun() -> case <<1:1,16#12,16#34>> of <<_:1,A:16>> -> A end end, fun() -> case <<1:1,16#12,16#34>> of <<_:1,A:8,Rest/binary>> -> {A,Rest} end end]] end)()",
            "ok [4660,nomatch,4660,{18,<<52>>}]",
        ),
        case(
            "bits/match_string_literal_pattern",
            "a string literal in a pattern is one 8-bit segment per character",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<\"abc\">> of <<\"abc\">> -> yes; _ -> no end end, fun() -> case <<65,66,67>> of <<\"ABC\">> -> yes; _ -> no end end, fun() -> case <<1,\"abc\">> of <<X:8,\"abc\">> -> X end end, fun() -> case <<97,98>> of <<\"abc\">> -> yes; _ -> no end end]] end)()",
            "ok [yes,yes,1,no]",
        ),
        case(
            "bits/match_unsized_binary_and_bitstring_fields",
            "an unsized binary/bitstring pattern field takes the rest of the subject at any alignment",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1,2,3,4>> of <<A:2/binary,Rest/binary>> -> {A,Rest} end end, fun() -> case <<1,2,3,4>> of <<A:3/binary,Rest/bitstring>> -> {A,bit_size(Rest)} end end, fun() -> case <<1,2,3,4>> of <<A:5/binary>> -> A; _ -> nomatch end end, fun() -> case <<1,2,3:5>> of <<_:21,Rest/bitstring>> -> {bit_size(Rest)} end end]] end)()",
            "ok [{<<1,2>>,<<3,4>>},{<<1,2,3>>,8},nomatch,{0}]",
        ),
        case(
            "bits/match_non_byte_aligned_subject",
            "a non-byte-aligned subject matches integer fields and an empty rest at its own alignment",
            &[Contract::LanguageSemantics],
            "(fun() -> [F() || F <- [fun() -> case <<1,2,3:5>> of <<A:8,B:8,C:5>> -> {A,B,C} end end, fun() -> case <<>> of <<>> -> empty end end, fun() -> case <<1:3,2:7>> of <<A:10>> -> A end end, fun() -> N = 12, case <<212,14:4>> of <<A:N/little>> -> A end end]] end)()",
            "ok [{1,2,3},empty,130,3796]",
        ),
        // -- the bit operators and the size BIFs (through the registry) --------------------
        case(
            "bits/bit_ops_on_bignums",
            "band/bor/bxor/bnot/bsl/bsr are exact on bignum operands past 60 bits",
            &[Contract::LanguageSemantics, Contract::OtpApps],
            "[(16#FFFFFFFFFFFFFFFF band 16#123456789ABCDEF0), ((1 bsl 64) bor 1), ((1 bsl 64) bxor (1 bsl 64)), bnot(1 bsl 64), (1 bsl 64), ((1 bsl 64) bsr 64), (1 bsl 100)]",
            "ok [1311768467463790320,18446744073709551617,0,-18446744073709551617,18446744073709551616,1,1267650600228229401496703205376]",
        ),
        case(
            "bits/bit_ops_with_negative_operands",
            "the bit operators on a negative operand use the infinite two's-complement image",
            &[Contract::LanguageSemantics, Contract::OtpApps],
            "[(-1 band (1 bsl 64)), ((-1) bor (1 bsl 64)), ((-1) bxor (-1)), bnot(-1), bnot(0), ((-1) bsr 1)]",
            "ok [18446744073709551616,-1,0,0,-1,-1]",
        ),
        case(
            "bits/bit_size_and_byte_size",
            "bit_size/1 counts the bits of any bit string, byte_size/1 its whole bytes, and a non-bit-string is badarg",
            &[Contract::LanguageSemantics, Contract::OtpApps],
            "[bit_size(<<1,2,3>>), byte_size(<<1,2,3>>), bit_size(<<1:1>>), bit_size(<<>>), byte_size(<<>>), begin try bit_size(1234) catch error:SReason -> SReason end end, begin try byte_size(1234) catch error:TReason -> TReason end end]",
            "ok [24,3,1,0,0,badarg,badarg]",
        ),
    ]
}
