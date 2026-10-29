%% Diagnostic probe for RC-A of `results/khepri-notes.md` — the `prim_file`
%% iodata walk and the *on-disk bytes* it produces.
%%
%% `dets_v9:init_file/17` writes a valid `iodata()` whose improper list tail is
%% a binary (`{0, [FileHeader | <<0:(4*SEGARRSZ)/unit:8>>]}`), and `ra_log_ets`
%% opens a `dets` file during `ra` start-up. The old observation was
%% `{error,badarg}` and `dets:open_file/2` dying with
%% `{case_clause,{error,badarg}}`; this probe re-checks the write path *and*
%% whether the bytes that reach the disk are the same on both runtimes (md5 of
%% the files, taken by the caller, after the probe prints their sizes).
%%
%% It writes only relative paths, so each runtime must be run in its own
%% throwaway cwd (which is what makes the two files comparable):
%%   md5sum rust/raw_probe.dat oracle/raw_probe.dat
%%   md5sum rust/dets_probe.dets oracle/dets_probe.dets
%%
%% Erlc: `erlc -o . iodata_probe.erl   %% (this file's sibling)`
%% Run:  `beam_rt -noshell -pa . -eval 'dets_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'dets_probe:main(), halt(0).'`
-module(dets_probe).
-export([main/0]).

main() ->
    _ = file:delete("raw_probe.dat"),
    show("file:open(raw_probe.dat, [write, raw, binary, read])",
         caught(fun() -> file:open("raw_probe.dat", [write, raw, binary, read]) end)),
    {ok, Fd} = file:open("raw_probe.dat", [write, raw, binary, read]),
    show("file:write(Fd, [<<\"ab\">> | <<\"cd\">>])",
         caught(fun() -> file:write(Fd, [<<"ab">> | <<"cd">>]) end)),
    show("file:pwrite(Fd, 8, [<<\"ef\">> | <<\"gh\">>])",
         caught(fun() -> file:pwrite(Fd, 8, [<<"ef">> | <<"gh">>]) end)),
    show("file:pwrite(Fd, [{0, [<<\"AB\">> | <<\"CD\">>]}])",
         caught(fun() -> file:pwrite(Fd, [{0, [<<"AB">> | <<"CD">>]}]) end)),
    show("file:position(Fd, eof)", caught(fun() -> file:position(Fd, eof) end)),
    show("file:close(Fd)", caught(fun() -> file:close(Fd) end)),
    show("file:read_file(raw_probe.dat)",
         caught(fun() -> file:read_file("raw_probe.dat") end)),

    _ = file:delete("dets_probe.dets"),
    show("dets:open_file(dets_probe_tab, [{file, \"dets_probe.dets\"}])",
         caught(fun() ->
                        dets:open_file(dets_probe_tab,
                                       [{file, "dets_probe.dets"}])
                end)),
    show("dets:insert(dets_probe_tab, [{k1,1},{k2,\"two\"},{k3,<<\"3\">>}])",
         caught(fun() ->
                        dets:insert(dets_probe_tab,
                                    [{k1, 1}, {k2, "two"}, {k3, <<"3">>}])
                end)),
    show("dets:sync(dets_probe_tab)", caught(fun() -> dets:sync(dets_probe_tab) end)),
    show("dets:lookup(dets_probe_tab, k2)",
         caught(fun() -> dets:lookup(dets_probe_tab, k2) end)),
    show("dets:info(dets_probe_tab, size)",
         caught(fun() -> dets:info(dets_probe_tab, size) end)),
    show("dets:close(dets_probe_tab)", caught(fun() -> dets:close(dets_probe_tab) end)),
    %% reopen the on-disk file: the header this runtime wrote must parse back
    show("dets:open_file(dets_probe_tab) (reopen the written file)",
         caught(fun() ->
                        dets:open_file(dets_probe_tab,
                                       [{file, "dets_probe.dets"}])
                end)),
    show("dets:lookup(dets_probe_tab, k1) after reopen",
         caught(fun() -> dets:lookup(dets_probe_tab, k1) end)),
    show("dets:close(dets_probe_tab) (second close)",
         caught(fun() -> dets:close(dets_probe_tab) end)),
    show("file size raw_probe.dat",
         caught(fun() -> filelib:file_size("raw_probe.dat") end)),
    show("file size dets_probe.dets",
         caught(fun() -> filelib:file_size("dets_probe.dets") end)),
    ok.

caught(Fun) ->
    try Fun() of
        {ok, Fd} when is_port(Fd) -> {ok, file};
        Value -> {ok, Value}
    catch
        Class:Reason -> {Class, Reason}
    end.

show(Name, Value) ->
    io:format("~-64s => ~p~n", [Name, Value]).
