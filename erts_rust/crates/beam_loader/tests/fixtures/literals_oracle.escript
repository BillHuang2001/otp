#!/usr/bin/env escript
%%! -noshell
%%% The `beam_loader` literal-pool ORACLE for `tests/literals.rs`.
%%%
%%% Usage:  escript literals_oracle.escript <otp-lib-root>
%%%         escript literals_oracle.escript --reencode <beam-file>
%%%
%%% `<otp-lib-root>` mode walks every `*.beam` under <otp-lib-root> (like the Rust
%%% sweep in `tests/real_modules.rs`), parses each `LitT` chunk with OTP's own
%%% `beam_lib` + `zlib` (an implementation independent of `beam_loader`'s framing),
%%% and prints one TAB-separated line per literal:
%%%
%%%     <path relative to the root> <TAB> <literal index> <TAB> <canonical hex>
%%%
%%% `--reencode <beam-file>` mode does exactly the same for the single named `.beam`
%%% file, printing one line per literal, keyed by index only:
%%%
%%%     <literal index> <TAB> <canonical hex>
%%%
%%% where the canonical form is `term_to_binary(binary_to_term(Payload))` -- OTP's
%%% own decode/encode round-trip. `binary_to_term/1` normalizes a flat map's key
%%% order to ERTS's canonical (`erts_cmp_flatmap_keys`) order, so the canonical image
%%% may differ from the raw payload bytes for a map-containing literal while denoting
%%% the same term; the Rust side therefore compares such literals against the
%%% canonical image, and compares terms SEMANTICALLY (via `beam_term::compare`) in
%%% the install sweep, which is insensitive to that representational freedom.
%%%
%%% A payload OTP cannot decode crashes the script (non-zero exit), which the Rust
%%% test reports as an oracle failure -- never as a silent pass.
main(["--reencode", File]) ->
    lists:foreach(fun({Index, Payload}) ->
                          io:format("~p\t~s~n", [Index, hex(canonical(Payload))])
                  end,
                  literals(File));
main([Root0]) when is_list(Root0), Root0 =/= "--reencode" ->
    Root = string:trim(Root0, trailing, "/"),
    Files = lists:sort(collect(Root)),
    lists:foreach(fun(File) -> emit(File, Root) end, Files);
main(_Args) ->
    io:format(standard_error,
              "usage: ~s <otp-lib-root> | --reencode <beam-file>~n",
              [escript:script_name()]),
    halt(2).

collect(Dir) ->
    case filelib:is_dir(Dir) of
        true ->
            lists:append([collect(Child) || Child <- filelib:wildcard(filename:join(Dir, "*"))]);
        false ->
            case filename:extension(Dir) of
                ".beam" -> [Dir];
                _ -> []
            end
    end.

emit(File, Root) ->
    Rel = relative(File, Root),
    lists:foreach(fun({Index, Payload}) ->
                          io:format("~s\t~p\t~s~n", [Rel, Index, hex(canonical(Payload))])
                  end,
                  literals(File)).

%% The raw `LitT` payloads (ETF bytes) of one `.beam` file, in `Operand::Literal(idx)`
%% index order: `{Index, Payload}` pairs. A module without a `LitT` chunk yields `[]`.
literals(File) ->
    {ok, _Module, Sections} = beam_lib:all_chunks(File),
    case lists:keyfind("LitT", 1, Sections) of
        false -> [];
        {_, Chunk} -> payloads(Chunk)
    end.

payloads(Chunk) ->
    <<Uncompressed:32, Rest/binary>> = Chunk,
    Table =
        case Uncompressed of
            0 -> Rest;
            _ -> zlib:uncompress(Rest)
        end,
    <<Count:32/signed, Records/binary>> = Table,
    payloads(Records, Count, 0, []).

payloads(_Rest, Count, _Index, Acc) when Count =< 0 ->
    lists:reverse(Acc);
payloads(<<Size:32/signed, Payload:Size/binary, Rest/binary>>, Count, Index, Acc) ->
    payloads(Rest, Count - 1, Index + 1, [{Index, Payload} | Acc]).

%% OTP's own decode/encode round-trip of one payload.
canonical(Payload) ->
    term_to_binary(binary_to_term(Payload)).

relative(File, Root) ->
    Prefix = Root ++ "/",
    case lists:prefix(Prefix, File) of
        true -> lists:nthtail(length(Prefix), File);
        false -> File
    end.

hex(Bin) ->
    <<<<(nibble(H)), (nibble(L))>> || <<H:4, L:4>> <= Bin>>.

nibble(N) when N < 10 -> $0 + N;
nibble(N) -> $a + N - 10.
