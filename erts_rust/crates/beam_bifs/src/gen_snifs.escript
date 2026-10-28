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
%% @doc Generate `snifs.rs' — the `bif.tab' roster behind
%% `erlang:system_info(snifs)'.
%%
%% The C BIF (`erts/emulator/beam/erl_bif_info.c:2764-2772', `build_snifs_term')
%% walks the generated `bif_table' from index 0 upward and **conses** each
%% `{Module, Name, Arity}' onto the accumulating list, so the answer is the table
%% in REVERSE order. The table itself is emitted by `utils/make_tables' from
%% `erts/emulator/beam/bif.tab', in `bif.tab' file order, so `bif.tab' is the one
%% faithful source: no other file in the tree carries the roster in that order.
%%
%% This script therefore parses `erts/emulator/beam/bif.tab' directly — the same
%% grammar `src/bif_tab.rs' parses (whole-line `#' comments, the `bif'/`ubif'/
%% `hbif' keywords, an optional trailing `C-name' column, quoted operator names
%% and `lo-hi' arity ranges) — and renders one `Snif' row per declaration in
%% **file order**. `snifs()' in the emitted file is that roster reversed, i.e.
%% the oracle's list order.
%%
%% The `is_snif_term' filter (`erl_bif_info.c:2743') drops a row whose *module*
%% atom name contains a `.'. No `bif.tab' row in this tree has one, so this
%% script **asserts** the absence instead of emitting a filter that could never
%% fire (a dotted row is a loud abort naming the row, never a silently dropped
%% one).
%%
%% Usage (from the OTP checkout root — `$ERL_TOP', else found by walking up, is
%% used to locate `erts/emulator/beam/bif.tab'):
%%
%%     escript erts_rust/crates/beam_bifs/src/gen_snifs.escript
%%
%% Options:
%%   `--top <dir>'   the OTP root (default: `$ERL_TOP', else found by walking
%%                   up from the current directory)
%%   `--out <file>'  the output path (default: `snifs.rs' next to this script)
%%
%% **The emitted file is generated data — never hand-edit it.** It is pinned by
%% `tests/snifs.rs', which re-parses `bif.tab' with `src/bif_tab.rs' and compares
%% the roster row for row, and (when an `erl' is on `PATH') compares it against a
%% live oracle's `system_info(snifs)'.

main(Args) ->
    Top = opt(Args, "--top", default_top()),
    Out = opt(Args, "--out", default_out()),
    Tab = filename:join([Top, "erts", "emulator", "beam", "bif.tab"]),
    ok = assert_file(Tab),
    Rows = parse_bif_tab(Tab),
    ok = check_no_dotted_module(Rows, Tab),
    %% `unicode:characters_to_binary' so a non-ASCII codepoint in a name is emitted
    %% as UTF-8 rather than tripping the byte-list limit.
    ok = file:write_file(Out, unicode:characters_to_binary(render(Rows))),
    io:format("wrote ~s (~p snifs)~n", [Out, length(Rows)]).

%% ---------------------------------------------------------------------------
%% Options / paths
%% ---------------------------------------------------------------------------
opt(["--top", T | _], "--top", _) -> T;
opt(["--out", O | _], "--out", _) -> O;
opt([_ | T], Key, Default) -> opt(T, Key, Default);
opt([], _, Default) -> Default.

default_out() ->
    filename:join(filename:dirname(filename:absname(escript:script_name())),
                  "snifs.rs").

default_top() ->
    case os:getenv("ERL_TOP") of
        false -> search_up(filename:absname("."));
        Top ->
            case is_top(Top) of
                true -> Top;
                false -> search_up(filename:absname("."))
            end
    end.

search_up(Dir) ->
    case is_top(Dir) of
        true -> Dir;
        false ->
            Parent = filename:dirname(Dir),
            case Parent of
                Dir ->
                    abort("cannot find the OTP root (erts/emulator/beam/bif.tab); "
                          "pass --top or set ERL_TOP");
                _ ->
                    search_up(Parent)
            end
    end.

is_top(Dir) ->
    filelib:is_regular(filename:join([Dir, "erts", "emulator", "beam", "bif.tab"])).

assert_file(Path) ->
    case filelib:is_regular(Path) of
        true -> ok;
        false -> abort("missing " ++ Path)
    end.

%% ---------------------------------------------------------------------------
%% Parse `bif.tab'
%% ---------------------------------------------------------------------------
%% Same grammar as `src/bif_tab.rs': `#` starts a whole-line comment, blank lines
%% are ignored, the line is `<keyword> <module>:<name>/<arity> <C-name>*`, and only
%% the first two columns carry the identity.
parse_bif_tab(File) ->
    {ok, Bin} = file:read_file(File),
    Lines = string:split(binary_to_list(Bin), "\n", all),
    lists:append([parse_line(Line, File, N) || {N, Line} <- lists:zip(lists:seq(1, length(Lines)), Lines)]).

parse_line(Line, File, N) ->
    case string:trim(Line) of
        "" -> [];
        "#" ++ _ -> [];
        Trimmed ->
            case string:lexemes(Trimmed, " \t") of
                [Kind, Spec | _] when Kind =:= "bif"; Kind =:= "ubif"; Kind =:= "hbif" ->
                    parse_spec(Spec, File, N);
                [Kind | _] ->
                    abort(io_lib:format("~s:~p: unknown directive `~s' "
                                        "(expected `bif', `ubif' or `hbif')",
                                        [File, N, Kind]));
                [] ->
                    abort(io_lib:format("~s:~p: empty declaration", [File, N]))
            end
    end.

%% `string:split(_, ":", leading)' so a quoted operator name may itself contain a
%% `:' (`erlang:'=:='/2'); `string:split(_, "/", trailing)' likewise for `/' (`erlang:'/'/2').
parse_spec(Spec, File, N) ->
    case string:split(Spec, ":", leading) of
        [Module, Rest] when Module =/= "" ->
            case string:split(Rest, "/", trailing) of
                [Name, Arity] when Name =/= "" ->
                    [{Module, unquote(Name), A} || A <- arities(Arity, Spec, File, N)];
                _ ->
                    abort(io_lib:format("~s:~p: `~s' is missing the `/arity' suffix",
                                        [File, N, Spec]))
            end;
        _ ->
            abort(io_lib:format("~s:~p: `~s' is not `module:name/arity' (no `:')",
                                [File, N, Spec]))
    end.

%% An operator or keyword is written quoted (`'and'`, `'+'`, `'=:='`, `'!'`,
%% `'/'`); the quotes are Erlang term syntax, not part of the atom, so they are
%% stripped to recover the name — exactly what `src/bif_tab.rs' does.
unquote([$' | Rest]) ->
    case lists:reverse(Rest) of
        [$' | Inner] when Inner =/= [] -> lists:reverse(Inner);
        _ -> [$' | Rest]
    end;
unquote(Name) ->
    Name.

%% A single arity, or the inclusive range `lo-hi' (expanded), like `src/bif_tab.rs'.
arities(Part, Spec, File, N) ->
    case string:split(Part, "-") of
        [Single] ->
            [to_arity(Single, Spec, File, N)];
        [Lo, Hi] ->
            L = to_arity(Lo, Spec, File, N),
            H = to_arity(Hi, Spec, File, N),
            case L =< H of
                true -> lists:seq(L, H);
                false -> abort(io_lib:format("~s:~p: `~s' has a descending arity range",
                                             [File, N, Spec]))
            end
    end.

to_arity(Part, Spec, File, N) ->
    case string:to_integer(Part) of
        {A, ""} when A >= 0 -> A;
        _ -> abort(io_lib:format("~s:~p: `~s' has a non-numeric arity `~s'",
                                 [File, N, Spec, Part]))
    end.

%% `is_snif_term' (`erl_bif_info.c:2743') drops a row whose module atom name
%% contains a `.'. The roster is emitted unfiltered, so that must stay vacuous.
check_no_dotted_module(Rows, File) ->
    case [R || {Module, _, _} = R <- Rows, lists:member($., Module)] of
        [] -> ok;
        Dotted ->
            abort(io_lib:format("~s: ~p row(s) name a dotted (package-qualified) module, "
                                "which `is_snif_term' filters out; this roster does not "
                                "model that filter: ~p", [File, length(Dotted), Dotted]))
    end.

%% ---------------------------------------------------------------------------
%% Render
%% ---------------------------------------------------------------------------
render(Rows) ->
    Header =
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
        "// %CopyrightEnd%\n"
        "\n",
    Doc =
        "//! The static-NIF (`snifs`) roster behind `erlang:system_info(snifs)`.\n"
        "//!\n"
        "//! **This file is GENERATED DATA — never hand-edit it.** It is emitted by the\n"
        "//! committed generator `gen_snifs.escript` in this directory, which parses this\n"
        "//! repository's real `erts/emulator/beam/bif.tab` (the table `utils/make_tables`\n"
        "//! turns into the C `bif_table` in that same file order). To regenerate (from the\n"
        "//! OTP checkout root):\n"
        "//!\n"
        "//! ```text\n"
        "//! escript erts_rust/crates/beam_bifs/src/gen_snifs.escript\n"
        "//! ```\n"
        "//!\n"
        "//! ## Why this file is long\n"
        "//!\n"
        "//! It *is* the table: one [`Snif`] row per declared `bif.tab` entry, in `bif.tab`\n"
        "//! file order. There is nothing to factor out — the roster is the whole content of\n"
        "//! the answer `erlang:system_info(snifs)` gives, and splitting it would only\n"
        "//! obscure that it is machine output.\n"
        "//!\n"
        "//! ## Ground truth\n"
        "//!\n"
        "//! `erts/emulator/beam/erl_bif_info.c:2764-2772` (`build_snifs_term`) walks the C\n"
        "//! `bif_table` from index 0 **upward** and conses each `{Module, Name, Arity}` onto\n"
        "//! the accumulating list, so `BIF_SIZE` rows come out in REVERSE table order —\n"
        "//! which is why [`snifs`] reverses this table rather than exposing it as-is.\n"
        "//!\n"
        "//! `is_snif_term` (`erl_bif_info.c:2743`) additionally skips a row whose *module*\n"
        "//! atom name contains a `.` (a package-qualified name). No `bif.tab` row in this\n"
        "//! tree has one, so `gen_snifs.escript` asserts that absence and this roster stays\n"
        "//! filter-free; a dotted row appears as a generator abort, never as a silently\n"
        "//! dropped entry.\n"
        "//!\n"
        "//! ## OTP-version skew\n"
        "//!\n"
        "//! The roster describes *this* source tree (OTP 30), which declares three rows an\n"
        "//! OTP-29 oracle does not have — `erlang:pid_to_binary/1`, `erlang:port_to_binary/1`\n"
        "//! and `erlang:ref_to_binary/1` (the same class of skew as the\n"
        "//! `erts_debug:instructions/0` roster). `tests/snifs.rs` pins that difference\n"
        "//! against a live oracle instead of removing the rows.\n"
        "\n",
    Body =
        "/// One declared `bif.tab` row: `module:function/arity`.\n"
        "///\n"
        "/// The term shape of one `system_info(snifs)` element is the 3-tuple\n"
        "/// `{Module, Name, Arity}`, i.e. these three fields in this order.\n"
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]\n"
        "pub struct Snif {\n"
        "    /// The module, verbatim (`erlang`, `erts_internal`, `records`, …).\n"
        "    pub module: &'static str,\n"
        "    /// The function name, with the quotes of a quoted operator stripped (`+`, `=:=`).\n"
        "    pub function: &'static str,\n"
        "    /// The arity.\n"
        "    pub arity: u32,\n"
        "}\n"
        "\n"
        "/// The roster in `bif.tab` file order — the table `snifs()` reverses.\n"
        "pub(crate) const SNIFS_TABLE_ORDER: &[Snif] = &[\n"
        "    " ++ row_text(Rows) ++ "\n"
        "];\n"
        "\n"
        "/// The static-NIF roster in the oracle's `erlang:system_info(snifs)` order.\n"
        "///\n"
        "/// `build_snifs_term` conses the `bif.tab` rows forward onto the tail, so the\n"
        "/// oracle's list is this table reversed; that reversal lives here once so no\n"
        "/// consumer re-derives it.\n"
        "pub fn snifs() -> impl DoubleEndedIterator<Item = Snif> + ExactSizeIterator {\n"
        "    SNIFS_TABLE_ORDER.iter().rev().copied()\n"
        "}\n",
    lists:flatten([Header, Doc, Body]).

row_text(Rows) ->
    string:join([row(R) || R <- Rows], ",\n    ").

row({Module, Name, Arity}) ->
    io_lib:format("Snif { module: \"~s\", function: \"~s\", arity: ~p }",
                  [Module, escape(Name), Arity]).

escape(Name) ->
    [escape_char(C) || C <- Name].

escape_char($") -> "\\\"";
escape_char($\\) -> "\\\\";
escape_char(C) -> C.

abort(Msg) ->
    io:format(standard_error, "gen_snifs: ~s~n", [Msg]),
    halt(1).
