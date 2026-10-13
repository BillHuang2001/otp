#!/usr/bin/env escript
%%! -noshell
%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2025-2026. All Rights Reserved.
%%
%% Licensed under the Apache License, Version 2.0 (the "License");
%% you may not use this file except in compliance with the License.
%% You may obtain a copy of the License at
%%
%%     http://www.apache.org/licenses/LICENSE-2.0
%%
%% Unless required by applicable law or agreed to in writing, software
%% distributed under the License is distributed on an "AS IS" BASIS,
%% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
%% See the License for the specific language governing permissions and
%% limitations under the License.
%%
%% %CopyrightEnd%
%%
%% @doc Generate `props.rs' — the Unicode property tables the grapheme-cluster
%% walk needs.
%%
%% This is the Rust-side counterpart of `lib/stdlib/uc_spec/gen_unicode_mod.escript'
%% (the generator that emits `unicode_util.erl'): it parses the *same* three
%% in-repo property files, with the *same* grammar and the *same* range merging,
%% so the Rust tables describe exactly the code-point sets the oracle's generated
%% `gc/1' family tests.
%%
%% Usage (from the repository root; the sandbox `$HOME' is read-only, but that
%% does not matter for an escript):
%%
%%     ERL_TOP="$PWD" escript \
%%         erts_rust/crates/beam_bifs/src/unicode/grapheme/gen_props.escript
%%
%% Options:
%%   `--from <dir>'  the directory holding the three `.txt' files
%%                   (default: `$ERL_TOP/lib/stdlib/uc_spec', else found by
%%                   walking up from the current directory)
%%   `--out <file>'  the output path (default: `props.rs' next to this script)
%%
%% Only the *sets* are generated: `gen_gc/2''s `split_ranges'/`split_small_ranges'/
%% `optimize_ranges' pass reshapes which clauses are emitted (and in which order)
%% but is asserted set-equal by the original generator, and every generated
%% predicate is a pure per-code-point membership test. The clause *order* of
%% `gc_1/2' is a property of the walk, not of the tables, and is reproduced by
%% hand in `../mod.rs'.

main(Args) ->
    Dir = opt(Args, "--from", default_data_dir()),
    Out = opt(Args, "--out", default_out()),
    Props = load_all(Dir),
    Data = props(Dir, Props),
    ok = file:write_file(Out, Data),
    io:format("wrote ~s~n", [Out]).

%% ---------------------------------------------------------------------------
%% Options / paths
%% ---------------------------------------------------------------------------
opt(["--from", D | _], "--from", _) -> D;
opt(["--out", O | _], "--out", _) -> O;
opt([_ | T], Key, Default) -> opt(T, Key, Default);
opt([], _, Default) -> Default.

default_data_dir() ->
    case os:getenv("ERL_TOP") of
        false ->
            search_up(filename:absname("."));
        Top ->
            D = filename:join([Top, "lib", "stdlib", "uc_spec"]),
            case filelib:is_regular(filename:join(D, "GraphemeBreakProperty.txt")) of
                true -> D;
                false -> search_up(filename:absname("."))
            end
    end.

search_up(Dir) ->
    P = filename:join([Dir, "lib", "stdlib", "uc_spec", "GraphemeBreakProperty.txt"]),
    case filelib:is_regular(P) of
        true ->
            filename:join([Dir, "lib", "stdlib", "uc_spec"]);
        false ->
            Parent = filename:dirname(Dir),
            case Parent of
                Dir -> abort_no_data();
                _ -> search_up(Parent)
            end
    end.

abort_no_data() ->
    io:format(standard_error,
              "gen_props: cannot find lib/stdlib/uc_spec; pass --from <dir>~n", []),
    halt(2).

default_out() ->
    filename:join(filename:dirname(escript:script_name()), "props.rs").

%% ---------------------------------------------------------------------------
%% Parsing — a transcription of `parse_properties/2' in gen_unicode_mod.escript
%% ---------------------------------------------------------------------------
load_all(Dir) ->
    Files = ["GraphemeBreakProperty.txt", "emoji-data.txt", "IndicSyllabicCategory.txt"],
    lists:foldl(
      fun(F, Acc) ->
              Path = filename:join(Dir, F),
              case file:read_file(Path) of
                  {ok, _} -> {ok, Bin} = file:read_file(Path),
                             each_line(Bin, fun parse_props/2, Acc);
                  {error, _} -> io:format(standard_error, "gen_props: missing ~s~n", [Path]),
                                halt(2)
              end
      end, #{}, Files).

each_line(Bin, Fun, Acc0) ->
    Lines = binary:split(Bin, <<"\n">>, [global]),
    lists:foldl(
      fun(Line0, Acc) ->
              Line = string:trim(binary_to_list(Line0), trailing, "\r"),
              case Line of
                  "#" ++ _ -> Acc;
                  _ ->
                      case string:trim(Line) of
                          "" -> Acc;
                          _ -> Fun(Line, Acc)
                      end
              end
      end, Acc0, Lines).

parse_props(Line0, Acc) ->
    [Line | _Comments] = tokens(Line0, "#"),
    case tokens(Line, ";") of
        [CodePoints, Class | _] ->
            {Lo, Hi} =
                case tokens(CodePoints, ".") of
                    [CodePoint] ->
                        {hex_to_int(CodePoint), hex_to_int(CodePoint)};
                    [CodePoint1, "", CodePoint2] ->
                        {hex_to_int(CodePoint1), hex_to_int(CodePoint2)}
                end,
            Key = to_atom(Class),
            maps:update_with(Key, fun(L) -> [{Lo, Hi} | L] end, [{Lo, Hi}], Acc);
        _ ->
            Acc
    end.

hex_to_int(HexStr) ->
    list_to_integer(string:trim(HexStr, both), 16).

to_atom(Str) ->
    list_to_atom(string:lowercase(string:trim(Str, both))).

%% Differs from string:lexemes, it returns empty string as token between two delimiters
tokens(S, [C]) ->
    tokens(lists:reverse(S), C, []).

tokens([Sep | S], Sep, Toks) ->
    tokens(S, Sep, [[] | Toks]);
tokens([C | S], Sep, Toks) ->
    tokens_2(S, Sep, Toks, [C]);
tokens([], _, Toks) ->
    Toks.

tokens_2([Sep | S], Sep, Toks, Tok) ->
    tokens(S, Sep, [Tok | Toks]);
tokens_2([C | S], Sep, Toks, Tok) ->
    tokens_2(S, Sep, Toks, [C | Tok]);
tokens_2([], _Sep, Toks, Tok) ->
    [Tok | Toks].

%% ---------------------------------------------------------------------------
%% Range algebra — the transcription of `merge_ranges_1/2' (sorted, disjoint,
%% adjacency-merged ranges).
%% ---------------------------------------------------------------------------
ranges(Props, Key) ->
    merge_ranges_1(lists:usort(maps:get(Key, Props, [])), []).

merge_ranges_1([{Next, Stop} | R], [{Start, Prev} | Acc]) when Next =< Prev + 1 ->
    merge_ranges_1(R, [{Start, max(Prev, Stop)} | Acc]);
merge_ranges_1([Next | R], Acc) ->
    merge_ranges_1(R, [Next | Acc]);
merge_ranges_1([], Acc) ->
    lists:reverse(Acc).

%% ---------------------------------------------------------------------------
%% The emitted tables
%% ---------------------------------------------------------------------------
%% {Rust constant, property key}
plain_sets() ->
    [{"CR", cr},
     {"LF", lf},
     {"CONTROL", control},
     {"EXTEND", extend},
     {"SPACINGMARK", spacingmark},
     {"ZWJ", zwj},
     {"PREPEND", prepend},
     {"L", l},
     {"V", v},
     {"T", t},
     {"LV", lv},
     {"LVT", lvt},
     {"REGIONAL_INDICATOR", regional_indicator},
     {"EXTENDED_PICTOGRAPHIC", extended_pictographic},
     {"CONSONANT", consonant},
     {"VOWEL_INDEPENDENT", vowel_independent},
     {"VIRAMA", virama},
     {"INVISIBLE_STACKER", invisible_stacker}].

props(Dir, P) ->
    R = fun(Key) -> ranges(P, Key) end,
    Plain = [{Name, R(Key)} || {Name, Key} <- plain_sets()],
    Zwj = R(zwj),
    %% `is_control/1' = cr ∪ lf ∪ control
    IsControl = merge_ranges_1(lists:usort(R(cr) ++ R(lf) ++ R(control)), []),
    %% `is_extend/1` = extend ∪ spacingmark (the `zwj' class is a separate value)
    ExtTre = merge_ranges_1(lists:usort(R(extend) ++ R(spacingmark)), []),
    case [{Lo, Hi} || {Lo, Hi} <- ExtTre, Lo =< 16#200D, 16#200D =< Hi] of
        [] -> ok;
        _ -> io:format(standard_error, "gen_props: ZWJ inside extend ∪ spacingmark~n", []),
             halt(2)
    end,
    %% `gc_1/2''s "extended pictographic below 256" clause
    ExtLow = [{Lo, Hi} || {Lo, Hi} <- R(extended_pictographic), Lo < 256],
    IndicC = merge_ranges_1(lists:usort(R(consonant) ++ R(vowel_independent) ++
                                             [{16#1B0B, 16#1B0C}]), []),
    IndicL = merge_ranges_1(lists:usort(R(virama) ++ R(invisible_stacker)), []),
    Composites = [{"IS_CONTROL", IsControl},
                  {"IS_EXTEND_TRUE", ExtTre},
                  {"EXTENDED_PICTOGRAPHIC_LOW", ExtLow},
                  {"IS_INDIC_CONSONANT", IndicC},
                  {"IS_INDIC_LINKER", IndicL}],
    iolist_to_binary(
      [header(Dir),
       [table(Name, Rs) || {Name, Rs} <- Plain],
       "/// The single code point of the `zwj' class (U+200D).\n",
       "pub(crate) const ZWJ_CP: u32 = 0x200D;\n\n",
       "/// Asserted by the generator: the `zwj' class holds exactly U+200D.\n",
       io_lib:format("const ZWJ_CLASS: &[(u32, u32)] = &[~s];~n~n",
                     [io_lib:format("(0x~4.16.0B, 0x~4.16.0B)", [Lo2, Hi2]) || {Lo2, Hi2} <- Zwj]),
       [table(Name, Rs) || {Name, Rs} <- Composites],
       predicates()]).

header(Dir) ->
    ["// %CopyrightBegin%\n"
     "//\n"
     "// SPDX-License-Identifier: Apache-2.0\n"
     "//\n"
     "// Copyright Ericsson AB 2025-2026. All Rights Reserved.\n"
     "//\n"
     "// Licensed under the Apache License, Version 2.0 (the \"License\");\n"
     "// you may not use this file except in compliance with the License.\n"
     "// You may obtain a copy of the License at\n"
     "//\n"
     "//     http://www.apache.org/licenses/LICENSE-2.0\n"
     "//\n"
     "// Unless required by applicable law or agreed to in writing, software\n"
     "// distributed under the License is distributed on an \"AS IS\" BASIS,\n"
     "// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.\n"
     "// See the License for the specific language governing permissions and\n"
     "// limitations under the License.\n"
     "//\n"
     "// %CopyrightEnd%\n"
     "//! **GENERATED FILE — DO NOT EDIT BY HAND.**\n"
     "//!\n"
     "//! The Unicode property tables (sorted, disjoint, adjacency-merged\n"
     "//! `(lo, hi)` code-point ranges) that `../mod.rs`'s port of the generated\n"
     "//! `unicode_util:gc/1` family needs, plus the membership predicates over\n"
     "//! them.\n"
     "//!\n"
     "//! Generated by `gen_props.escript` (next to this file) from the in-repo\n"
     "//! Unicode Character Database files\n",
     io_lib:format("//! `~s/{GraphemeBreakProperty,emoji-data,IndicSyllabicCategory}.txt`,~n", [Dir]),
     "//! with the same grammar and the same range merging as\n"
     "//! `lib/stdlib/uc_spec/gen_unicode_mod.escript` (which generates the oracle's\n"
     "//! `unicode_util.erl`). Regenerate with:\n"
     "//!\n"
     "//! ```text\n"
     "//! ERL_TOP=\"$PWD\" escript \\\n"
     "//!     erts_rust/crates/beam_bifs/src/unicode/grapheme/gen_props.escript\n"
     "//! ```\n"
     "//!\n"
     "//! This file is long because it is *data*: one line per merged range (the\n"
     "//! same shape the original generator emits one clause per range for). It is\n"
     "//! exempt from the crate's ~1000-line guideline for that reason.\n"
     "//!\n"
     "//! The generated `gc_1/2` clause *order* is not encoded here (it is a\n"
     "//! property of the walk, not of the tables); `../mod.rs` reproduces it.\n"
     "//!\n"
     "//! Every table is sorted ascending and non-overlapping, so membership is a\n"
     "//! binary search (`in_ranges`).\n"
     "\n"
     "use std::cmp::Ordering;\n"
     "\n"
     "/// Membership of `cp` in a sorted, disjoint range table.\n"
     "#[inline]\n"
     "pub(crate) fn in_ranges(table: &[(u32, u32)], cp: u32) -> bool {\n"
     "    table\n"
     "        .binary_search_by(|&(lo, hi)| {\n"
     "            if cp < lo {\n"
     "                Ordering::Greater\n"
     "            } else if cp > hi {\n"
     "                Ordering::Less\n"
     "            } else {\n"
     "                Ordering::Equal\n"
     "            }\n"
     "        })\n"
     "        .is_ok()\n"
     "}\n"
     "\n"].

table(Name, Ranges) ->
    Rows = [io_lib:format("    (0x~4.16.0B, 0x~4.16.0B),~n", [Lo, Hi]) || {Lo, Hi} <- Ranges],
    io_lib:format("/// `~s` — ~w merged range(s).~npub(crate) const ~s: &[(u32, u32)] = &[~n~s];~n~n",
                  [Name, length(Ranges), Name, Rows]).

predicates() ->
    "/// `is_control/1` — `cr ∪ lf ∪ control`.\n"
    "#[inline]\n"
    "pub(crate) fn is_control(cp: u32) -> bool {\n"
    "    in_ranges(IS_CONTROL, cp)\n"
    "}\n"
    "\n"
    "/// `is_extend/1`'s three-valued result (`false | true | zwj`).\n"
    "#[derive(Clone, Copy, PartialEq, Eq, Debug)]\n"
    "pub(crate) enum Extend {\n"
    "    /// `false`\n"
    "    No,\n"
    "    /// `true`\n"
    "    Yes,\n"
    "    /// `zwj` — the single code point of the `zwj` class.\n"
    "    Zwj,\n"
    "}\n"
    "\n"
    "/// `is_extend(CP)` — `zwj` for U+200D, `true` for `extend ∪ spacingmark`.\n"
    "#[inline]\n"
    "pub(crate) fn is_extend(cp: u32) -> Extend {\n"
    "    if cp == ZWJ_CP {\n"
    "        Extend::Zwj\n"
    "    } else if in_ranges(IS_EXTEND_TRUE, cp) {\n"
    "        Extend::Yes\n"
    "    } else {\n"
    "        Extend::No\n"
    "    }\n"
    "}\n"
    "\n"
    "/// `is_ext_pict/1` — the whole `Extended_Pictographic` set (not just the\n"
    "/// below-256 prefix `gc_1/2` special-cases).\n"
    "#[inline]\n"
    "pub(crate) fn is_ext_pict(cp: u32) -> bool {\n"
    "    in_ranges(EXTENDED_PICTOGRAPHIC, cp)\n"
    "}\n"
    "\n"
    "/// The `Extended_Pictographic` prefix `gc_1/2` handles before the Latin-1\n"
    "/// fast path.\n"
    "#[inline]\n"
    "pub(crate) fn in_ext_pict_low(cp: u32) -> bool {\n"
    "    in_ranges(EXTENDED_PICTOGRAPHIC_LOW, cp)\n"
    "}\n"
    "\n"
    "/// `is_indic_consonant/1` — `consonant ∪ vowel_independent ∪ {16#1B0B,16#1B0C}`.\n"
    "#[inline]\n"
    "pub(crate) fn is_indic_consonant(cp: u32) -> bool {\n"
    "    in_ranges(IS_INDIC_CONSONANT, cp)\n"
    "}\n"
    "\n"
    "/// `is_indic_linker/1` — `virama ∪ invisible_stacker`.\n"
    "#[inline]\n"
    "pub(crate) fn is_indic_linker(cp: u32) -> bool {\n"
    "    in_ranges(IS_INDIC_LINKER, cp)\n"
    "}\n"
    "\n"
    "/// `gc_1/2`'s `prepend` clauses.\n"
    "#[inline]\n"
    "pub(crate) fn in_prepend(cp: u32) -> bool {\n"
    "    in_ranges(PREPEND, cp)\n"
    "}\n"
    "\n"
    "/// `gc_1/2`'s Hangul `L` clauses (`gc_h_L/2`).\n"
    "#[inline]\n"
    "pub(crate) fn in_l(cp: u32) -> bool {\n"
    "    in_ranges(L, cp)\n"
    "}\n"
    "\n"
    "/// `gc_1/2`'s Hangul `V` clauses (`gc_h_V/2`).\n"
    "#[inline]\n"
    "pub(crate) fn in_v(cp: u32) -> bool {\n"
    "    in_ranges(V, cp)\n"
    "}\n"
    "\n"
    "/// `gc_1/2`'s Hangul `T` clauses (`gc_h_T/2`).\n"
    "#[inline]\n"
    "pub(crate) fn in_t(cp: u32) -> bool {\n"
    "    in_ranges(T, cp)\n"
    "}\n"
    "\n"
    "/// `gc_h_lv_lvt/3`'s Hangul `LV` clauses.\n"
    "#[inline]\n"
    "pub(crate) fn in_lv(cp: u32) -> bool {\n"
    "    in_ranges(LV, cp)\n"
    "}\n"
    "\n"
    "/// `gc_h_lv_lvt/3`'s Hangul `LVT` clauses.\n"
    "#[inline]\n"
    "pub(crate) fn in_lvt(cp: u32) -> bool {\n"
    "    in_ranges(LVT, cp)\n"
    "}\n"
    "\n"
    "/// `gc_1/2`'s `regional_indicator` clauses (`gc_regional/2`).\n"
    "#[inline]\n"
    "pub(crate) fn in_regional(cp: u32) -> bool {\n"
    "    in_ranges(REGIONAL_INDICATOR, cp)\n"
    "}\n"
    "\n"
    "#[cfg(test)]\n"
    "mod tests {\n"
    "    use super::*;\n"
    "\n"
    "    /// Every table is sorted, disjoint and non-empty in the intended way, and\n"
    "    /// the composites really are the unions the walk assumes.\n"
    "    #[test]\n"
    "    fn tables_are_sorted_and_disjoint() {\n"
    "        let tables: &[(&str, &[(u32, u32)])] = &[\n"
    "            (\"cr\", CR), (\"lf\", LF), (\"control\", CONTROL), (\"extend\", EXTEND),\n"
    "            (\"spacingmark\", SPACINGMARK), (\"zwj\", ZWJ), (\"prepend\", PREPEND),\n"
    "            (\"l\", L), (\"v\", V), (\"t\", T), (\"lv\", LV), (\"lvt\", LVT),\n"
    "            (\"regional_indicator\", REGIONAL_INDICATOR),\n"
    "            (\"extended_pictographic\", EXTENDED_PICTOGRAPHIC),\n"
    "            (\"consonant\", CONSONANT), (\"vowel_independent\", VOWEL_INDEPENDENT),\n"
    "            (\"virama\", VIRAMA), (\"invisible_stacker\", INVISIBLE_STACKER),\n"
    "            (\"is_control\", IS_CONTROL), (\"is_extend_true\", IS_EXTEND_TRUE),\n"
    "            (\"extended_pictographic_low\", EXTENDED_PICTOGRAPHIC_LOW),\n"
    "            (\"is_indic_consonant\", IS_INDIC_CONSONANT),\n"
    "            (\"is_indic_linker\", IS_INDIC_LINKER),\n"
    "        ];\n"
    "        for (name, t) in tables {\n"
    "            assert!(!t.is_empty(), \"{name} is empty\");\n"
    "            for w in t.windows(2) {\n"
    "                assert!(w[0].0 <= w[0].1, \"{name}: inverted range\");\n"
    "                assert!(w[0].1 < w[1].0, \"{name}: overlapping/unsorted ranges\");\n"
    "            }\n"
    "            assert!(t[t.len() - 1].0 <= t[t.len() - 1].1, \"{name}: inverted last\");\n"
    "        }\n"
    "    }\n"
    "\n"
    "    #[test]\n"
    "    fn zwj_class_holds_exactly_u200d() {\n"
    "        assert_eq!(ZWJ_CLASS, &[(0x200D, 0x200D)]);\n"
    "        assert_eq!(is_extend(0x200D), Extend::Zwj);\n"
    "        assert!(!in_ranges(IS_EXTEND_TRUE, 0x200D));\n"
    "    }\n"
    "\n"
    "    #[test]\n"
    "    fn composites_are_unions() {\n"
    "        for cp in 0u32..=0x10_FFFF {\n"
    "            assert_eq!(is_control(cp), in_ranges(CR, cp) || in_ranges(LF, cp)\n"
    "                || in_ranges(CONTROL, cp), \"control {cp:#x}\");\n"
    "            assert_eq!(in_ranges(IS_EXTEND_TRUE, cp),\n"
    "                in_ranges(EXTEND, cp) || in_ranges(SPACINGMARK, cp), \"extend {cp:#x}\");\n"
    "            assert_eq!(is_indic_consonant(cp), in_ranges(CONSONANT, cp)\n"
    "                || in_ranges(VOWEL_INDEPENDENT, cp)\n"
    "                || (0x1B0B..=0x1B0C).contains(&cp), \"indic consonant {cp:#x}\");\n"
    "            assert_eq!(is_indic_linker(cp),\n"
    "                in_ranges(VIRAMA, cp) || in_ranges(INVISIBLE_STACKER, cp),\n"
    "                \"indic linker {cp:#x}\");\n"
    "        }\n"
    "    }\n"
    "\n"
    "    /// The headline membership counts, pinned so a data-file upgrade is\n"
    "    /// visible rather than silent.\n"
    "    #[test]\n"
    "    fn range_counts_are_pinned() {\n"
    "        assert_eq!(IS_CONTROL.len(), 18);\n"
    "        assert_eq!(IS_EXTEND_TRUE.len(), 390);\n"
    "        assert_eq!(EXTENDED_PICTOGRAPHIC.len(), 188);\n"
    "        assert_eq!(EXTENDED_PICTOGRAPHIC_LOW, &[(169, 169), (174, 174)]);\n"
    "        assert_eq!(IS_INDIC_CONSONANT.len(), 230);\n"
    "        assert_eq!(IS_INDIC_LINKER.len(), 41);\n"
    "        assert_eq!(PREPEND.len(), 15);\n"
    "    }\n"
    "}\n".
