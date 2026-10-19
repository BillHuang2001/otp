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
%% The `zlib` stream-API fixture for `tests/zlib_stream_dict.rs`, driven through the
%% runtime's **scheduled `run` route** (`beam_rt run zlib_stream.beam zlib_stream:<f>/0`).
%%
%% Compiled **at test time** with the system `erlc` into a scratch directory (never
%% committed as a `.beam`); the suite is the source of truth for the expected values.
%%
%% Every function returns a term, never a binary: the `run` route prints its result with
%% `~w`, and this runtime's binary printing is `<<55,56,...>>` (a pre-existing printer
%% limitation, unrelated to `zlib`), so hex digests travel as `binary_to_list/1` integer
%% lists and payloads as sizes/booleans.
-module(zlib_stream).
-export([params_stash/0,
         dict_protocol/0,
         safe_inflate_chunks/0,
         reset_and_dictionary/0,
         truncated_stream/0]).

%% The dictionary the `need_dictionary` protocol of `dict_protocol/0` uses.
dictionary() ->
    <<"a dictionary with repeated words words words and more words">>.

%% The payload compressed against `dictionary()`.
dict_payload() ->
    <<"words words words and more words words">>.

%% `deflateParams/3`'s stash path: `zlib.erl` stows the sync-flush progress of its own
%% internal `deflate(Z, <<>>, sync)` through `setStash_nif/2` and recovers it on the next
%% `deflate/3` (`restore_progress/2`), so `deflateParams/3` returns just `ok` while its
%% bytes reappear at the head of the following call's iolist.
params_stash() ->
    Z = zlib:open(),
    ok = zlib:deflateInit(Z, 0, deflated, 15, 8, default),
    D1 = zlib:deflate(Z, binary:copy(<<"A">>, 16), none),
    Params = zlib:deflateParams(Z, 9, default),
    D2 = zlib:deflate(Z, <<"BBBB">>, finish),
    Bin = iolist_to_binary([D1 | D2]),
    Round = zlib:uncompress(Bin),
    ok = zlib:deflateEnd(Z),
    zlib:close(Z),
    {Params,
     [byte_size(X1) || X1 <- lists:flatten(D1)],
     [byte_size(X2) || X2 <- lists:flatten(D2)],
     byte_size(Bin),
     binary_to_list(binary:encode_hex(Bin)),
     Round =:= <<"AAAAAAAAAAAAAAAABBBB">>}.

%% The whole `need_dictionary` protocol: the Adler digest, the exact compressed stream,
%% the raising `inflate/2`, the non-raising `inflate/3` and `safeInflate/2`, the recovery
%% after `inflateSetDictionary/2`, and the per-handle continuity of the need (a fresh
%% handle and a reset handle both answer it again).
dict_protocol() ->
    Dict = dictionary(),
    P = dict_payload(),
    Z = zlib:open(),
    ok = zlib:deflateInit(Z),
    Adler = zlib:deflateSetDictionary(Z, Dict),
    S = iolist_to_binary(zlib:deflate(Z, P, finish)),
    ok = zlib:deflateEnd(Z),
    zlib:close(Z),
    %% The deprecated raising route.
    I = zlib:open(),
    ok = zlib:inflateInit(I),
    Raise = try zlib:inflate(I, S) of
                V -> {value, iolist_size(V)}
            catch
                error:R -> {error, R}
            end,
    %% The same handle, still without the dictionary: not a second `need_dictionary`.
    Again = try zlib:inflate(I, <<>>) of
                V2 -> {value, iolist_size(V2)}
            catch
                error:R2 -> {error, R2}
            end,
    ok = zlib:inflateSetDictionary(I, Dict),
    Recovered = iolist_to_binary(zlib:inflate(I, <<>>)) =:= P,
    %% The `{exception_on_need_dict, false}` route.
    I2 = zlib:open(),
    ok = zlib:inflateInit(I2),
    Opt = zlib:inflate(I2, S, [{exception_on_need_dict, false}]),
    OptSet = zlib:inflateSetDictionary(I2, Dict),
    OptRecovered = iolist_to_binary(zlib:inflate(I2, <<>>)) =:= P,
    %% The `safeInflate/2` route.
    I3 = zlib:open(),
    ok = zlib:inflateInit(I3),
    Safe = zlib:safeInflate(I3, S),
    %% A brand-new handle needs the dictionary too (it is stream state, not global).
    Fresh = zlib:open(),
    ok = zlib:inflateInit(Fresh),
    FreshNeed = try zlib:inflate(Fresh, S) of
                    V3 -> {value, iolist_size(V3)}
                catch
                    error:R3 -> {error, R3}
                end,
    %% ... and so does the handle that already recovered, once it is reset.
    ok = zlib:inflateReset(I),
    AfterReset = try zlib:inflate(I, S) of
                     V4 -> {value, iolist_size(V4)}
                 catch
                     error:R4 -> {error, R4}
                 end,
    zlib:close(I),
    zlib:close(I2),
    zlib:close(I3),
    zlib:close(Fresh),
    {Adler, byte_size(S), binary_to_list(binary:encode_hex(S)), byte_size(Dict),
     Raise, Again, Recovered, Opt, OptSet, OptRecovered, Safe, FreshNeed, AfterReset}.

%% `safeInflate/2`'s chunking on a 100 000-byte payload: six full output chunks of
%% `?INFLATE_OUT_CHUNKSIZE`, the remainder, and one further call that answers `finished`
%% with no output at all.
safe_inflate_chunks() ->
    P = binary:copy(<<"0123456789">>, 10000),
    C = zlib:compress(P),
    Z = zlib:open(),
    ok = zlib:inflateInit(Z),
    Results = [zlib:safeInflate(Z, C),
               zlib:safeInflate(Z, <<>>),
               zlib:safeInflate(Z, <<>>),
               zlib:safeInflate(Z, <<>>),
               zlib:safeInflate(Z, <<>>),
               zlib:safeInflate(Z, <<>>),
               zlib:safeInflate(Z, <<>>),
               zlib:safeInflate(Z, <<>>)],
    Shape = [{Tag, [byte_size(X) || X <- O]} || {Tag, O} <- Results],
    Total = lists:sum([iolist_size(O) || {_Tag, O} <- Results]),
    ok = zlib:inflateEnd(Z),
    zlib:close(Z),
    {Shape, Total, byte_size(P), byte_size(C)}.

%% `inflateReset/1` mid-stream (a fresh `inflate/2` of a valid stream then succeeds),
%% `inflateGetDictionary/1` on an empty and on a dictionary-fed stream, and
%% `deflateReset/1` on a stream that already finished.
reset_and_dictionary() ->
    I0 = zlib:open(),
    ok = zlib:inflateInit(I0),
    FreshDict = zlib:inflateGetDictionary(I0),
    Payload = binary:copy(<<"abcdefgh">>, 1000),
    Good = zlib:compress(Payload),
    %% A 10-byte prefix of a valid stream produces nothing; a reset discards its state.
    Half = binary:part(Good, 0, 10),
    I1 = zlib:open(),
    ok = zlib:inflateInit(I1),
    Part = zlib:inflate(I1, Half),
    ResetOk = zlib:inflateReset(I1),
    AfterReset = zlib:inflate(I1, Good),
    Full = iolist_to_binary(AfterReset) =:= Payload,
    %% The dictionary accessor on a dictionary-fed stream.
    Dict = dictionary(),
    P2 = dict_payload(),
    Z = zlib:open(),
    ok = zlib:deflateInit(Z),
    Adler = zlib:deflateSetDictionary(Z, Dict),
    S = iolist_to_binary(zlib:deflate(Z, P2, finish)),
    ok = zlib:deflateEnd(Z),
    zlib:close(Z),
    I2 = zlib:open(),
    ok = zlib:inflateInit(I2),
    _ = zlib:inflate(I2, S, [{exception_on_need_dict, false}]),
    ok = zlib:inflateSetDictionary(I2, Dict),
    DictGot = zlib:inflateGetDictionary(I2),
    Recovered = iolist_to_binary(zlib:inflate(I2, <<>>)) =:= P2,
    DictAfter = zlib:inflateGetDictionary(I2),
    %% `deflateReset/1` after a finished stream restarts it from the zlib header.
    D = zlib:open(),
    ok = zlib:deflateInit(D),
    _ = zlib:deflate(D, <<"junk">>, finish),
    DReset = zlib:deflateReset(D),
    DOut = iolist_to_binary(zlib:deflate(D, <<"fresh">>, finish)),
    ok = zlib:deflateEnd(D),
    zlib:close(D),
    zlib:close(I0),
    zlib:close(I1),
    zlib:close(I2),
    {byte_size(FreshDict), iolist_size(Part), ResetOk, iolist_size(AfterReset), Full,
     Adler, byte_size(DictGot), DictGot =:= Dict, Recovered,
     byte_size(DictAfter), DictAfter =:= Dict, DReset,
     binary_to_list(binary:encode_hex(DOut))}.

%% A truncated stream (the last 4 bytes — the Adler-32 trailer — dropped) is **not**
%% detected by `inflate/2`: it returns the whole payload. Only `inflateEnd/1` raises
%% `data_error`. The whole stream is the control: no raise, and the end succeeds.
truncated_stream() ->
    P = binary:copy(<<"hello world ">>, 100),
    Good = zlib:compress(P),
    Trunc = binary:part(Good, 0, byte_size(Good) - 4),
    I = zlib:open(),
    ok = zlib:inflateInit(I),
    R = try zlib:inflate(I, Trunc) of
            V -> {value, iolist_size(V), iolist_to_binary(V) =:= P}
        catch
            error:E -> {error, E}
        end,
    EndTrunc = try zlib:inflateEnd(I) of
                   V2 -> {value, V2}
               catch
                   error:E2 -> {error, E2}
               end,
    ok = zlib:close(I),
    I2 = zlib:open(),
    ok = zlib:inflateInit(I2),
    R2 = try zlib:inflate(I2, Good) of
             V3 -> {value, iolist_size(V3), iolist_to_binary(V3) =:= P}
         catch
             error:E3 -> {error, E3}
         end,
    EndWhole = try zlib:inflateEnd(I2) of
                   V4 -> {value, V4}
               catch
                   error:E4 -> {error, E4}
               end,
    ok = zlib:close(I2),
    %% `safeInflate/2` reports `finished` for the truncated stream too — the raise is
    %% still `inflateEnd/1`'s.
    I3 = zlib:open(),
    ok = zlib:inflateInit(I3),
    S = zlib:safeInflate(I3, Trunc),
    S2 = zlib:safeInflate(I3, <<>>),
    EndSafe = try zlib:inflateEnd(I3) of
                  V5 -> {value, V5}
              catch
                  error:E5 -> {error, E5}
              end,
    ok = zlib:close(I3),
    Shape = fun({Tag, O}) -> {Tag, iolist_size(O)} end,
    {byte_size(Good), byte_size(Trunc), R, EndTrunc, R2, EndWhole,
     Shape(S), Shape(S2), EndSafe}.
