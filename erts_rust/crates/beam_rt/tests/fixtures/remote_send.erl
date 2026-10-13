%% The outbound send-destination classification fixtures (`tests/remote_send.rs`).
%%
%% Every function here is one of the three send routes the runtime shares a single
%% destination classifier with (`src/context/send.rs::classify_send_destination`):
%%
%%   * `bang/2` / `bang_named/3` / `same_node/2` / `unregistered_same_node/1` are the
%%     **compiled `Dest ! Msg`** route (the compiler emits the `send` instruction, which
%%     `beam_engine` serves through `ExecutionContext::send_message_with_off_heap`);
%%   * `bif/2` / `bif_named/3` are the **`erlang:send/2` BIF** route (lowered to
%%     `call_ext erlang:send/2`, served by the `SendBridge` send hook).
%%
%% `same_node/2` and `unregistered_same_node/1` build the tuple from `node()`, so the
%% `{Name, Node}` pair really names *this* runtime's own node — the C's local-named-send
%% arm (`erts/emulator/beam/bif.c:2628-2655`), which delivers to `whereis(Name)` and
%% reports success without delivering when nothing is registered.
%%
%% `recv_once/0` is the same-node receiver: it registers a name and blocks until one
%% message arrives, so a delivered message is visible as its return value.
-module(remote_send).

-export([
    bang/2,
    bif/2,
    bang_named/3,
    bif_named/3,
    same_node/2,
    unregistered_same_node/1,
    recv_once/0
]).

%% The compiled `!` operator, with the destination passed in.
bang(Dest, Msg) ->
    Dest ! Msg.

%% The `erlang:send/2` BIF (the compiler lowers it to `call_ext erlang:send/2`).
bif(Dest, Msg) ->
    erlang:send(Dest, Msg).

%% The compiled `{Name, Node} ! Msg` pair, both parts built from the caller's atoms.
bang_named(Name, Node, Msg) ->
    {Name, Node} ! Msg.

%% The same pair through the BIF, so the two routes can be compared on one destination.
bif_named(Name, Node, Msg) ->
    erlang:send({Name, Node}, Msg).

%% The same-node pair, as compiled code builds it: `node()` is *this* runtime's node.
same_node(Name, Msg) ->
    {Name, node()} ! Msg.

%% A same-node pair whose name is never registered: accepted, and the message is dropped.
unregistered_same_node(Msg) ->
    {remote_send_noreg, node()} ! Msg.

%% Registers a name and answers the first message it receives (the same-node receiver).
recv_once() ->
    register(remote_send_recv, self()),
    receive
        M -> M
    end.
