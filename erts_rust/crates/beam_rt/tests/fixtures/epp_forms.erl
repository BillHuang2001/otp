%% The `epp:parse_file/3` driver for the scheduled-route regression case
%% (`tests/compile_file.rs`). `go/1` takes the input source file as a char list, parses it
%% with `epp` and answers `{FormCount, Tags}` — a pure `~w`-printing value, so the built
%% runtime's own `run` output and the system oracle's `io:format("~w~n", …)` are directly
%% comparable. It drives the real `epp`/`erl_scan` io-device read path, which is the route
%% on which a delivery-driven collection of the receiving process used to corrupt its scan
%% options (and so produce one form too many).
-module(epp_forms).

-export([go/1]).

go(File) ->
    {ok, Forms} = epp:parse_file(File, [], []),
    {length(Forms), [element(1, Form) || Form <- Forms]}.
