%%
%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2025-2026. All Rights Reserved.
%%
%% Licensed under the Apache License, Version 2.0 (the "License");
%% you may not use this file except in compliance with the License.
%% You may obtain a copy of the License at
%%
%%     http://www.apache.org/licenses/LICENSE-2.0
%%
%% Unless required by applicable law or agreed to in writing, software
%% distributed under the License is distributed on an "AS IS" BASIS,
%% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
%% See the License for the specific language governing permissions and
%% limitations under the License.
%%
%% %CopyrightEnd%
%%
%% The **`{active, …}` socket-input** fixture for `tests/inet_active.rs` (contract A.4): every
%% case opens a real socket with `erlang:open_port({spawn_driver, "tcp_inet"|"udp_inet"}, [binary])`
%% and drives it with the **numeric** `erlang:port_control/3` requests the driver dispatches
%% (`inet_drv.c`'s request table, each offset by `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`), which is
%% exactly what `prim_inet` does underneath — so the identical source runs under this runtime and
%% under the system `erl` without `gen_tcp`/`gen_udp`/`prim_inet` in the picture at all.
%%
%% Each case is **self-contained in one VM**: the same process listens on `127.0.0.1:0` and opens
%% its own peer socket in the same run, so no external peer coordination is needed. Every case
%% blocks in a compiled `receive` for the delivered socket message, which is why the suite drives
%% it through the `run` action (a scheduled process) rather than `-eval`.
%%
%% Printed lines are `~w`-rendered and **normalised**: the port term is a per-runtime table slot
%% and a UDP source port is per-run, so both are replaced by the atoms `'$port'` / `'$source'`;
%% every other byte is the driver's own. Each case also prints content predicates
%% (`t1_first_is_hello=true` …) so no run can pass on an empty result.
-module(inet_active).
-export([t1/0, t2/0, t3/0, t4/0, t5/0, t6/0, t7/0]).

%% `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`: every numeric request is this plus the opcode.
-define(MAGIC, 16#03f1a300).
-define(REQ_OPEN, 1).
-define(REQ_CONNECT, 3).
-define(REQ_BIND, 6).
-define(REQ_SETOPTS, 7).
-define(REQ_ACCEPT, 26).
-define(REQ_LISTEN, 27).
-define(LOPT_ACTIVE, 22).
-define(LOPT_PACKET, 23).
-define(LOPT_MODE, 24).
-define(AF_INET, 1).
-define(AF_LOOPBACK, 4).
-define(SOCK_STREAM, 1).
-define(SOCK_DGRAM, 2).
-define(ACTIVE_TRUE, 1).
-define(ACTIVE_ONCE, 2).
-define(ACTIVE_MULTI, 3).
-define(MODE_LIST, 0).
-define(MODE_BINARY, 1).
-define(TCP_PB_2, 2).

%% An option record is `[code, value_be32]`; only `{active, N}` carries a trailing 16-bit counter
%% (`prim_inet:enc_opt_val/2`: `{active, once}` is `<<?INET_LOPT_ACTIVE:8, ?INET_ONCE:32>>`, while
%% `{active, N}` is `<<?INET_LOPT_ACTIVE:8, ?INET_MULTI:32, N:16>>`).
opt(Code, Value) ->
    <<Code, 0, 0, 0, Value>>.

opt(?LOPT_ACTIVE, Value, Counter) ->
    <<?LOPT_ACTIVE, 0, 0, 0, Value, Counter:16>>;
opt(Code, Value, Counter) ->
    <<Code, 0, 0, 0, Value, Counter:16>>.

ctl(P, Req, Data) -> erlang:port_control(P, ?MAGIC + Req, Data).

open_socket(Driver, Type) ->
    P = erlang:open_port({spawn_driver, Driver}, [binary]),
    [1] = ctl(P, ?REQ_OPEN, <<?AF_INET, Type, 0>>),
    P.

%% Binds to 127.0.0.1 with the kernel-assigned port 0 and reads the port back out of the reply.
bind_loopback(P) ->
    [1, Hi, Lo] = ctl(P, ?REQ_BIND, <<?AF_LOOPBACK, 0, 0>>),
    (Hi bsl 8) bor Lo.

listen(P, Backlog) ->
    [1] = ctl(P, ?REQ_LISTEN, <<Backlog:16>>),
    ok.

%% A loopback connect normally completes inside the call (a bare `[1]`); the async form answers
%% `[1] ++ <<Id:16/big>>` and the completion arrives as `{inet_async, P, Id, ok}`.
connect(P, Port) ->
    case ctl(P, ?REQ_CONNECT, <<2000:32, ?AF_INET, Port:16, 127, 0, 0, 1>>) of
        [1] ->
            ok;
        [1, Hi, Lo] ->
            Id = (Hi bsl 8) bor Lo,
            receive
                {inet_async, P, Id, ok} -> ok;
                {inet_async, P, Id, Other} -> exit({connect, Other})
            after 5000 -> exit(connect_timeout)
            end;
        Other ->
            exit({connect_reply, Other})
    end.

%% `INET_REQ_ACCEPT` is the multi-operation accept: an async `{ok, Port2}` completes it.
accept(L) ->
    [1, Hi, Lo] = ctl(L, ?REQ_ACCEPT, <<2000:32>>),
    Id = (Hi bsl 8) bor Lo,
    receive
        {inet_async, L, Id, {ok, S}} -> S;
        {inet_async, L, Id, Other} -> exit({accept, Other})
    after 5000 -> exit(accept_timeout)
    end.

setopts(P, Opts) ->
    [1] = ctl(P, ?REQ_SETOPTS, iolist_to_binary(Opts)),
    ok.

%% `prim_inet:send/3`'s own wire shape: a 16-bit-tagged caller prefix (the `term_to_binary` image
%% of a monitor reference) followed by the payload; the driver echoes the decoded tag as the fourth
%% element of `{inet_reply, S, Status, Tag}`.
send_data(S, Data) ->
    Tag = make_ref(),
    TagBin = term_to_binary(Tag, [local]),
    true = erlang:port_command(S, [<<(byte_size(TagBin)):16, TagBin/binary>>, Data]),
    receive
        {inet_reply, S, ok, Tag} -> ok;
        {inet_reply, S, Err, Tag} -> exit({send, Err})
    after 5000 -> exit(send_timeout)
    end.

send_dgram(S, DstPort, Data) ->
    Tag = make_ref(),
    TagBin = term_to_binary(Tag, [local]),
    Packet = <<?AF_INET, DstPort:16, 127, 0, 0, 1, 0:32, Data/binary>>,
    true = erlang:port_command(S, [<<(byte_size(TagBin)):16, TagBin/binary>>, Packet]),
    receive
        {inet_reply, S, ok, Tag} -> ok;
        {inet_reply, S, Err, Tag} -> exit({send, Err})
    after 5000 -> exit(send_timeout)
    end.

%% The next socket-input message (or `timeout`), i.e. what the `{active, …}` channel delivered.
wait_socket(Ms) ->
    receive
        {tcp, S, D} -> {tcp, S, D};
        {tcp_passive, S} -> {tcp_passive, S};
        {udp, S, IP, P, D} -> {udp, S, IP, P, D};
        {udp_passive, S} -> {udp_passive, S}
    after Ms -> timeout
    end.

%% The per-runtime / per-run parts are masked; everything else is the driver's own rendering.
norm({tcp, S, D}) when is_port(S) -> {tcp, '$port', D};
norm({tcp_passive, S}) when is_port(S) -> {tcp_passive, '$port'};
norm({udp, S, IP, P, D}) when is_port(S), is_integer(P) -> {udp, '$port', IP, '$source', D};
norm({udp_passive, S}) when is_port(S) -> {udp_passive, '$port'};
norm(Other) -> Other.

%% Three sockets in this very VM: `L` listens, `C` connects to it, `A` is the accepted socket.
tcp_pair() ->
    L = open_socket("tcp_inet", ?SOCK_STREAM),
    Port = bind_loopback(L),
    ok = listen(L, 5),
    C = open_socket("tcp_inet", ?SOCK_STREAM),
    ok = connect(C, Port),
    A = accept(L),
    {L, C, A}.

%% The `{mode,binary}` record is set explicitly: the driver's own effective default for a
%% `[binary]` port is a LIST (measured — `prim_inet:open/5` opens `[binary]` and the mode still
%% resolves to list), so a binary-mode case must say so.
t1() ->
    {L, C, A} = tcp_pair(),
    ok = setopts(A, [opt(?LOPT_MODE, ?MODE_BINARY), opt(?LOPT_ACTIVE, ?ACTIVE_TRUE)]),
    ok = send_data(C, <<"hello">>),
    M1 = wait_socket(5000),
    io:format("t1_first=~w~n", [norm(M1)]),
    io:format("t1_socket_is_port=~w~n", [is_port(element(2, M1))]),
    io:format("t1_first_is_hello=~w~n", [element(3, M1) =:= <<"hello">>]),
    ok = send_data(C, <<"again">>),
    M2 = wait_socket(5000),
    io:format("t1_second=~w~n", [norm(M2)]),
    io:format("t1_second_is_again=~w~n", [element(3, M2) =:= <<"again">>]),
    erlang:port_close(A), erlang:port_close(C), erlang:port_close(L),
    ok.

%% `{active, 2}`: two deliveries, exactly one `{tcp_passive, S}` at exhaustion, then silence until
%% a re-arm. Nothing is written during the passive window: the withheld-write shape (a write under a
%% spent grant, handed over by the re-arm) is `t3`'s case, and keeping `t2` free of it leaves the
%% counter's own passive message the only thing this case pins.
t2() ->
    {L, C, A} = tcp_pair(),
    ok = setopts(A, [opt(?LOPT_MODE, ?MODE_BINARY), opt(?LOPT_ACTIVE, ?ACTIVE_MULTI, 2)]),
    ok = send_data(C, <<"one">>),
    M1 = wait_socket(5000),
    ok = send_data(C, <<"two">>),
    M2 = wait_socket(5000),
    M3 = wait_socket(5000),
    io:format("t2_one=~w~n", [norm(M1)]),
    io:format("t2_two=~w~n", [norm(M2)]),
    io:format("t2_passive=~w~n", [norm(M3)]),
    io:format("t2_silent=~w~n", [norm(wait_socket(500))]),
    ok = setopts(A, [opt(?LOPT_ACTIVE, ?ACTIVE_TRUE)]),
    ok = send_data(C, <<"three">>),
    io:format("t2_rearmed=~w~n", [norm(wait_socket(5000))]),
    erlang:port_close(A), erlang:port_close(C), erlang:port_close(L),
    ok.

%% `{active, once}`: the write under the grant is delivered, a write made while the grant is spent is
%% withheld in the socket, the re-arm hands THAT write over (`bbbb`, never the fresh `cccc` written
%% after it) and spends its own grant on it, and the next re-arm delivers `cccc`.
t3() ->
    {L, C, A} = tcp_pair(),
    ok = setopts(A, [opt(?LOPT_MODE, ?MODE_BINARY), opt(?LOPT_ACTIVE, ?ACTIVE_ONCE)]),
    ok = send_data(C, <<"hello">>),
    M1 = wait_socket(5000),
    io:format("t3_first=~w~n", [norm(M1)]),
    ok = send_data(C, <<"bbbb">>),
    io:format("t3_silent=~w~n", [norm(wait_socket(500))]),
    ok = setopts(A, [opt(?LOPT_ACTIVE, ?ACTIVE_ONCE)]),
    ok = send_data(C, <<"cccc">>),
    M3 = wait_socket(5000),
    io:format("t3_second=~w~n", [norm(M3)]),
    io:format("t3_silent_again=~w~n", [norm(wait_socket(500))]),
    ok = setopts(A, [opt(?LOPT_ACTIVE, ?ACTIVE_ONCE)]),
    io:format("t3_third=~w~n", [norm(wait_socket(5000))]),
    erlang:port_close(A), erlang:port_close(C), erlang:port_close(L),
    ok.

%% The same payload under `{mode, binary}` (the default the fixture asks for explicitly) and
%% `{mode, list}`: a binary versus a charlist.
t4() ->
    {L1, C1, A1} = tcp_pair(),
    ok = setopts(A1, [opt(?LOPT_MODE, ?MODE_BINARY), opt(?LOPT_ACTIVE, ?ACTIVE_ONCE)]),
    ok = send_data(C1, <<"hello">>),
    B = wait_socket(5000),
    io:format("t4_binary=~w~n", [norm(B)]),
    io:format("t4_binary_is_binary=~w~n", [is_binary(element(3, B))]),
    erlang:port_close(A1), erlang:port_close(C1), erlang:port_close(L1),
    {L2, C2, A2} = tcp_pair(),
    ok = setopts(A2, [opt(?LOPT_MODE, ?MODE_LIST), opt(?LOPT_ACTIVE, ?ACTIVE_ONCE)]),
    ok = send_data(C2, <<"hello">>),
    Lt = wait_socket(5000),
    io:format("t4_list=~w~n", [norm(Lt)]),
    io:format("t4_list_is_list=~w~n", [is_list(element(3, Lt))]),
    io:format("t4_list_is_hello=~w~n", [element(3, Lt) =:= "hello"]),
    erlang:port_close(A2), erlang:port_close(C2), erlang:port_close(L2),
    ok.

%% `{packet, 2}`: the peer writes its own 16-bit length header and the owner receives the body
%% alone.
t5() ->
    {L, C, A} = tcp_pair(),
    ok = setopts(A, [opt(?LOPT_PACKET, ?TCP_PB_2),
                     opt(?LOPT_MODE, ?MODE_BINARY),
                     opt(?LOPT_ACTIVE, ?ACTIVE_ONCE)]),
    ok = send_data(C, <<5:16, "hello">>),
    M = wait_socket(5000),
    io:format("t5_stripped=~w~n", [norm(M)]),
    io:format("t5_body_is_hello=~w~n", [element(3, M) =:= <<"hello">>]),
    erlang:port_close(A), erlang:port_close(C), erlang:port_close(L),
    ok.

%% One datagram: `{udp, S, {127,0,0,1}, SourcePort, Data}`, where the source port is the peer's own
%% real bound port (masked as `'$source'` for the comparison, asserted here as a real integer).
t6() ->
    A = open_socket("udp_inet", ?SOCK_DGRAM),
    APort = bind_loopback(A),
    C = open_socket("udp_inet", ?SOCK_DGRAM),
    CPort = bind_loopback(C),
    ok = setopts(A, [opt(?LOPT_MODE, ?MODE_BINARY), opt(?LOPT_ACTIVE, ?ACTIVE_TRUE)]),
    ok = send_dgram(C, APort, <<"hi">>),
    M = wait_socket(5000),
    io:format("t6_datagram=~w~n", [norm(M)]),
    io:format("t6_source_is_peer=~w~n", [element(4, M) =:= CPort]),
    io:format("t6_source_is_real=~w~n", [is_integer(element(4, M)) andalso element(4, M) > 0]),
    io:format("t6_address=~w~n", [element(3, M)]),
    erlang:port_close(A), erlang:port_close(C),
    ok.

%% An accepted socket inherits its listener's option block (`{packet, 2}` and `{mode, list}`, but
%% **not** `{active, …}` — the C copies everything except the active grant), so the accepted
%% port's own re-armed `{active, 2}` delivers charlists with the framing header stripped and its
%% `{tcp_passive, S}` arrives at the listener's counter.
t7() ->
    L = open_socket("tcp_inet", ?SOCK_STREAM),
    Port = bind_loopback(L),
    ok = listen(L, 5),
    ok = setopts(L, [opt(?LOPT_PACKET, ?TCP_PB_2),
                     opt(?LOPT_MODE, ?MODE_LIST),
                     opt(?LOPT_ACTIVE, ?ACTIVE_MULTI, 2)]),
    C = open_socket("tcp_inet", ?SOCK_STREAM),
    ok = connect(C, Port),
    A = accept(L),
    io:format("t7_accepted_is_port=~w~n", [is_port(A)]),
    ok = setopts(A, [opt(?LOPT_ACTIVE, ?ACTIVE_MULTI, 2)]),
    ok = send_data(C, <<5:16, "hello">>),
    M1 = wait_socket(5000),
    ok = send_data(C, <<5:16, "world">>),
    M2 = wait_socket(5000),
    M3 = wait_socket(5000),
    io:format("t7_first=~w~n", [norm(M1)]),
    io:format("t7_first_is_charlist=~w~n", [element(3, M1) =:= "hello"]),
    io:format("t7_second=~w~n", [norm(M2)]),
    io:format("t7_second_is_world=~w~n", [element(3, M2) =:= "world"]),
    io:format("t7_passive=~w~n", [norm(M3)]),
    erlang:port_close(A), erlang:port_close(C), erlang:port_close(L),
    ok.
