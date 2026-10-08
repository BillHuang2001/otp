#!/usr/bin/env escript
%%! -noshell
%%% The `beam_loader` literal-pool ORACLE for `tests/literals.rs`.
%%%
%%% Usage:  escript literals_oracle.escript <otp-lib-root>
%%%
%%% Walks every `*.beam` under <otp-lib-root> (like the Rust sweep in
%%% `tests/real_modules.rs`), parses each `LitT` chunk with OTP's own `beam_lib` +
%%% `zlib` (an implementation independent of `beam_loader`'s framing), and prints
%%% one TAB-separated line per literal:
%%%
%%%     <path relative to the root> <TAB> <literal index> <TAB> <canonical hex>
%%%
%%% where the canonical form is `term_to_binary(binary_to_term(Payload))` -- OTP's
%%% own decode/encode round-trip. `binary_to_term/1` normalizes a flat map's key
%%% order to ERTS's canonical order, so the canonical image may differ from the raw
%%% payload bytes for a map-containing literal while denoting the same term; the
%%% Rust side therefore compares terms SEMANTICALLY (via `beam_term::compare`),
%%% which is insensitive to that representational freedom.
%%%
%%% A payload OTP cannot decode crashes the script (non-zero exit), which the Rust
%%% test reports as an oracle failure -- never as a silent pass.
main([Root0]) ->
    Root = string:trim(Root0, trailing, "/"),
    Files = lists:sort(collect(Root)),
    lists:foreach(fun(File) -> emit(File, Root) end, Files).

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
    {ok, _Module, Sections} = beam_lib:all_chunks(File),
    case lists:keyfind("LitT", 1, Sections) of
        false ->
            ok;
        {_, Chunk} ->
            <<Uncompressed:32, Rest/binary>> = Chunk,
            Table =
                case Uncompressed of
                    0 -> Rest;
                    _ -> zlib:uncompress(Rest)
                end,
            <<Count:32/signed, Records/binary>> = Table,
            records(Records, Count, relative(File, Root), 0)
    end.

records(_Rest, Count, _Rel, _Index) when Count =< 0 ->
    ok;
records(<<Size:32/signed, Payload:Size/binary, Rest/binary>>, Count, Rel, Index) ->
    Canonical = term_to_binary(binary_to_term(Payload)),
    io:format("~s\t~p\t~s~n", [Rel, Index, hex(Canonical)]),
    records(Rest, Count - 1, Rel, Index + 1).

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
