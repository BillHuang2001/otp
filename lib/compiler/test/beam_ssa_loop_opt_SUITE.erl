-module(beam_ssa_loop_opt_SUITE).
-include_lib("common_test/include/ct.hrl").
-export([all/0, suite/0]).
-export([init_per_suite/1, end_per_suite/1]).
-export([loop_opt_preserves_result/1, kill_switches/1,
         off_by_default/1, non_trivial_loops_unchanged/1]).

suite() -> [{ct_hooks,[ts_install_cth]}, {timetrap,{minutes,5}}].
all() -> [loop_opt_preserves_result, kill_switches,
          off_by_default, non_trivial_loops_unchanged].
init_per_suite(Config) -> Config.
end_per_suite(_Config) -> ok.

loop_opt_preserves_result(Config) ->
    Mods = [{lp_count, counter_src()}, {lp_traverse, traverse_src()},
            {lp_nested, nested_src()}, {lp_ctor, invariant_ctor_src()}],
    lists:foreach(
      fun({Name, Src}) ->
              Plain = run(Config, Name, Src, []),
              Opt = run(Config, Name, Src, [loop_opt]),
              case {Plain, Opt} of
                  {R, R} -> ok;
                  _ -> ct:fail({result_mismatch, Name, Plain, Opt})
              end
      end, Mods).

kill_switches(Config) ->
    Src = nested_src(),
    Plain  = run(Config, kill_plain,  Src, []),
    Opt    = run(Config, kill_opt,    Src, [loop_opt]),
    NoLoop = run(Config, kill_noloop, Src, [loop_opt, no_ssa_opt_loop_opt]),
    NoSsa  = run(Config, kill_nossa,  Src, [loop_opt, no_ssa_opt]),
    case {Plain, Opt, NoLoop, NoSsa} of
        {R, R, R, R} -> ok;
        Other -> ct:fail({kill_switch_mismatch, Other})
    end.

off_by_default(Config) ->
    _ = run(Config, default_mod, counter_src(), []),
    Opts = compile:env_compiler_options(),
    case lists:member(loop_opt, Opts) of
        true -> ct:fail(loop_opt_enabled_by_env);
        false -> ok
    end.

non_trivial_loops_unchanged(Config) ->
    Src = raising_src(),
    R1 = (load(Config, lp_raising, Src, [loop_opt])):bad(3),
    R2 = (load(Config, lp_raising_plain, Src, [])):bad(3),
    case {R1, R2} of
        {R, R} when R =:= ok -> ok;
        Other -> ct:fail({raising_mismatch, Other})
    end.

run(Config, Name, Src, Opts) ->
    Mod = load(Config, Name, Src, Opts),
    Mod:sample().

%% Compile `Src' with `Opts', load the resulting module and return its name.
load(Config, Name, Src, Opts) ->
    Priv = ?config(priv_dir, Config),
    Dir = filename:join(Priv, atom_to_list(Name)),
    ok = filelib:ensure_dir(filename:join(Dir, "x")),
    File = filename:join(Dir, atom_to_list(Name) ++ ".erl"),
    ok = file:write_file(File, Src),
    Res = compile:file(File, [binary, return_errors, return_warnings | Opts]),
    {Mod, Bin} = case Res of
                     {ok, M0, B0} -> {M0, B0};
                     {ok, M0, B0, _Ws} -> {M0, B0};
                     _ -> ct:fail({compile_failed, Name, Res})
                 end,
    {module, Mod} = code:load_binary(Mod, File, Bin),
    Mod.

counter_src() ->
    "-module(lp_count).\n-export([sample/0, count/1]).\n"
    "sample() -> [count(N) || N <- lists:seq(0, 20)].\n"
    "count(N) -> count(N, 0).\n"
    "count(0, Acc) -> Acc;\n"
    "count(N, Acc) when N > 0 -> count(N - 1, Acc + N).\n".
traverse_src() ->
    "-module(lp_traverse).\n-export([sample/0, rev/1]).\n"
    "sample() -> [length(rev(lists:seq(1, N))) || N <- lists:seq(0, 50)].\n"
    "rev(L) -> rev(L, []).\n"
    "rev([H | T], Acc) -> rev(T, [H | Acc]);\n"
    "rev([], Acc) -> Acc.\n".
nested_src() ->
    "-module(lp_nested).\n-export([sample/0, outer/1]).\n"
    "sample() -> [outer(N) || N <- lists:seq(0, 12)].\n"
    "outer(N) -> outer(N, 0).\n"
    "outer(0, Acc) -> Acc;\n"
    "outer(N, Acc) when N > 0 -> outer(N - 1, Acc + inner(N, 0)).\n"
    "inner(0, Acc) -> Acc;\n"
    "inner(M, Acc) when M > 0 -> inner(M - 1, Acc + M).\n".
invariant_ctor_src() ->
    "-module(lp_ctor).\n-export([sample/0, f/2]).\n"
    "sample() -> [f(a, N) || N <- lists:seq(0, 25)].\n"
    "f(A, N) -> loop(A, N, 0).\n"
    "loop(_A, 0, Acc) -> Acc;\n"
    "loop(A, N, Acc) when N > 0 ->\n"
    "    T = {A},\n"
    "    A1 = element(1, T),\n"
    "    loop(A1, N - 1, {Acc, A1}).\n".
raising_src() ->
    "-module(lp_raising).\n-export([sample/0, bad/1]).\n"
    "sample() -> bad(3).\n"
    "bad(X) -> bad(X, 0).\n"
    "bad(0, _) -> ok;\n"
    "bad(N, Acc) when N > 0 ->\n"
    "    try bad(N - 1, Acc + N) of R -> R\n"
    "    catch error:E -> {E, ok} end.\n".
