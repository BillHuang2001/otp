%%% The **Erlang half** of `beam_rt`'s driver event/timer end-to-end test (contract A.4), compiled
%%% by `tests/driver.rs` at test time — see `tests/fixtures/README.md` for the recipe.
%%%
%%% `{path}` is replaced with the compiled `event_driver.c` shared object's path (without the
%%% platform suffix) before `erlc` runs: the path only exists once the fixture is built, which is
%%% exactly why this source is committed with a placeholder instead of a compiled `.beam`.
%%%
%%% `recv/0` opens the C driver port, sends it one byte (`port_command` → the driver's `output`,
%%% which writes to its internal pipe), and then collects the **two** messages the driver produces:
%%% `<<"R">>` from its `ready_input` callback (the runtime's poller reporting the pipe's read end,
%%% which the driver selected with `driver_select`) and `<<"T">>` from its `timeout` callback (the
%%% port timer the driver armed with `driver_set_timer`). Both `receive`s are bounded by `after 5000`
%%% so a missing byte surfaces as the atom `timeout` in the returned tuple rather than a hang.
%%%
%%% The three other entry points drive the same C source compiled in the other modes (`tests/driver.rs`
%%% prefixes a `#define`): `recv_v/0` reaches an entry that declares **only** `outputv` (the vector
%%% callback, which echoes the `port_command` payload back through `driver_output`), and
%%% `recv_term/0` reaches an entry whose `output` delivers a **term-data** term — `{7, 9}`, built from
%%% an `ERL_DRV_INT` and an `ERL_DRV_EXT2TERM` (ETF) operand — either to the port owner
%%% (`erl_drv_output_term`) or to `driver_connected`'s pid (`erl_drv_send_term`). A driver-emitted
%%% term is delivered **bare** — never wrapped in `{Port, Term}` — so the `receive` below matches the
%%% tuple itself, and a `timeout` names the path that did not fire.
-module(event_driver).
-export([recv/0, recv_v/0, recv_term/0]).

recv() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"go">>),
    Read = receive {Port, {data, D1}} -> D1 after 5000 -> timeout end,
    Timer = receive {Port, {data, D2}} -> D2 after 5000 -> timeout end,
    {Read, Timer}.

%% The vector-callback mode: the port answers `port_command`'s own bytes back, so the first
%% `{Port, {data, Data}}` message is the echo (the port's 50 ms timer answers `<<"T">>` far later,
%% and the driver never writes to the pipe this mode does not use).
recv_v() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"vv">>),
    receive {Port, {data, D}} -> D after 5000 -> timeout end.

%% The term-data mode: `{7, 9}` must arrive as a bare term (a `{Port, {data, …}}` wrapper would not
%% match this clause and would leave the receive to time out).
recv_term() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"go">>),
    receive {7, 9} -> {7, 9} after 5000 -> timeout end.
