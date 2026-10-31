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
%% The compiled `binary:part/3` window fixture for
%% `tests/binary_part_refc_window.rs`.
%%
%% `go/0` builds an off-heap parent with the engine's `bs_create_bin` path, then takes a
%% 510-byte `binary:part/3` window over it. The window must stay a `[BinRef][ErlSubBits]`
%% pair over the **parent** payload on the way back to the caller, so
%% `binary:referenced_byte_size/1` answers the parent's byte size (1025), not the window's
%% own 510. The parent is built by a compiled module — hence exact-size (1025) — unlike the
%% `-eval` route's grown-capacity accumulator, which reports 2050.
-module(binary_part_refc_window_fixture).

-export([go/0]).

go() ->
    B = <<"z",(binary:copy(<<255>>,1024))/binary>>,
    C = binary:part(B, 1, 510),
    {byte_size(C), binary:referenced_byte_size(C)}.
