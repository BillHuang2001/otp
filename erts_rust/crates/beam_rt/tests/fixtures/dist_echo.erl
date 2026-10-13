%% The inbound-distribution fixture (`tests/live_inbound.rs`).
%%
%% `wait/0` registers the name `echo` on the calling process and then blocks until a
%% message arrives, returning it. A real `erl` peer that reaches this node's
%% distribution listener sends `{echo, Node} ! Msg`, which arrives as a `DOP_REG_SEND`
%% addressed to the registered name — so the returned term *is* the observable proof
%% that an inbound distribution message reached a running local process's mailbox.
%%
%% The `after 20` re-arms a deadline on every iteration, and it is **load-bearing** for
%% the CLI `run` route: a bare `receive` with no deadline leaves the scheduler quiescent
%% (an empty run queue, nothing to poll, no pending timer), so `Runtime::run_scheduled`
%% stops with its honest `blocked` diagnostic before the peer's message can arrive — the
%% same rule `tests/dist.rs`'s keep-alive fixture relies on. Each arm's deadline keeps a
%% pass stepping, so the distribution half (`pump_dist`) keeps polling for the inbound
%% connection and message.
-module(dist_echo).

-export([wait/0]).

wait() -> register(echo, self()), loop().

loop() -> receive M -> M after 20 -> loop() end.
