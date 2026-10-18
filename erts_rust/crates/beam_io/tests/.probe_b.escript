#!/usr/bin/env escript
%%! -noshell
main(_) ->
    M = 16#03f1a300,
    P = open_port({spawn_driver, "tcp_inet"}, [binary]),
    F = fun(C, A) ->
            binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A)))
        end,
    _ = F(1, <<1,1,0>>),
    [_, BH, BL] = F(6, <<4,0,0>>),
    Bound = BH * 256 + BL,
    _ = F(27, <<0,5>>),
    Acc = F(26, <<-1:32>>),
    io:format("accept=~p~n", [Acc]),
    {ok, Peer} = gen_tcp:connect({127,0,0,1}, Bound, [binary, {active,false}]),
    AccMsg = drain_some(),
    io:format("accept_msgs=~p~n", [AccMsg]),
    P2 = case AccMsg of
             [{inet_async, _, _, {ok, PP}} | _] -> PP;
             _ -> undefined
         end,
    io:format("accepted_port=~p~n", [P2]),
    G = fun(C, A) ->
            binary_to_list(iolist_to_binary(erlang:port_control(P2, C + M, A)))
        end,
    io:format("acc_status=~p~n", [G(16, <<>>)]),
    R1 = G(42, <<-1:32,0:32>>),
    io:format("acc_recv1=~p~n", [R1]),
    ok = gen_tcp:send(Peer, <<"world">>),
    timer:sleep(200),
    io:format("acc_msgs=~p~n", [drain_some()]),
    R2 = G(42, <<-1:32,0:32>>),
    io:format("acc_recv2=~p~n", [R2]),
    %% ---- caller tag round trip on the accepted port ----
    Tag = term_to_binary(make_ref(), [local]),
    io:format("tag_size=~p~n", [byte_size(Tag)]),
    io:format("tag=~p~n", [binary_to_list(Tag)]),
    Cmd = <<(byte_size(Tag)):16, Tag/binary, "hello">>,
    io:format("command=~p~n", [erlang:port_command(P2, Cmd)]),
    timer:sleep(200),
    Peek = case gen_tcp:recv(Peer, 0, 1000) of
               {ok, Data} -> binary_to_list(Data);
               Other -> Other
           end,
    io:format("peer_got=~p~n", [Peek]),
    io:format("cmd_msgs=~p~n", [drain_some()]),
    io:format("zero_command=~p~n", [erlang:port_command(P2, <<0:16, "raw">>)]),
    timer:sleep(200),
    Peek2 = case gen_tcp:recv(Peer, 0, 1000) of
                {ok, Data2} -> binary_to_list(Data2);
                Other2 -> Other2
            end,
    io:format("peer_got_zero=~p~n", [Peek2]),
    io:format("zero_msgs=~p~n", [drain_some()]),
    %% ---- close with a pending receive ----
    R3 = G(42, <<-1:32,0:32>>),
    io:format("pre_close_recv=~p~n", [R3]),
    port_close(P2),
    timer:sleep(200),
    io:format("close_msgs=~p~n", [drain_some()]),
    timer:sleep(200),
    io:format("close_msgs2=~p~n", [drain_some()]),
    halt(0).

drain_some() ->
    receive
        M -> [M | drain_some()]
    after 300 -> []
    end.
