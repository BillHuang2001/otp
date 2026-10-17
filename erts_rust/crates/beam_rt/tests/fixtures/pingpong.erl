%% The two-process **ping-pong** fixture for the `run`-route sleep accounting
%% (`tests/run_route_sleep.rs`).
%%
%% `go/1` spawns one worker and then does `N` synchronous round trips with it:
%% `Worker ! {self(), N}` and a bound-variable `receive {_, N}` that copies the
%% message onto the worker's heap and the reply back onto the parent's — the same
%% shape as the bench driver's `copy_bound` `send_recv_plain` case, entered through
%% the CLI's `run` action instead of the driver.
%%
%% The shape is chosen for the *scheduler*: every iteration parks the parent on an
%% empty mailbox until the worker's reply arrives, so the run queue empties between
%% iterations and the pass boundary is reached over and over. A pass that slept a
%% poll slice before looking for work would pay that sleep on every iteration.
%%
%% A missing reply fails **loudly** (`{pingpong_timeout, N}`) instead of hanging or
%% silently answering `N`.
-module(pingpong).
-export([go/1]).

go(N) when is_integer(N), N >= 0 ->
    Worker = spawn(fun() -> worker() end),
    loop(N, Worker),
    Worker ! stop,
    N.

loop(0, _Worker) ->
    ok;
loop(N, Worker) ->
    Worker ! {self(), N},
    case receive {_, N} -> ok after 5000 -> timeout end of
        ok -> loop(N - 1, Worker);
        timeout -> erlang:error({pingpong_timeout, N})
    end.

worker() ->
    receive
        {From, N} ->
            From ! {self(), N},
            worker();
        stop ->
            ok
    end.
