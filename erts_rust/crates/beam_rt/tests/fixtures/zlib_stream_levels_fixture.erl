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
%% The `zlib` stream-API fixture for `tests/zlib_stream_levels.rs`.
%%
%% Compiled at test time by the system `erlc` into a scratch directory; never committed as a
%% `.beam` (the source is the single artifact). Every function prints its result with
%% `io:format/2` and returns `ok`, so the observable is the printed transcript — never the
%% value the CLI's `~w` result printer would render (a binary renders as `<<55,56,…>>` on the
%% scheduled `run` route, a pre-existing printer difference unrelated to `zlib`). No binary is
%% ever printed: sizes are integers, digests are `binary:encode_hex/1` charlists.
-module(zlib_stream_levels_fixture).

-export([multi_sync/0, flush_modes/0, init_forms/0, reset_mid_stream/0, deflate_params/0,
         round_trip/0]).

%% The fixture payload, byte-identical to the one `tests/zlib_stream_levels.rs` asserts the
%% md5 of (1024 bytes, `A1D3CA40959CD47D539D2A0115E99AF2`).
payload() ->
    Source = <<"It was the best of times, it was the worst of times, it was the age of wisdom, "
               "it was the age of foolishness, it was the epoch of belief, it was the epoch of "
               "incredulity.\n">>,
    binary:part(binary:copy(Source, 6), 0, 1024).

%% The payload in four 256-byte parts: the transcript's input chunks.
parts() ->
    F1 = payload(),
    [binary:part(F1, Offset, 256) || Offset <- [0, 256, 512, 768]].

%% A digest as an uppercase hex charlist (never a binary, see the module docs).
hex(Bin) ->
    binary_to_list(binary:encode_hex(erlang:md5(Bin))).

%% One deflate stream: four `sync` parts, then `finish` — the transcript's byte-count vector
%% (five entries), the total and the md5 of the concatenation.
transcript(Level) ->
    Z = zlib:open(),
    ok = zlib:deflateInit(Z, Level),
    Sync = [iolist_to_binary(zlib:deflate(Z, Part, sync)) || Part <- parts()],
    Last = iolist_to_binary(zlib:deflate(Z, [], finish)),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    Outs = Sync ++ [Last],
    {[byte_size(Out) || Out <- Outs], iolist_to_binary(Outs)}.

%% The multi-`sync` transcript, printed once per compression level 0..9.
multi_sync() ->
    F1 = payload(),
    io:format("payload length ~w md5 ~s~n", [byte_size(F1), hex(F1)]),
    lists:foreach(
      fun(Level) ->
              {Counts, All} = transcript(Level),
              io:format("level ~w counts ~w total ~w md5 ~s~n",
                        [Level, Counts, byte_size(All), hex(All)])
      end, lists:seq(0, 9)),
    ok.

%% One flush row: the whole payload deflated in ONE call with `Flush` (plus the closing
%% `finish` call, which a `finish` row does not need a second of).
run_flush(Flush) ->
    Z = zlib:open(),
    ok = zlib:deflateInit(Z, 6),
    Outs = case Flush of
               finish ->
                   [iolist_to_binary(zlib:deflate(Z, payload(), finish))];
               _ ->
                   [iolist_to_binary(zlib:deflate(Z, payload(), Flush)),
                    iolist_to_binary(zlib:deflate(Z, [], finish))]
           end,
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    All = iolist_to_binary(Outs),
    io:format("flush ~w counts ~w total ~w md5 ~s~n",
              [Flush, [byte_size(Out) || Out <- Outs], byte_size(All), hex(All)]).

%% `deflate/2` (no flush argument — the wrapper's `none` default) and `deflate/3` with each of
%% the four flush values.
flush_modes() ->
    Z = zlib:open(),
    ok = zlib:deflateInit(Z, 6),
    D1 = iolist_to_binary(zlib:deflate(Z, payload())),
    D2 = iolist_to_binary(zlib:deflate(Z, [], finish)),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    All = iolist_to_binary([D1, D2]),
    io:format("flush deflate_2 counts ~w total ~w md5 ~s~n",
              [[byte_size(D1), byte_size(D2)], byte_size(All), hex(All)]),
    lists:foreach(fun run_flush/1, [none, sync, full, finish]),
    ok.

%% One one-shot compression under an arbitrary `deflateInit` form: the compressed size and
%% md5, plus the round-trip witness (the recovered bytes are the payload again).
%%
%% `Bits` is the window-bits value the matching `inflateInit/2` needs: a **negative** deflate
%% window bits suppresses the zlib header (a raw deflate stream), which only a raw inflate
%% stream can read back.
one_shot(Label, Bits, Init) ->
    Z = zlib:open(),
    ok = Init(Z),
    Compressed = iolist_to_binary([zlib:deflate(Z, payload(), none),
                                   zlib:deflate(Z, [], finish)]),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    I = zlib:open(),
    ok = zlib:inflateInit(I, Bits),
    Recovered = iolist_to_binary(zlib:inflate(I, Compressed)),
    ok = zlib:inflateEnd(I),
    ok = zlib:close(I),
    io:format("form ~w compressed ~w md5 ~s recovered ~w equal ~w~n",
              [Label, byte_size(Compressed), hex(Compressed), byte_size(Recovered),
               Recovered =:= payload()]).

%% `deflateInit/1`, `deflateInit/2` (the atom and integer levels `zlib.erl` accepts) and
%% `deflateInit/6` (positive and negative window bits, memlevels 1..9, every strategy).
%% `zlib.erl` has no `{level, L}` tuple form: `arg_level/1` takes `default`/`none`/
%% `best_speed`/`best_compression` or an integer 0..9, and `/2` forwards to `/6` with
%% `deflated`, 15, 8, `default`.
init_forms() ->
    Forms =
        [{deflateInit_1, 15, fun(Z) -> zlib:deflateInit(Z) end},
         {deflateInit_2_default, 15, fun(Z) -> zlib:deflateInit(Z, default) end},
         {deflateInit_2_none, 15, fun(Z) -> zlib:deflateInit(Z, none) end},
         {deflateInit_2_best_speed, 15, fun(Z) -> zlib:deflateInit(Z, best_speed) end},
         {deflateInit_2_best_compression, 15,
          fun(Z) -> zlib:deflateInit(Z, best_compression) end},
         {deflateInit_2_integer_3, 15, fun(Z) -> zlib:deflateInit(Z, 3) end},
         {deflateInit_6_default, 15,
          fun(Z) -> zlib:deflateInit(Z, default, deflated, 15, 8, default) end},
         {deflateInit_6_no_header, -15,
          fun(Z) -> zlib:deflateInit(Z, default, deflated, -15, 8, default) end},
         {deflateInit_6_window_9_mem_1_filtered, 9,
          fun(Z) -> zlib:deflateInit(Z, best_compression, deflated, 9, 1, filtered) end},
         {deflateInit_6_window_12_mem_9_huffman_only, 12,
          fun(Z) -> zlib:deflateInit(Z, best_speed, deflated, 12, 9, huffman_only) end},
         {deflateInit_6_window_10_mem_5_rle, 10,
          fun(Z) -> zlib:deflateInit(Z, 4, deflated, 10, 5, rle) end}],
    lists:foreach(fun({Label, Bits, Init}) -> one_shot(Label, Bits, Init) end, Forms),
    ok.

%% A stream that has already compressed two parts, `deflateReset`-ed, then driven through the
%% full transcript, must produce exactly what a brand-new stream produces.
reset_mid_stream() ->
    [P1, P2 | _] = parts(),
    Fresh = zlib:open(),
    ok = zlib:deflateInit(Fresh, 6),
    {FreshCounts, FreshAll} = transcript_on(Fresh),
    Reused = zlib:open(),
    ok = zlib:deflateInit(Reused, 6),
    _ = iolist_to_binary(zlib:deflate(Reused, P1, sync)),
    _ = iolist_to_binary(zlib:deflate(Reused, P2, none)),
    ok = zlib:deflateReset(Reused),
    {ResetCounts, ResetAll} = transcript_on(Reused),
    io:format("reset fresh counts ~w total ~w md5 ~s~n",
              [FreshCounts, byte_size(FreshAll), hex(FreshAll)]),
    io:format("reset after_reset counts ~w total ~w md5 ~s~n",
              [ResetCounts, byte_size(ResetAll), hex(ResetAll)]),
    io:format("reset identical ~w~n", [FreshAll =:= ResetAll]),
    ok.

%% The four-`sync`-then-`finish` transcript on an already-initialised stream (which this
%% closes).
transcript_on(Z) ->
    Sync = [iolist_to_binary(zlib:deflate(Z, Part, sync)) || Part <- parts()],
    Last = iolist_to_binary(zlib:deflate(Z, [], finish)),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    Outs = Sync ++ [Last],
    {[byte_size(Out) || Out <- Outs], iolist_to_binary(Outs)}.

%% `deflateParams/3` mid-stream: `none` -> `best_compression`/`default` ->
%% `1`/`filtered`, then the four `sync` parts and `finish`. `zlib:deflateParams/3` itself
%% flushes with `sync` and re-injects the pending output (`save_progress/3`), so the counts
%% move if those bytes are dropped or duplicated; the concatenation must still inflate back to
%% the payload.
deflate_params() ->
    [P1, P2, P3, P4] = parts(),
    Z = zlib:open(),
    ok = zlib:deflateInit(Z, none),
    A = iolist_to_binary(zlib:deflate(Z, P1, sync)),
    ok = zlib:deflateParams(Z, best_compression, default),
    B = iolist_to_binary(zlib:deflate(Z, P2, sync)),
    ok = zlib:deflateParams(Z, 1, filtered),
    C = iolist_to_binary(zlib:deflate(Z, P3, sync)),
    D = iolist_to_binary(zlib:deflate(Z, P4, sync)),
    E = iolist_to_binary(zlib:deflate(Z, [], finish)),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    Outs = [A, B, C, D, E],
    All = iolist_to_binary(Outs),
    io:format("params counts ~w total ~w md5 ~s~n",
              [[byte_size(Out) || Out <- Outs], byte_size(All), hex(All)]),
    I = zlib:open(),
    ok = zlib:inflateInit(I),
    Recovered = iolist_to_binary(zlib:inflate(I, All)),
    ok = zlib:inflateEnd(I),
    ok = zlib:close(I),
    io:format("params recovered ~w equal ~w~n",
              [byte_size(Recovered), Recovered =:= payload()]).

%% A deflate -> inflate round trip, in one `inflate/2` call and in three chunks, recovers the
%% payload byte for byte.
round_trip() ->
    F1 = payload(),
    Z = zlib:open(),
    ok = zlib:deflateInit(Z, best_compression),
    Compressed = iolist_to_binary([zlib:deflate(Z, F1, none),
                                   zlib:deflate(Z, [], finish)]),
    ok = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    I1 = zlib:open(),
    ok = zlib:inflateInit(I1),
    Whole = iolist_to_binary(zlib:inflate(I1, Compressed)),
    ok = zlib:inflateEnd(I1),
    ok = zlib:close(I1),
    Chunk = (byte_size(Compressed) + 2) div 3,
    I2 = zlib:open(),
    ok = zlib:inflateInit(I2),
    Chunked = iolist_to_binary(
                [zlib:inflate(I2, binary:part(Compressed, Offset,
                                              min(Chunk, byte_size(Compressed) - Offset)))
                 || Offset <- chunk_offsets(byte_size(Compressed), Chunk)]),
    ok = zlib:inflateEnd(I2),
    ok = zlib:close(I2),
    io:format("roundtrip compressed ~w compressed_md5 ~s~n", [byte_size(Compressed), hex(Compressed)]),
    io:format("roundtrip whole ~w whole_md5 ~s whole_equal ~w~n",
              [byte_size(Whole), hex(Whole), Whole =:= F1]),
    io:format("roundtrip chunked ~w chunked_md5 ~s chunked_equal ~w~n",
              [byte_size(Chunked), hex(Chunked), Chunked =:= F1]),
    ok.

%% The offsets of the `Chunk`-byte slices of a `Size`-byte binary.
chunk_offsets(Size, Chunk) ->
    chunk_offsets(Size, Chunk, 0).

chunk_offsets(Size, _Chunk, Offset) when Offset >= Size ->
    [];
chunk_offsets(Size, Chunk, Offset) ->
    [Offset | chunk_offsets(Size, Chunk, Offset + Chunk)].
