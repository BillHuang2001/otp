-module(gc_class_compile).
%% The **C4 (a real in-process compile)** workload class of the GC-strategy A/B harness
%% (`../gc_strategy_classes.rs`).
%%
%% `run(N)` compiles ONE REAL `lib/*/src/*.erl` source N times, in the **calling** process
%% (`no_spawn_compiler_process` — contract A.9's E0 option list plus `deterministic`/`no_docs`), into
%% N distinct output directories, and answers `{N, Module}` with the last compile's module name. The
%% compile's readers, passes and code emission therefore allocate on this process's own heap, which
%% is what makes the class's heap activity real and collectable; a compile handed to a spawned
%% compiler child would do its work on somebody else's heap and measure nothing.
%%
%% The paths come from the environment (`BEAM_RT_GCH_SRC`, `BEAM_RT_GCH_OUT`) rather than from
%% arguments, so this fixture stays a plain path-free source: the harness sets both variables on the
%% child process it re-execs, and `os:getenv/1` reads the child's own environment. `<Out>/out<i>` is
%% created by the harness beforehand (`compile:file/2` does not create its `{outdir, Dir}`), and one
%% directory per compile keeps every artifact this run's own.
-export([run/1]).

run(N) when is_integer(N), N > 0 ->
    Source = env("BEAM_RT_GCH_SRC"),
    Base = env("BEAM_RT_GCH_OUT"),
    LastModule = compile_all(N, Source, Base, 1, none),
    {N, LastModule}.

compile_all(0, _Source, _Base, _Index, LastModule) ->
    LastModule;
compile_all(N, Source, Base, Index, _LastModule) ->
    OutDir = Base ++ "/out" ++ integer_to_list(Index),
    Options = [no_spawn_compiler_process, deterministic, no_docs, {outdir, OutDir}],
    case compile:file(Source, Options) of
        {ok, Module} ->
            compile_all(N - 1, Source, Base, Index + 1, Module);
        Other ->
            exit({compile_failed, Index, Other})
    end.

env(Name) ->
    case os:getenv(Name) of
        false -> exit({missing_env, Name});
        Value -> Value
    end.
