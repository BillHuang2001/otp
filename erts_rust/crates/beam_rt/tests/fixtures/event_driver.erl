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
-module(event_driver).
-export([recv/0]).

recv() ->
    Port = open_port({spawn_driver, "{path}"}, [binary]),
    true = port_command(Port, <<"go">>),
    Read = receive {Port, {data, D1}} -> D1 after 5000 -> timeout end,
    Timer = receive {Port, {data, D2}} -> D2 after 5000 -> timeout end,
    {Read, Timer}.
