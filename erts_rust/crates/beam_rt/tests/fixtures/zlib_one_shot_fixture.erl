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
%% The one-shot (`zlib:compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`)
%% fixture for `tests/zlib_one_shot.rs`.
%%
%% Compiled at test time by the system `erlc` into a scratch directory; never committed as a
%% `.beam` (the source is the single artifact). Every function prints its transcript with
%% `io:format/2` and returns `ok`, so the observable is the printed lines — never the value the
%% CLI's `~w` result printer would render (a binary renders as `<<55,56,…>>` on the scheduled
%% `run` route, a pre-existing printer difference unrelated to `zlib`). No binary is ever
%% printed: sizes are integers, digests/hex images are `binary:encode_hex/1` charlists.
%%
%% The payload table is **byte-identical** to the one the `-eval` expressions of
%% `tests/zlib_one_shot.rs` build (`PAYLOADS_ERL`): the transcript's own
%% `payload <label> bytes <N> md5 <HEX>` lines are what pins the two constructions to the same
%% bytes, because the `run` route's transcript and the `-eval` route's transcript are compared
%% against each other and against the oracle's answer for both.
-module(zlib_one_shot_fixture).

-export([goldens/0, fast_paths/0, wrapper/0, libz/0, concat/0]).

%% A digest as an uppercase hex charlist (never a binary, see the module docs).
hex(Bin) ->
    binary_to_list(binary:encode_hex(erlang:md5(Bin))).

%% A raw hex image as an uppercase hex charlist.
image(Bin) ->
    binary_to_list(binary:encode_hex(Bin)).

%% `F1`: 1 024 bytes, md5 `A1D3CA40959CD47D539D2A0115E99AF2` — the payload
%% `common_test/test_server_node`'s `compressed_literals` compresses.
f1() ->
    Source = <<"It was the best of times, it was the worst of times, it was the age of wisdom, "
               "it was the age of foolishness, it was the epoch of belief, it was the epoch of "
               "incredulity.\n">>,
    binary:part(binary:copy(Source, 6), 0, 1024).

%% The six payloads every surface is driven over, in the suite's order.
payloads() ->
    [empty, one, f1, repetitive, all_bytes, mixed].

%% The payload behind each label; identical to `PAYLOADS_ERL` in `tests/zlib_one_shot.rs`.
payload(empty) ->
    <<>>;
payload(one) ->
    <<"a">>;
payload(f1) ->
    f1();
payload(repetitive) ->
    binary:copy(<<"0123456789abcdef">>, 6250);
payload(all_bytes) ->
    list_to_binary(lists:seq(0, 255));
payload(mixed) ->
    iolist_to_binary([[erlang:md5(integer_to_binary(I)), binary:copy(<<"ab">>, 8)]
                      || I <- lists:seq(1, 32)]).

%% The three named goldens, as hex images (the whole compressed value, not a digest).
goldens() ->
    io:format("zip hello_hello_hello ~s~n", [image(zlib:zip(<<"hello hello hello">>))]),
    io:format("compress hello_hello_hello ~s~n",
              [image(zlib:compress(<<"hello hello hello">>))]),
    io:format("gzip hello ~s~n", [image(zlib:gzip(<<"hello">>))]),
    ok.

%% The three compressors' bytes and the three round trips, one payload at a time.
fast_paths() ->
    lists:foreach(
      fun(Label) ->
              Data = payload(Label),
              Compressed = zlib:compress(Data),
              Zipped = zlib:zip(Data),
              Gzipped = zlib:gzip(Data),
              io:format("payload ~w bytes ~w md5 ~s~n", [Label, byte_size(Data), hex(Data)]),
              io:format("~w compress ~w ~s zip ~w ~s gzip ~w ~s~n",
                        [Label, byte_size(Compressed), hex(Compressed),
                         byte_size(Zipped), hex(Zipped), byte_size(Gzipped), hex(Gzipped)]),
              Uncompressed = zlib:uncompress(Compressed),
              Unzipped = zlib:unzip(Zipped),
              Ungzipped = zlib:gunzip(Gzipped),
              io:format("~w roundtrip uncompress ~w ~s ~w unzip ~w ~s ~w gunzip ~w ~s ~w~n",
                        [Label,
                         byte_size(Uncompressed), hex(Uncompressed), Uncompressed =:= Data,
                         byte_size(Unzipped), hex(Unzipped), Unzipped =:= Data,
                         byte_size(Ungzipped), hex(Ungzipped), Ungzipped =:= Data])
      end, payloads()),
    ok.

%% One deflate stream over `Data`: `Init(Z)`, then `deflate/3` with `finish`, `deflateEnd/1`
%% and `close/1` — the transcript the one-shot functions are documented wrappers of
%% (`zlib.erl`), driven through the public stream API instead of the fast path.
deflate_with(Init, Data) ->
    Z = zlib:open(),
    ok = Init(Z),
    Out = iolist_to_binary(zlib:deflate(Z, Data, finish)),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    Out.

%% The `deflateInit` shape each one-shot function is a wrapper of (`zlib.erl`):
%% `compress/1` is `deflateInit(Z, default)` (window bits 15 — the RFC 1950 framing), `zip/1`
%% is `-15` (RFC 1951, raw) and `gzip/1` is `16 + 15 = 31` (RFC 1952), all at memLevel 8 and
%% the `default` strategy.
wrapper_init(compress) ->
    fun(Z) -> zlib:deflateInit(Z, default) end;
wrapper_init(zip) ->
    fun(Z) -> zlib:deflateInit(Z, default, deflated, -15, 8, default) end;
wrapper_init(gzip) ->
    fun(Z) -> zlib:deflateInit(Z, default, deflated, 31, 8, default) end.

%% The one-shot function each framing is compared against.
one_shot(compress, Data) ->
    zlib:compress(Data);
one_shot(zip, Data) ->
    zlib:zip(Data);
one_shot(gzip, Data) ->
    zlib:gzip(Data).

%% The wrapper-vs-fast-path byte comparison, one framing per payload.
wrapper() ->
    lists:foreach(
      fun(Framing) ->
              lists:foreach(
                fun(Label) ->
                        Data = payload(Label),
                        Wrapped = deflate_with(wrapper_init(Framing), Data),
                        OneShot = one_shot(Framing, Data),
                        io:format("wrapper ~w ~w bytes ~w md5 ~s one_shot ~w md5 ~s same ~w~n",
                                  [Framing, Label,
                                   byte_size(Wrapped), hex(Wrapped),
                                   byte_size(OneShot), hex(OneShot), Wrapped =:= OneShot])
                end, payloads())
      end, [compress, zip, gzip]),
    ok.

%% A.9: `zlib:compress/1`'s **whole** compressed image for the five payloads the property is
%% pinned on — the bytes `common_test/test_server_node`'s `compressed_literals` embeds.
libz() ->
    lists:foreach(
      fun(Label) ->
              Data = payload(Label),
              Compressed = zlib:compress(Data),
              io:format("libz ~w source ~w ~s compressed ~w ~s image ~s~n",
                        [Label, byte_size(Data), hex(Data),
                         byte_size(Compressed), hex(Compressed), image(Compressed)])
      end, [empty, one, f1, repetitive, mixed]),
    ok.

%% `gunzip/1` over **concatenated gzip members**: each member is its own stream, and the
%% recovered value is the members' payloads concatenated (`zlib.erl`'s `inflateInit(Z, 31,
%% reset)` — the `reset` option is what keeps reading past a member's trailer).
concat() ->
    Parts = [<<"first member ">>, <<"second member">>, f1()],
    Members = [zlib:gzip(Part) || Part <- Parts],
    Joined = iolist_to_binary(Members),
    Recovered = zlib:gunzip(Joined),
    Whole = iolist_to_binary(Parts),
    io:format("concat members ~w joined ~w recovered ~w md5 ~s equal ~w~n",
              [length(Members), byte_size(Joined), byte_size(Recovered), hex(Recovered),
               Recovered =:= Whole]),
    Single = zlib:gunzip(hd(Members)),
    io:format("concat single member ~w recovered ~w equal ~w~n",
              [byte_size(hd(Members)), byte_size(Single), Single =:= hd(Parts)]),
    ok.
