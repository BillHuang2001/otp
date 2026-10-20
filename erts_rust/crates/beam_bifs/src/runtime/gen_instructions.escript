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
%% @doc Generate `instructions.rs' — the `opc[]' dispatch-name roster behind
%% `erts_debug:instructions/0'.
%%
%% The C BIF (`erts/emulator/beam/beam_debug.c:376-393',
%% `erts_debug_instructions_0') builds
%% `[opc[I].name || I <- 0..num_instructions-1]' over the table `beam_makeops'
%% emits into the generated `beam_opcodes.c'. Those names carry the
%% **operand-type suffixes** (`allocate_heap_tIt', `append_cons_Is', …) and are
%% *not* `genop.tab''s names, so they cannot be derived from the loader's
%% opcode roster: the only faithful source is the generator itself.
%%
%% This script therefore runs this repository's real
%% `erts/emulator/utils/beam_makeops' over the `FLAVOR=jit' x86 tables — the
%% flavour the oracle itself runs — and parses **only** the `opc[]' initializer
%% of the generated `beam_opcodes.c' (the file also carries `gen_opc[]',
%% `instr_*' and other tables, so a plain scan of the whole file would pick up
%% unrelated names).
%%
%% Usage (from the OTP checkout root — `$ERL_TOP', else found by walking up, is
%% used to locate `erts/emulator'):
%%
%%     escript erts_rust/crates/beam_bifs/src/runtime/gen_instructions.escript
%%
%% Options:
%%   `--top <dir>'   the OTP root (default: `$ERL_TOP', else found by walking
%%                   up from the current directory)
%%   `--out <file>'  the output path (default: `instructions.rs' next to this
%%                   script)
%%
%% Requires `perl' on `PATH' (the ERTS build system's own dependency).
%%
%% **The emitted file is generated data — never hand-edit it.** The output is
%% pinned by the tests in `erts_debug.rs', which compare it against a live
%% oracle modulo a documented OTP-version-skew exception set.

main(Args) ->
    Top = opt(Args, "--top", default_top()),
    Out = opt(Args, "--out", default_out()),
    Emu = filename:join([Top, "erts", "emulator"]),
    ok = assert_file(filename:join([Emu, "utils", "beam_makeops"])),
    GenDir = run_makeops(Emu),
    Names = parse_opcodes(filename:join(GenDir, "beam_opcodes.c")),
    %% `unicode:characters_to_binary' so a non-ASCII codepoint in the prose
    %% header is emitted as UTF-8 rather than tripping the byte-list limit.
    ok = file:write_file(Out, unicode:characters_to_binary(render(Names))),
    io:format("wrote ~s (~p instructions)~n", [Out, length(Names)]).

%% ---------------------------------------------------------------------------
%% Options / paths
%% ---------------------------------------------------------------------------
opt(["--top", T | _], "--top", _) -> T;
opt(["--out", O | _], "--out", _) -> O;
opt([_ | T], Key, Default) -> opt(T, Key, Default);
opt([], _, Default) -> Default.

default_out() ->
    filename:join(filename:dirname(filename:absname(escript:script_name())),
                  "instructions.rs").

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
                    abort("cannot find the OTP root (erts/emulator/utils/beam_makeops); "
                          "pass --top or set ERL_TOP");
                _ ->
                    search_up(Parent)
            end
    end.

is_top(Dir) ->
    filelib:is_regular(filename:join([Dir, "erts", "emulator", "utils",
                                      "beam_makeops"])).

assert_file(Path) ->
    case filelib:is_regular(Path) of
        true -> ok;
        false -> abort("missing " ++ Path)
    end.

%% ---------------------------------------------------------------------------
%% Run beam_makeops
%% ---------------------------------------------------------------------------
%% The invocation is the ERTS build system's own: `-wordsize 8', the `-jit yes'
%% flavour plus the `beam/jit/x86/*' tables (passing `beam/emu/ops.tab' as well
%% is an error — it duplicates the `L' label).
run_makeops(Emu) ->
    Perl = case os:find_executable("perl") of
               false -> abort("perl not found on PATH (beam_makeops is a perl script)");
               P -> P
           end,
    GenDir = filename:join(tmp_root(), "beam_bifs_instructions"),
    ok = filelib:ensure_dir(filename:join(GenDir, "x")),
    Args = ["utils/beam_makeops",
            "-wordsize", "8",
            "-outdir", GenDir,
            "-jit", "yes",
            "-DUSE_VM_PROBES=0",
            "-DARCH_64=1",
            "-emulator",
            "../../lib/compiler/src/genop.tab",
            "beam/predicates.tab",
            "beam/generators.tab",
            "beam/jit/x86/ops.tab",
            "beam/jit/x86/predicates.tab",
            "beam/jit/x86/generators.tab"],
    Port = open_port({spawn_executable, Perl},
                     [{args, Args}, {cd, Emu}, {env, [{"LANG", "C"}]},
                      exit_status, stderr_to_stdout, binary]),
    {Status, Output} = collect(Port, []),
    case Status of
        0 ->
            ok = assert_file(filename:join(GenDir, "beam_opcodes.c")),
            GenDir;
        _ ->
            abort(io_lib:format("beam_makeops failed (exit ~p):~n~s",
                                [Status, Output]))
    end.

collect(Port, Acc) ->
    receive
        {Port, {data, Data}} ->
            collect(Port, [Acc, Data]);
        {Port, {exit_status, Status}} ->
            {Status, Acc}
    after 120000 ->
            port_close(Port),
            abort("beam_makeops timed out")
    end.

tmp_root() ->
    case [D || V <- ["TMPDIR", "TMP", "TEMP"],
              D <- [os:getenv(V)], D =/= false] of
        [Dir | _] -> Dir;
        [] -> "."
    end.

%% ---------------------------------------------------------------------------
%% Parse the `opc[]' initializer
%% ---------------------------------------------------------------------------
%% Every row is `/* N */  {"name", {…}, "Sig"}'. Only the rows *inside* the
%% `const OpEntry opc[] = { … };' block count, and only the name is taken.
parse_opcodes(File) ->
    {ok, Bin} = file:read_file(File),
    Lines = string:split(binary_to_list(Bin), "\n", all),
    {Rows, Declared} = scan(Lines, false, [], undefined, File),
    case Declared of
        undefined ->
            abort("no `num_instructions' declaration in " ++ File);
        N when N =/= length(Rows) ->
            abort(io_lib:format("num_instructions is ~p but opc[] holds ~p rows",
                                [N, length(Rows)]));
        _ ->
            ok
    end,
    ok = check_indices(Rows, File),
    [Name || {_Index, Name} <- lists:keysort(1, Rows)].

scan([], _InBlock, Rows, Declared, _File) ->
    {Rows, Declared};
scan([Line | Tail], InBlock, Rows, Declared, File) ->
    Trimmed = string:trim(Line),
    case {InBlock, Trimmed} of
        {false, "const OpEntry opc[] = {" ++ _} ->
            scan(Tail, true, Rows, Declared, File);
        {false, _} ->
            scan(Tail, false, Rows, num_instructions(Line, Declared), File);
        {true, "};"} ->
            scan(Tail, false, Rows, Declared, File);
        {true, ""} ->
            scan(Tail, true, Rows, Declared, File);
        {true, _} ->
            scan(Tail, true, [row(Line, File) | Rows], Declared, File)
    end.

row(Line, File) ->
    case re:run(Line, "^/\\* +([0-9]+) \\*/  \\{\"([^\"]*)\",",
                [{capture, all_but_first, list}]) of
        {match, [Index, Name]} -> {list_to_integer(Index), Name};
        nomatch -> abort("unparsable opc[] row in " ++ File ++ ": " ++ Line)
    end.

num_instructions(Line, Declared) ->
    case re:run(Line, "^const int num_instructions = ([0-9]+);",
                [{capture, all_but_first, list}]) of
        {match, [N]} -> list_to_integer(N);
        nomatch -> Declared
    end.

%% The rows must cover `0..N-1' exactly once — anything else means the generated
%% file is not the table this script believes it is.
check_indices(Rows, File) ->
    Indices = lists:sort([I || {I, _} <- Rows]),
    case Indices =:= lists:seq(0, length(Rows) - 1) of
        true -> ok;
        false -> abort("opc[] indices are not 0..N-1 in " ++ File)
    end.

%% ---------------------------------------------------------------------------
%% Render
%% ---------------------------------------------------------------------------
render(Names) ->
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
        "//! The `beam_makeops`-generated `opc[]` dispatch-name roster behind\n"
        "//! `erts_debug:instructions/0`.\n"
        "//!\n"
        "//! **This file is GENERATED DATA — never hand-edit it.** It is emitted by the\n"
        "//! committed generator `gen_instructions.escript` in this directory, which runs\n"
        "//! this repository's real `erts/emulator/utils/beam_makeops` over the\n"
        "//! `FLAVOR=jit` x86 tables and parses the `opc[]` initializer of the generated\n"
        "//! `beam_opcodes.c`. To regenerate (from the OTP checkout root):\n"
        "//!\n"
        "//! ```text\n"
        "//! escript erts_rust/crates/beam_bifs/src/runtime/gen_instructions.escript\n"
        "//! ```\n"
        "//!\n"
        "//! ## Why this file is long\n"
        "//!\n"
        "//! It *is* the table: one `&str` per `beam_makeops` dispatch entry, in `opc[]`\n"
        "//! index order. There is nothing to factor out and nothing to shorten — the\n"
        "//! names carry the operand-type suffixes `beam_makeops` derives\n"
        "//! (`allocate_heap_tIt`, `append_cons_Is`, …), so they are *not* `genop.tab`'s\n"
        "//! opcode names and cannot be derived from the loader's roster. Splitting the\n"
        "//! table would only obscure that it is machine output.\n"
        "//!\n"
        "//! ## Ground truth\n"
        "//!\n"
        "//! `erts/emulator/beam/beam_debug.c:376-393` (`erts_debug_instructions_0`)\n"
        "//! builds `[opc[I].name || I <- 0..num_instructions-1]` — every row of the\n"
        "//! table, in dispatch order, with nothing filtered. `num_instructions` is the\n"
        "//! generated `beam_opcodes.c` constant, which the generator cross-checks\n"
        "//! against the row count before writing this file.\n"
        "//!\n"
        "//! ## OTP-version skew\n"
        "//!\n"
        "//! The table describes *this* source tree (OTP 30). A live oracle of a\n"
        "//! different OTP release differs, because `ops.tab` itself differs; the test\n"
        "//! module of `super::erts_debug` therefore carries an explicit, documented\n"
        "//! exception set for the release skew and pins this roster's own length.\n"
        "\n",
    Body = "/// The `opc[]` roster in dispatch order: `INSTRUCTIONS[N]` is the name of\n"
        "/// instruction `N`.\n"
        "pub(crate) const INSTRUCTIONS: &[&str] = &[\n"
        "    " ++ row_text(Names) ++ "\n"
        "];\n",
    lists:flatten([Header, Doc, Body]).

%% Two names per line keeps every generated line inside the 100-column style the
%% rest of the crate uses (some dispatch names are long), without pretending the
%% table is prose.
row_text(Names) ->
    string:join([string:join([quote(N) || N <- Chunk], ", ")
                 || Chunk <- chunk(Names, 2)], ",\n    ").

chunk([], _N) -> [];
chunk(List, N) when length(List) =< N -> [List];
chunk(List, N) ->
    {Head, Tail} = lists:split(N, List),
    [Head | chunk(Tail, N)].

quote(Name) ->
    [$", [escape(C) || C <- Name], $"].

escape($") -> "\\\"";
escape($\\) -> "\\\\";
escape(C) -> C.

abort(Msg) ->
    io:format(standard_error, "gen_instructions: ~s~n", [Msg]),
    halt(1).
