%%% Fixture for `tests/process_info_binary.rs` — the `binary` item of
%%% `erlang:process_info/2` for a process querying **itself**, for a process holding
%%% nothing, and for a process probing **another** pid.
%%%
%%% `self_payload/1` builds a `Bytes`-byte binary with compiled bitstring code
%%% (`bs_create_bin` above `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes, so it is a genuine
%%% off-heap reference-counted payload) and then asks for its own roster. `B` is used
%%% after the query, so it is live across it — the oracle reports a live payload and
%%% never a collected one. The size is a runtime value on purpose: a compile-time
%%% constant would be folded into a literal and never reach the payload reservoir.
%%%
%%% `self_none/0` asks the same question from a process that built nothing.
%%%
%%% `hold/1` builds one off-heap binary, reports readiness and then parks holding it,
%%% which is the shape a **second** process can probe (`probe/1`).
%%%
%%% The `report_*` wrappers exist for the oracle probe: a spawned export process's
%%% return value is unobservable in Erlang, so each answers through message passing.
%%% `canon/1` reduces a roster to the flat integer list both sides compare
%%% (`[Count, PositivePtr?, Size, Refc, …]`), because a payload pointer is an address
%%% and can only be compared as a shape.
%%%
%%% `tests/process_info_binary.rs` compiles this at test time with the system `erlc`
%%% (cwd = the scratch dir, bare `pi_binary.erl`); the fixture is never committed as a
%%% `.beam`.
-module(pi_binary).
-export([self_payload/1, self_payload_list/1, self_none/0, hold/1, probe/1, sink/0,
         report_payload/2, report_payload_list/2, report_none/1, report_other/2,
         report_hold/1, canon/1]).

%% Build a `Bytes`-byte off-heap binary, then read our OWN `binary` roster.
self_payload(Bytes) ->
    B = <<0:(Bytes*8)>>,
    R = erlang:process_info(self(), binary),
    {R, byte_size(B)}.

%% The same query in the LIST form (`process_info/2` answer items in the requested
%% order), which is the other branch the roster read is gated on.
self_payload_list(Bytes) ->
    B = <<0:(Bytes*8)>>,
    Items = erlang:process_info(self(), [status, binary]),
    {binary, R} = lists:keyfind(binary, 1, Items),
    {R, byte_size(B)}.%% The same self query from a process that built no off-heap binary.
self_none() ->
    erlang:process_info(self(), binary).

%% Build one off-heap binary, report readiness, then park holding it.
hold(Parent) ->
    B = <<0:(40000*8)>>,
    Parent ! {ready, self(), byte_size(B)},
    receive stop -> {stopped, byte_size(B)} end.

%% Another process's `binary` roster, read from here.
probe(Pid) ->
    erlang:process_info(Pid, binary).

%% The parent of the parked holder: it receives and drops the readiness report.
sink() ->
    receive
        stop -> ok;
        _ -> sink()
    end.

%% The oracle-side wrappers (a spawned export process cannot return a value).
report_payload(Parent, Bytes) ->
    {Roster, Size} = self_payload(Bytes),
    Parent ! {result, canon(Roster) ++ [Size]}.

report_payload_list(Parent, Bytes) ->
    {Roster, Size} = self_payload_list(Bytes),
    Parent ! {result, canon({binary, Roster}) ++ [Size]}.report_none(Parent) ->
    Parent ! {result, canon(self_none())}.

report_other(Parent, Pid) ->
    Parent ! {result, canon(probe(Pid))}.

report_hold(Parent) ->
    hold(Parent).

%% `[{Ptr, Size, Refc}, …]` as `[Count, IsPositiveInt?, Size, Refc, …]`.
canon({binary, Roster}) ->
    [length(Roster) | lists:append([[positive(P), S, Rc] || {P, S, Rc} <- Roster])].

%% 1 when `P` is a positive integer (the shape a payload pointer must have), else 0.
positive(P) when is_integer(P), P > 0 -> 1;
positive(_) -> 0.
