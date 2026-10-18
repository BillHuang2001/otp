#!/usr/bin/env escript
%%! -noshell
main(_) ->
    M = 16#03f1a300,
    P = open_port({spawn_driver, "tcp_inet"}, [binary]),
    F = fun(C, A) ->
            binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A)))
        end,
    {ok, L} = gen_tcp:listen(0, [{ip, {127,0,0,1}}, binary, {active, false}]),
    {ok, LP} = inet:port(L),
    io:format("listen=~p~n", [LP]),
    io:format("open=~p~n", [F(1, <<1,1,0>>)]),
    Conn = F(3, <<-1:32, 4, LP:16>>),
    io:format("connect=~p~n", [Conn]),
    %% drain the connect completion
    Drained = drain(),
    io:format("connect_msgs=~p~n", [Drained]),
    {ok, S} = gen_tcp:accept(L),
    R1 = F(42, <<-1:32, 0:32>>),
    io:format("recv1=~p~n", [R1]),
    R2 = F(42, <<-1:32, 0:32>>),
    io:format("recv2=~p~n", [R2]),
    R3 = F(42, <<-1:32>>),
    io:format("recv_short=~p~n", [R3]),
    R4 = F(42, <<-1:32, 16#04000001:32>>),
    io:format("recv_huge=~p~n", [R4]),
    Msgs0 = drain(),
    io:format("msgs_before_data=~p~n", [Msgs0]),
    ok = gen_tcp:send(S, <<"hello">>),
    timer:sleep(200),
    Msgs1 = drain(),
    io:format("msgs_after_data=~p~n", [Msgs1]),
    %% listener: recv on a non-connected port
    P2 = open_port({spawn_driver, "tcp_inet"}, [binary]),
    G = fun(C, A) ->
            binary_to_list(iolist_to_binary(erlang:port_control(P2, C + M, A)))
        end,
    _ = G(1, <<1,1,0>>),
    io:format("l_bind=~p~n", [G(6, <<4,0,0>>)]),
    io:format("l_listen=~p~n", [G(27, <<0,5>>)]),
    io:format("listener_recv=~p~n", [G(42, <<-1:32,0:32>>)]),
    io:format("listener_recv_short=~p~n", [G(42, <<-1:32>>)]),
    halt(0).

drain() ->
    receive
        M -> [M | drain()]
    after 300 -> []
    end.
