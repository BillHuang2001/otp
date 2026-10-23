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

%% The `erlang:display/1` terms `tests/display_oracle.rs` cross-checks against the live
%% system `erl` (compiled at test time with the system `erlc`; never committed as a
%% `.beam`).
%%
%% One term per `erlang:display/1` call, in this order, so the suite can name the output's
%% lines by position:
%%
%%   1. a native record (`-record #r{…}` is the OTP-29 native-record syntax) — the `%T`
%%      renderer's `#Module:Name{key=value,…}` form, fields in the stored key order;
%%   2. a map — `%T` writes `#{k=>v}` with no spaces;
%%   3. a float — printf's `%e` with six fraction digits;
%%   4. an atom;
%%   5. a printable list — the printable-string rule, `"xy"` and never `[120,121]`.
%%
%% Every term is pure ASCII, so the comparison covers the whole stdout image of both
%% runtimes byte for byte (the `%T` renderer's documented latin-1 deviation needs a code
%% unit ≥ 16#80 in a printable string; see `beam_bifs`' `term_conv::display_1`).
-module(display_terms).

-export([all/0]).

-record #r{zz, yy}.

all() ->
    erlang:display(#r{zz = 1, yy = "xy"}),
    erlang:display(#{alpha => 1, beta => [1, 2]}),
    erlang:display(1.5),
    erlang:display(hello),
    erlang:display("xy"),
    ok.
