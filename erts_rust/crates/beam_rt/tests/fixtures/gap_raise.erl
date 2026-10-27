%% Compiled at test time by `tests/gap_raise_catch.rs` — no `.beam` is committed for it.
%%
%% The catcher deliberately never spells the reason atom. A capability gap's reason is built by
%% the runtime from a Rust string (`unsupported: …`), so it lives in **no** module's atom chunk:
%% neither this module nor any preloaded one can name it, and the boot seed does not hold it
%% either. `caught/0` is therefore the end-to-end witness that the runtime names the atom in the
%% shared live namespace before the raise reaches the engine — the only way the engine can land
%% the raise in a live catch frame instead of letting it escape the process.
%%
%% `erlang:system_monitor/1` is the probe: it is a **curated** capability gap of this runtime
%% (an `erlang`-namespace BIF with no `bif.tab` entry, see `src/error.rs`'s
%% `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`), so the raise is produced by the runtime itself and does
%% not depend on any other crate's coverage. The oracle does not have the gap (it raises
%% `badarg` for the same call), which is why only the runtime's own run asserts the reason.
-module(gap_raise).
-export([caught/0, uncaught/0]).

%% What a `try … catch C:R:S` around a gap-producing call observes.
%%
%% Prints a pure-`~p` line so the transcript is stable across tiers: the class must be `error`,
%% the reason must be an **atom** whose name carries the `unsupported` gap prefix, and the
%% stacktrace must be non-empty. Nothing here names the reason atom.
caught() ->
    try trip() of
        Value ->
            io:format("no_raise ~p~n", [Value]),
            ok
    catch
        Class:Reason:Stack ->
            io:format("caught class=~p gap_atom=~p frames_nonempty=~p~n",
                      [Class, is_gap_atom(Reason), Stack =/= []]),
            ok
    end.

%% The same call with no catch frame: the process dies with the gap's own reason, and the CLI
%% must still classify it as `unsupported` (exit 3) exactly as before.
uncaught() ->
    trip().

trip() ->
    erlang:system_monitor(0).

is_gap_atom(Reason) when is_atom(Reason) ->
    lists:prefix("unsupported", atom_to_list(Reason));
is_gap_atom(_) ->
    false.
