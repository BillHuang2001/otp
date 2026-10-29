%% Diagnostic probe for RC-A of `results/khepri-notes.md`.
%%
%% The runtime's `prim_file` iodata walk rejects a *valid* `iodata()` whose
%% (improper) list tail is a binary. `dets_v9:init_file/17` writes exactly that
%% shape (`{0, [FileHeader | <<0:(4*SEGARRSZ)/unit:8>>]}`), so `dets:open_file/2`
%% dies with `{case_clause,{error,badarg}}` in `dets_utils:pwrite/3`; the oracle
%% answers `{ok, Tab}`.
%%
%% Erlc: `erlc -o . iodata_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'iodata_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'iodata_probe:main(), halt(0).'`
-module(iodata_probe).
-export([main/0]).

main() ->
    _ = file:delete("/tmp/iodata_probe.dat"),
    {ok, Fd} = file:open("/tmp/iodata_probe.dat", [write, raw, binary, read]),
    show("file:write(Fd, [<<\"ab\">> | <<\"cd\">>])",
         catch file:write(Fd, [<<"ab">> | <<"cd">>])),
    show("file:pwrite(Fd, 0, [<<\"ab\">> | <<\"cd\">>])",
         catch file:pwrite(Fd, 0, [<<"ab">> | <<"cd">>])),
    show("file:pwrite(Fd, [{0, [<<\"ab\">> | <<\"cd\">>]}])",
         catch file:pwrite(Fd, [{0, [<<"ab">> | <<"cd">>]}])),
    show("file:write(Fd, [<<\"ab\">>, <<\"cd\">>])   %% proper list",
         catch file:write(Fd, [<<"ab">>, <<"cd">>])),
    _ = file:close(Fd),
    _ = file:delete("/tmp/iodata_probe.dets"),
    show("dets:open_file(iodata_probe_tab, [{file, \"/tmp/iodata_probe.dets\"}])",
         catch dets:open_file(iodata_probe_tab, [{file, "/tmp/iodata_probe.dets"}])),
    ok.

show(Name, Value) ->
    io:format("~-52s => ~p~n", [Name, Value]).
