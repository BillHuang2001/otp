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
%% The `zlib` guard-class **defect fixture** for `tests/zlib_guards.rs`.
%%
%% `zlib_guards.erl` names the six guard atoms in `guard_atoms/0`; this module is the same
%% probe shape **without** that reference, so `not_initialized` / `already_initialized` /
%% `stream_error` never enter this module's atom table. On this runtime a raise whose reason
%% atom is absent from the raising module's atom table is not caught by `try ... catch` — it
%% escapes `caught/1` and terminates the process (`beam_rt: error <atom>`, exit 1) — while the
%% system `erl` oracle answers `{exception,error,<atom>}` for exactly the same code. That is a
%% runtime defect (an atom-namespace/catch-frame issue, not a `zlib` one); the suite pins both
%% sides verbatim so a fix trips the assertion.
%%
%% Nothing here may name the guard atoms, not even in a comment that the compiler would not
%% read: only *code* references load atoms, and only code references are what this fixture must
%% stay free of.
-module(zlib_guards_unnamed).

-export([deflate_end_twice/0, after_finish/0, deflate_init_twice/0]).

%% One normalized observation, exactly as `zlib_guards:attempt/1` (`{exception,Class,Reason}`
%% or `{value,Value}`).
caught(F) ->
    try {value, F()} catch Class:Reason -> {exception, Class, Reason} end.

%% Print one labeled probe the same way `zlib_guards:p/2` does.
p(Label, F) ->
    io:format("~s ~w~n", [Label, caught(F)]).

%% `deflateEnd/1` on an already-ended stream — the `not_initialized` class.
deflate_end_twice() ->
    p("deflateEnd_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          _ = zlib:deflate(Z, <<"hello">>, finish),
          ok = zlib:deflateEnd(Z),
          zlib:deflateEnd(Z)
      end),
    ok.

%% `deflate/3` with a non-`finish` flush after a `finish` — the class `zlib.erl`'s C glue
%% reports as `stream_error`. The function name must not spell that atom either: an exported
%% function name *is* an atom in the module's table, so naming it here would defeat the whole
%% point of this fixture (measured — the first draft's `stream_error/0` made the class
%% catchable again).
after_finish() ->
    p("deflate_none_after_finish", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          _ = zlib:deflate(Z, <<"hello">>, finish),
          zlib:deflate(Z, <<"more">>, none)
      end),
    ok.

%% A second initialization of the same handle — the `already_initialized` class.
deflate_init_twice() ->
    p("deflateInit_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          zlib:deflateInit(Z)
      end),
    ok.
