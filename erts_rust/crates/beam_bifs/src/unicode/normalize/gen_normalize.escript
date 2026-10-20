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
%% @doc Generate `table.rs' — the composition tables behind
%% `prim_file:internal_normalize_utf8/1'.
%%
%% The authoritative data is the *committed C header*
%% `erts/emulator/beam/erl_unicode_normalize.h' (itself generated for the C core
%% by the language bindings' `dec.erl', "do not edit manually"): a prefix tree of
%% composition chains — `CompEntry' arrays linked through `subs'/`hash' pointers —
%% plus the `comp_candidate_map' membership bitmap. This script re-reads that
%% header, **re-validates every hash table** against the C's own `hashsearch'
%% linear probing, flattens the tree into one row per chain, and writes the Rust
%% tables.
%%
%% `lib/stdlib/uc_spec/*.txt' is deliberately *not* the source: the C table
%% ignores canonical-composition exclusions, and its chain keys nest
%% right-to-left (mark first, base last), which no per-code-point property file
%% expresses.
%%
%% Usage (from the repository root; the sandbox `$HOME' is read-only, but that
%% does not matter for an escript):
%%
%%     ERL_TOP="$PWD" escript \
%%         erts_rust/crates/beam_bifs/src/unicode/normalize/gen_normalize.escript
%%
%% Options:
%%   `--from <file>'  the header to read (default: `$ERL_TOP/erts/emulator/beam/
%%                    erl_unicode_normalize.h', else found by walking up from the
%%                    current directory)
%%   `--out <file>'   the output path (default: `table.rs' next to this script)
%%
%% The script fails loudly (non-zero exit, no output file) if the header does not
%% parse, if a `subs'/`hash' pointer dangles, if a hash table is not exactly the
%% one the C's `hashsearch' would find, if a node with a result also has children
%% (the C would return the shorter chain and make the rest unreachable), or if a
%% chain nests deeper than the C's 4-code-point `savepoints' window.

-mode(compile).

main(Args) ->
    Src = opt(Args, "--from", default_header()),
    Out = opt(Args, "--out", default_out()),
    io:format("reading ~s~n", [Src]),
    {ok, Bin} = file:read_file(Src),
    Model = parse(binary_to_list(Bin)),
    validate(Model),
    Chains = flatten(Model),
    Data = emit(Model, Chains, Src),
    ok = file:write_file(Out, unicode:characters_to_binary(Data)),
    io:format("wrote ~s (~p chains, ~p proper prefixes, ~p bitmap words, "
              "max chain length ~p)~n",
              [Out, length(Chains), length(prefixes(Chains)),
               length(maps:get(bitmap, Model)), max_len(Chains)]).

%% ---------------------------------------------------------------------------
%% Options / paths
%% ---------------------------------------------------------------------------
opt(["--from", D | _], "--from", _) -> D;
opt(["--out", O | _], "--out", _) -> O;
opt([_ | T], Key, Default) -> opt(T, Key, Default);
opt([], _, Default) -> Default.

default_header() ->
    Rel = filename:join(["erts", "emulator", "beam", "erl_unicode_normalize.h"]),
    case os:getenv("ERL_TOP") of
        false ->
            search_up(filename:absname("."), Rel);
        Top ->
            F = filename:join([Top, "erts", "emulator", "beam", "erl_unicode_normalize.h"]),
            case filelib:is_regular(F) of
                true -> F;
                false -> search_up(filename:absname("."), Rel)
            end
    end.

search_up(Dir, Rel) ->
    F = filename:join(Dir, Rel),
    case filelib:is_regular(F) of
        true ->
            F;
        false ->
            Parent = filename:dirname(Dir),
            case Parent of
                Dir -> abort("cannot find ~s (set ERL_TOP or pass --from)", [Rel]);
                _ -> search_up(Parent, Rel)
            end
    end.

default_out() ->
    filename:join(filename:dirname(filename:absname(escript:script_name())), "table.rs").

abort(Fmt, Args) ->
    io:format(standard_error, "gen_normalize: " ++ Fmt ++ "~n", Args),
    halt(1).

%% ---------------------------------------------------------------------------
%% Parsing the C header
%% ---------------------------------------------------------------------------
%% The header is a flat sequence of two array kinds, each terminated by `;':
%%   static int <name>[<n>] = { <ints> };                     -- a hash table
%%   static CompEntry <name>[] = { {c, res, num_subs, subs, hash}, ... };
%% plus `compose_tab_size' / `HASH_SIZE_FACTOR' / `COMP_CANDIDATE_MAP_OFFSET'
%% defines and the `comp_candidate_map' bitmap.
parse(Text) ->
    #{hash_factor => int_define(Text, "HASH_SIZE_FACTOR"),
      offset => int_define(Text, "COMP_CANDIDATE_MAP_OFFSET"),
      hashes => int_arrays(Text),
      nodes => entry_arrays(Text),
      root_size => static_int(Text, "compose_tab_size"),
      bitmap => bitmap(Text)}.

int_define(Text, Name) ->
    case re:run(Text, "#define " ++ Name ++ " +([0-9]+)",
                [{capture, all_but_first, list}]) of
        {match, [N]} -> list_to_integer(N);
        nomatch -> abort("the header has no `#define ~s'", [Name])
    end.

static_int(Text, Name) ->
    case re:run(Text, "static int " ++ Name ++ " *= *([0-9]+)",
                [{capture, all_but_first, list}]) of
        {match, [N]} -> list_to_integer(N);
        nomatch -> abort("the header has no `static int ~s'", [Name])
    end.

int_arrays(Text) ->
    Re = "static int ([A-Za-z0-9_]+)\\[([0-9]+)\\] *=\\s*([^;]*);",
    {match, Matches} = re:run(Text, Re, [global, {capture, all_but_first, list}]),
    maps:from_list([{list_to_atom(Name), int_array_entry(Name, Len, Body)}
                    || [Name, Len, Body] <- Matches]).

int_array_entry(Name, Len, Body) ->
    {match, Nums} = re:run(Body, "-?[0-9]+", [global, {capture, first, list}]),
    Vals = [list_to_integer(V) || [V] <- Nums],
    length(Vals) =:= list_to_integer(Len)
        orelse abort("~s declares ~s entries but holds ~p", [Name, Len, length(Vals)]),
    Vals.

entry_arrays(Text) ->
    Re = "static CompEntry ([A-Za-z0-9_]+)\\[\\] *=\\s*\\{([^;]*)\\};",
    {match, Matches} = re:run(Text, Re, [global, {capture, all_but_first, list}]),
    maps:from_list([{list_to_atom(Name), node_entries(Name, Body)} || [Name, Body] <- Matches]).

node_entries(Name, Body) ->
    Re = "\\{([0-9]+), ([0-9]+), ([0-9]+), ([A-Za-z0-9_]+), ([A-Za-z0-9_]+)\\}",
    case re:run(Body, Re, [global, {capture, all_but_first, list}]) of
        {match, Rows} ->
            [#{c => list_to_integer(C), res => list_to_integer(Res),
               subs => ptr(Subs), hash => ptr(Hash)}
             || [C, Res, _N, Subs, Hash] <- Rows];
        nomatch ->
            abort("~s has no entries", [Name])
    end.

ptr("NULL") -> none;
ptr(Name) -> list_to_atom(Name).

bitmap(Text) ->
    Re = "static Uint32 comp_candidate_map\\[\\] *=\\s*\\{([^;]*)\\};",
    case re:run(Text, Re, [{capture, all_but_first, list}]) of
        {match, [Body]} ->
            {match, Nums} = re:run(Body, "0x([0-9A-Fa-f]+)", [global, {capture, all_but_first, list}]),
            [list_to_integer(V, 16) || [V] <- Nums];
        nomatch ->
            abort("the header has no comp_candidate_map", [])
    end.

%% ---------------------------------------------------------------------------
%% Validation — every structural invariant the C relies on
%% ---------------------------------------------------------------------------
validate(#{hashes := Hashes, nodes := Nodes, root_size := RootSize,
           hash_factor := Factor, offset := Offset, bitmap := Bitmap}) ->
    Root = maps:get(compose_tab, Nodes, missing),
    Root =/= missing orelse abort("no `compose_tab' root table", []),
    maps:is_key(hash_compose_tab, Hashes) orelse abort("no root hash table", []),
    length(Root) =:= RootSize
        orelse abort("compose_tab_size ~p does not match the root's ~p entries",
                     [RootSize, length(Root)]),
    %% `is_candidate/1' indexes `comp_candidate_map[cp div 32 - OFFSET]' only for
    %% 768 =< cp =< 4023 (`cp < 768' and `cp > 4023' short-circuit), so the bitmap
    %% must cover exactly that range.
    MaxKey = lists:max([C || {_Name, Rows} <- maps:to_list(Nodes), #{c := C} <- Rows]),
    Needed = min(MaxKey, 4023) div 32 - Offset + 1,
    length(Bitmap) =:= Needed
        orelse abort("the bitmap holds ~p words; U+~.16B needs exactly ~p",
                     [length(Bitmap), MaxKey, Needed]),
    [begin
         maps:is_key(Subs, Nodes) orelse abort("~p: dangling subs pointer ~p", [Name, Subs]),
         maps:is_key(Hash, Hashes) orelse abort("~p: dangling hash pointer ~p", [Name, Hash])
     end
     || {Name, Rows} <- maps:to_list(Nodes), #{subs := Subs, hash := Hash} <- Rows,
        Subs =/= none],
    %% The C reaches a node table only through `hashsearch/4', so reproduce that
    %% function (bucket = c rem size, linear probing) and require it to find
    %% exactly the entry at exactly its own index.
    [begin
         HSize = length(Hash),
         HSize =:= length(Rows) * Factor
             orelse abort("~p: hash size ~p is not ~p x ~p entries",
                          [HashName, HSize, Factor, length(Rows)]),
         lists:sort([V || V <- Hash, V =/= -1]) =:= lists:seq(0, length(Rows) - 1)
             orelse abort("~p: the hash table does not index every entry exactly once",
                          [HashName]),
         lists:foreach(
           fun({I, #{c := C}}) ->
                   case hashsearch(Hash, HSize, Rows, C, 0) of
                       I -> ok;
                       Got -> abort("~p: hashsearch(U+~.16B) yields ~p, expected ~p",
                                    [HashName, C, Got, I])
                   end
           end, lists:zip(lists:seq(0, length(Rows) - 1), Rows))
     end
     || {Name, Rows} <- maps:to_list(Nodes),
        HashName <- [list_to_atom("hash_" ++ atom_to_list(Name))],
        Hash <- [maps:get(HashName, Hashes)]],
    %% `translate/3' returns a node's result *before* descending into its
    %% children (`erl_unicode.c:1446-1452`), so a node that has both would make
    %% every chain below it unreachable; the flattener has no way to express that
    %% and refuses instead of emitting a table the algorithm can never use.
    [abort("U+~.16B has both a result and children; the C would never reach the "
           "children", [C])
     || {_Name, Rows} <- maps:to_list(Nodes), #{c := C, res := Res, subs := Subs} <- Rows,
        Res =/= 0, Subs =/= none],
    ok.

%% The C's `hashsearch' (`erts/emulator/beam/erl_unicode.c:1431`): bucket = c rem
%% size, and a linear probe while the slot is occupied by a different character.
hashsearch(Hash, HSize, Rows, C, Tries) when Tries < HSize ->
    Bucket = (C rem HSize + Tries) rem HSize,
    case lists:nth(Bucket + 1, Hash) of
        -1 ->
            -1;
        Idx ->
            case maps:get(c, lists:nth(Idx + 1, Rows)) of
                C -> Idx;
                _ -> hashsearch(Hash, HSize, Rows, C, Tries + 1)
            end
    end;
hashsearch(_Hash, _HSize, _Rows, _C, _Tries) ->
    -1.

%% ---------------------------------------------------------------------------
%% Flattening the prefix tree into chains
%% ---------------------------------------------------------------------------
%% A chain is the path from the root to a node with `res' set, in the order the
%% C's `translate/3' walks it: `s[0]' first, which is the *last* character of the
%% string (the combining mark), and the base character last.
flatten(#{nodes := Nodes} = Model) ->
    Chains = walk(Nodes, maps:get(compose_tab, Nodes), []),
    Keys = [pack(Chars) || {Chars, _Res} <- Chains],
    length(lists:usort(Keys)) =:= length(Keys)
        orelse abort("two chains pack to the same key", []),
    max_len(Chains) =< 4
        orelse abort("a chain nests ~p deep; the C's savepoints[4] cannot hold it",
                     [max_len(Chains)]),
    %% A chain can only ever start at a code point `is_candidate/1' admits: the C
    %% opens a pending group there (`handle_first_norm`) and nowhere else, so a
    %% chain whose first element the bitmap denies would be dead data.
    [abort("chain ~w starts at U+~.16B, which is_candidate/1 denies", [Chain, C])
     || {[C | _] = Chain, _Res} <- Chains, not candidate(Model, C)],
    %% Sorted by the **packed key**, not by the code-point list: `pack/1' puts
    %% `s[0]' in the low bits, so sequence order and numeric order of the packed
    %% form disagree (`[16#300, 16#1FFE]' packs to `16#1FFE0300', above the
    %% `16#410301' that `[16#301, 16#41]' packs to). `translate/3' binary-searches
    %% this table by packed key, so the packed order is the one that must hold.
    lists:sort([{pack(Chars), Chars, Res} || {Chars, Res} <- Chains]).

%% The C's `is_candidate/1' (`erts/emulator/beam/erl_unicode.c:1418`).
candidate(_Model, Cp) when Cp < 768 ->
    false;
candidate(_Model, Cp) when Cp > 4023 ->
    Cp =:= 12441 orelse Cp =:= 12442;
candidate(#{offset := Offset, bitmap := Bitmap}, Cp) ->
    Index = Cp div 32 - Offset,
    Word = lists:nth(Index + 1, Bitmap),
    Word band (1 bsl (Cp rem 32)) =/= 0.

walk(Nodes, Rows, Prefix) ->
    lists:flatmap(
      fun(#{c := C, res := Res, subs := Subs}) ->
              Chain = Prefix ++ [C],
              case {Res, Subs} of
                  {0, none} ->
                      abort("dead node U+~.16B on a root path", [C]);
                  {0, Child} ->
                      walk(Nodes, maps:get(Child, Nodes), Chain);
                  {_, _} ->
                      [{Chain, Res}]
              end
      end, Rows).

%% Accepts both chain shapes: the raw `{Chars, Res}' pairs `walk/3' produces (the
%% validations in `flatten/1' run before the sort) and the `{Key, Chars, Res}'
%% triples `flatten/1' hands out.
max_len(Chains) ->
    lists:max([length(chars_of(Entry)) || Entry <- Chains]).

chars_of({_Key, Chars, _Res}) ->
    Chars;
chars_of({Chars, _Res}) ->
    Chars.

%% 16 bits per code point, `s[0]' in the low bits, zero in every unused slot.
pack(Chars) ->
    lists:foldl(fun(C, Acc) -> (Acc bsl 16) bor C end, 0, lists:reverse(Chars)).

%% Every proper prefix of every chain, packed the same way, deduplicated and
%% sorted — the "is this sequence still a live path?" predicate `translate/3'
%% answers with `TRANSLATE_MAYBE'.
prefixes(Chains) ->
    lists:usort(
      lists:flatmap(
        fun({_Key, Chars, _}) ->
                [pack(lists:sublist(Chars, N)) || N <- lists:seq(1, length(Chars) - 1)]
        end, Chains)).
%% ---------------------------------------------------------------------------
%% Emitting Rust
%% ---------------------------------------------------------------------------
emit(#{offset := Offset, bitmap := Bitmap}, Chains, Src) ->
    [licence(),
     "//! **GENERATED FILE — DO NOT EDIT BY HAND.**\n"
     "//!\n"
     "//! The composition tables the `prim_file:internal_normalize_utf8/1`\n"
     "//! algorithm (`./mod.rs`) walks, transcribed from the committed C header\n"
     "//! `", shorten(Src), "` — the file the\n"
     "//! C BIF `prim_file_internal_normalize_utf8_1`\n"
     "//! (`erts/emulator/beam/erl_unicode.c:2826`) reads through\n"
     "//! `do_utf8_to_list_normalize` (`:1569`).\n"
     "//!\n"
     "//! Regenerate with (from the repository root):\n"
     "//!\n"
     "//! ```text\n"
     "//! ERL_TOP=\"$PWD\" escript \\\n"
     "//!     erts_rust/crates/beam_bifs/src/unicode/normalize/gen_normalize.escript\n"
     "//! ```\n"
     "//!\n"
     "//! This file is long because it is *data*: one row per composition chain of\n"
     "//! the C prefix tree, plus the candidate bitmap. It is exempt from the crate's\n"
     "//! ~1000-line guideline for that reason; the algorithm itself lives in the\n"
     "//! hand-written `./mod.rs`.\n"
     "//!\n"
     "//! # The chain encoding\n"
     "//!\n"
     "//! The C table is a prefix tree keyed *right-to-left*: `translate/3` looks up\n"
     "//! the combining mark most recently seen (the rightmost character of the\n"
     "//! string) at the root, the character before it in that node's `subs`, and so\n"
     "//! on until a node whose `res` is set. A chain is therefore `[mark, ...,\n"
     "//! base]` in string *reverse* order, and [`CHAINS`] stores each one as `(key,\n"
     "//! result)` where `key` packs the code points 16 bits each, first element in\n"
     "//! the low bits and zero in every unused slot (every chain code point is\n"
     "//! non-zero, so the encoding is unambiguous and numeric order is lexicographic\n"
     "//! by prefix).\n"
     "//!\n"
     "//! [`CHAIN_PREFIXES`] holds the same encoding for every *proper* prefix of a\n"
     "//! chain: a sequence whose key is there, whose full key is not a chain, is one\n"
     "//! the C's `translate/3` answers `TRANSLATE_MAYBE` for — a live path that needs\n"
     "//! one more character, which is what keeps a pending group open instead of\n"
     "//! rolling it up.\n"
     "\n",
     bitmap(Bitmap, Offset),
     chains(Chains),
     prefix_table(Chains)].

licence() ->
    "// %CopyrightBegin%\n"
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
    "// %CopyrightEnd%\n".

shorten(Src) ->
    case string:split(Src, "erts/emulator/beam/", trailing) of
        [_, Tail] -> "erts/emulator/beam/" ++ Tail;
        _ -> Src
    end.

bitmap(Bitmap, Offset) ->
    ["/// `COMP_CANDIDATE_MAP_OFFSET` — `cp div 32` is biased by this before the\n"
     "/// bitmap lookup (`is_candidate/1`, `erts/emulator/beam/erl_unicode.c:1418`).\n"
     "pub(crate) const CANDIDATE_MAP_OFFSET: u32 = ", integer_to_list(Offset), ";\n\n"
     "/// `comp_candidate_map` — one bit per code point, low bit first within a word.\n"
     "pub(crate) const CANDIDATE_MAP: [u32; ", integer_to_list(length(Bitmap)), "] = [\n",
     [io_lib:format("    ~s,~n", [hex(W, 8)]) || W <- Bitmap],
     "];\n\n"].

chains(Chains) ->
    ["/// The composition chains, `(packed key, composed code point)`, sorted by key.\n"
     "pub(crate) const CHAINS: [(u64, u32); ", integer_to_list(length(Chains)), "] = [\n",
     [io_lib:format("    (~s, ~w),~n", [hex(Key, 16), Res]) || {Key, _Chars, Res} <- Chains],
     "];\n\n"].
prefix_table(Chains) ->
    Pre = prefixes(Chains),
    ["/// Every proper prefix of a chain, packed the same way, sorted.\n"
     "pub(crate) const CHAIN_PREFIXES: [u64; ", integer_to_list(length(Pre)), "] = [\n",
     [io_lib:format("    ~s,~n", [hex(K, 16)]) || K <- Pre],
     "];\n"].

hex(Value, Digits) ->
    Text = integer_to_list(Value, 16),
    Pad = lists:duplicate(max(0, Digits - length(Text)), $0),
    "0x" ++ Pad ++ Text.
