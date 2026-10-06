%% Mailbox/receive fixture for `tests/mailbox.rs` and the receive-seam unit tests in
%% `src/context.rs`.
%%
%% Rebuilt with the system `erlc` and committed as `mailbox.beam` (see
%% tests/fixtures/README.md for the regeneration recipe). Every export drives the
%% receive family through the *real* runtime glue — `send`, `loop_rec`,
%% `loop_rec_end`, `wait`, `remove_message` — so a run proves send+receive end to end.
-module(mailbox).

-export([selfsend/0, wait/0, scan/0]).

%% Oracle: `mailbox:selfsend()` -> hello.
%%
%% `self() ! hello` compiles to the `send` instruction (destination in x(0), message
%% in x(1)), and `receive X -> X end` matches it straight back out of the process
%% mailbox.
selfsend() ->
    self() ! hello,
    receive
        X -> X
    end.

%% Oracle: `mailbox:wait()` blocks on a fresh (empty) mailbox — `loop_rec` finds no
%% candidate and `wait` suspends. Used to assert the non-blocking entry's typed
%% `blocked` error and, once a message arrives, the resume path.
wait() ->
    receive
        X -> X
    end.

%% Oracle: `mailbox:scan()` -> 7.
%%
%% `42` fails the `{bar, X}` pattern and reaches `loop_rec_end` (the scan advances past
%% it); the second message, `{bar, 7}`, then matches.
scan() ->
    self() ! 42,
    self() ! {bar, 7},
    receive
        {bar, X} -> X
    end.
