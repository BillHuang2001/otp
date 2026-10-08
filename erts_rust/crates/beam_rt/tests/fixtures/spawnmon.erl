%% Fixture for `tests/spawn_monitor.rs` (compiled at test time — never committed as a
%% `.beam`).
%%
%% `erlang:spawn_monitor/1,2,3,4` are `erlang.erl` wrappers over
%% `erlang:spawn_opt(…, [monitor])`, served by the runtime-native floor
%% (`src/native/process.rs`). This module is run **unchanged** under both runtimes (this
%% one through `beam_rt run <file> spawnmon:<fn>/0` and the system `erl` oracle), so every
%% answer is compared on the same program.
-module(spawnmon).

%% `run/0` covers the two cases the suite pins; `local_node/0` and `badargs/0` add the
%% remaining arities and every guard-failing form, and `remote/0`/`remote4/0` the node form
%% this runtime reports as its typed capability gap.
-export([run/0, local_node/0, badargs/0, remote/0, remote4/0, child/0]).

%% Case (a): a child that exits **immediately** still delivers its `{'DOWN', …}` — proof
%% the monitor is established atomically, i.e. before the child's first schedulable
%% moment, so it can never run and die unnoticed between its creation and the monitor.
%% The answer is `{pid, true, true, normal}` (the pid is a pid, the reference is a
%% reference, the exit reason is `normal`).
%%
%% Case (b): the guard-failing form — `spawn_monitor/1` guards `is_function(F, 0)` — is
%% the oracle's `badarg`, never a fabricated child.
%%
%% The `after 5000` clauses bound every probe; a `{'DOWN'}` that never arrives is reported
%% as `down_timeout` rather than blocking the suite.
run() ->
    {Pid, Ref} = erlang:spawn_monitor(fun() -> ok end),
    Down = receive
               {'DOWN', Ref, process, Pid, Reason} ->
                   {pid, is_pid(Pid), is_reference(Ref), Reason}
           after 5000 ->
               down_timeout
           end,
    BadArg = try
                 erlang:spawn_monitor(fun(X, Y) -> {X, Y} end)
             catch
                 error:badarg -> badarg
             end,
    {Down, BadArg}.

%% The same node argument (`erlang:node()` — this runtime's own node) for every other
%% arity: `/2` (a 0-arity fun), `/3` (a module/function child) and `/4` (the node plus the
%% module/function child). Each must answer `{Pid, Ref}` and deliver its `{'DOWN', …,normal}`:
%% `{true,true,true}`.
local_node() ->
    {P2, R2} = erlang:spawn_monitor(node(), fun() -> ok end),
    {P3, R3} = erlang:spawn_monitor(?MODULE, child, []),
    {P4, R4} = erlang:spawn_monitor(node(), ?MODULE, child, []),
    {down(R2, P2), down(R3, P3), down(R4, P4)}.

%% Every guard-failing form is the oracle's `badarg` (no child is created):
%% `[badarg,badarg,badarg,badarg,badarg]`.
%%   * `/1`: a fun of the wrong arity (`is_function(F, 0)`);
%%   * `/2`: a non-atom node (`is_atom(Node)`);
%%   * `/3`: a non-list argument list (`is_list(Args)`);
%%   * `/4`: a non-atom node, and a non-atom module.
%% Each rejection is caught **in the same function** that makes the call (never through a
%% helper fun), so the `try` frame and the raise share one function's stack frame.
badargs() ->
    B1 = try
             erlang:spawn_monitor(fun(X, Y) -> {X, Y} end)
         catch
             error:badarg -> badarg
         end,
    B2 = try
             erlang:spawn_monitor(42, fun() -> ok end)
         catch
             error:badarg -> badarg
         end,
    B3 = try
             erlang:spawn_monitor(?MODULE, child, not_a_list)
         catch
             error:badarg -> badarg
         end,
    B4 = try
             erlang:spawn_monitor(42, ?MODULE, child, [])
         catch
             error:badarg -> badarg
         end,
    B5 = try
             erlang:spawn_monitor(node(), 42, child, [])
         catch
             error:badarg -> badarg
         end,
    [B1, B2, B3, B4, B5].

%% The `Node` forms on a node that is **not** this one: `erlang.erl` routes them through the
%% distributed `erlang:spawn_request/5` (contract A.5), so this runtime answers the typed
%% capability gap rather than fabricating a child. Only `tests/spawn_monitor.rs`'s gap test
%% calls these; the oracle cross-check deliberately does not, because a peer-less oracle
%% answers `{Pid, Ref}` for a name it cannot reach and logs a warning report.
remote() ->
    erlang:spawn_monitor('other@nohost', fun() -> ok end).

remote4() ->
    erlang:spawn_monitor('other@nohost', ?MODULE, child, []).

%% The module/function-bodied child the `/3` and `/4` forms spawn.
child() ->
    ok.

%% Whether the monitored child's `{'DOWN', …, normal}` arrived within the bound.
down(Ref, Pid) ->
    receive
        {'DOWN', Ref, process, Pid, normal} -> true
    after 5000 -> down_timeout
    end.
