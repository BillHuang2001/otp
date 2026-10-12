%% The driver for the scheduled-route in-process `compile:file/2` regression case
%% (`tests/compile_file.rs::a_scheduled_process_compiles_a_source_file_in_process_like_the_oracle`).
%%
%% `go/2` takes the source file to compile and the directory the produced object code must
%% land in, drives OTP's compiler **in-process** in the calling (scheduled) process — the
%% contract A.9 *E0* shape, `no_spawn_compiler_process` + `deterministic` + `{outdir, Dir}` —
%% and answers the plain integer `1` on success, a distinct integer otherwise. The returned
%% integer is the only observable, so the built runtime's own `run` output and the system
%% oracle's `io:format("~w~n", …)` are directly comparable.
-module(sched_compile_driver).

-export([go/2]).

go(Source, OutDir) ->
    case compile:file(Source, [no_spawn_compiler_process, deterministic, {outdir, OutDir}]) of
        {ok, _Module} -> 1;
        _ -> 3
    end.
