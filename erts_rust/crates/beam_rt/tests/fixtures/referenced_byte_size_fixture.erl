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
%% The compiled `binary:referenced_byte_size/1` fixture for `tests/referenced_byte_size.rs`.
%%
%% Compiled at test time by the system `erlc` into a scratch directory; never committed as a
%% `.beam`. `go/0` reproduces the `referenced` and `doctests` bodies of
%% `lib/stdlib/test/binary_module_SUITE.erl` and answers every `referenced_byte_size/1` figure
%% in the suite's order, so `beam_rt run <beam> referenced_byte_size_fixture:go/0` prints the
%% pinned list with `~w~n`.
-module(referenced_byte_size_fixture).

-export([go/0]).

go() ->
    A = <<0:(1024 * 8)>>,
    B = binary:copy(A, 1000),
    RefA = binary:referenced_byte_size(A),
    RefB = binary:referenced_byte_size(B),
    <<_:8,C:1023/binary>> = A,
    RefC = binary:referenced_byte_size(C),
    RefCopyC = binary:referenced_byte_size(binary:copy(C)),
    <<_:7,D:1023/binary,_:1>> = A,
    RefCopyD = binary:referenced_byte_size(binary:copy(D)),
    RefD = binary:referenced_byte_size(D),
    <<_:8,E:128/binary,_/binary>> = B,
    RefE = binary:referenced_byte_size(E),
    RefCopyE = binary:referenced_byte_size(binary:copy(E)),
    <<_:7,F:128/binary,_:1,_/binary>> = B,
    RefCopyF = binary:referenced_byte_size(binary:copy(F)),
    RefF = binary:referenced_byte_size(F),
    [RefA, RefB, RefC, RefCopyC, RefCopyD, RefD,
     RefE, RefCopyE, RefCopyF, RefF, doctest_tuple()].

doctest_tuple() ->
    DocA = binary:copy(<<1>>, 1000),
    <<_:10/binary, DocC/binary>> = DocA,
    {byte_size(DocC), binary:referenced_byte_size(DocC)}.
