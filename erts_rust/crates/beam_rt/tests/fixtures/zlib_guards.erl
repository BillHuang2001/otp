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
%% The `zlib` guard-class / controlling-process fixture for `tests/zlib_guards.rs`.
%%
%% Compiled at test time by the system `erlc` into a scratch directory; never committed as a
%% `.beam` (the source is the single artifact). Every exported function prints its transcript
%% with `io:format/2` and returns `ok`, so the observable is the printed transcript — never the
%% value the CLI's `~w` result printer would render (a binary renders as `<<55,56,…>>` on the
%% scheduled `run` route, a pre-existing printer difference unrelated to `zlib`). No binary is
%% ever printed: a probe returns an atom, an integer, a boolean or a small tuple.
%%
%% The transcript is the ONE source of truth for both routes of the Rust suite: `-eval`
%% (`zlib_guards:classes()` through the tree-walking evaluator) and the scheduled
%% `run <zlib_guards.beam> zlib_guards:classes/0`. Writing the probes once is what keeps the two
%% routes comparable — a divergence between them is then a real driver difference, not a
%% transcription slip.
-module(zlib_guards).

-export([guard_atoms/0, classes/0, controlling/0, closed_controller/0]).

%% The six guard-class reason atoms, **named** so they enter this module's atom table.
%%
%% Documentation of the class set, not a requirement: a NIF-raised class has no atom literal in
%% any module (`zlib.beam` never spells these — the C NIF's own static atoms carry them), yet
%% `try ... catch` catches it and reports the class, because the runtime names a BIF raise's
%% reason atom in its one shared atom table. `tests/zlib_guards.rs` pins that independently of
%% any module's atom table with the `zlib_guards_unnamed` fixture, which is the same probes
%% without this reference and prints the same transcripts.
guard_atoms() ->    [not_initialized, already_initialized, stream_error, data_error, badarg,
     not_on_controlling_process].

%% ---------------------------------------------------------------------------------------
%% The normalized observation
%% ---------------------------------------------------------------------------------------

%% One normalized observation: the raised `{exception, Class, Reason}` (never a
%% `{'EXIT', {Reason, Stacktrace}}`, so a stack-shape difference cannot mask a class), or the
%% `{value, Value}` the wrapper returned.
attempt(F) ->
    try {value, F()} catch Class:Reason -> {exception, Class, Reason} end.

%% Print one labeled probe. `~w` never wraps, so a long atom name cannot split a line and make
%% the transcript printer-dependent.
p(Label, F) ->
    io:format("~s ~w~n", [Label, attempt(F)]).

%% ---------------------------------------------------------------------------------------
%% A second process, for the controlling-process guards
%% ---------------------------------------------------------------------------------------

%% Run `F` in a second process (which is therefore NOT the stream's controller) and bring its
%% `attempt/1` answer back. Bounded: a child that never answers is the atom `timeout`, never a
%% hang.
foreign(F) ->
    Self = self(),
    _ = spawn(fun() -> Self ! {child, attempt(F)} end),
    await().

%% Spawn a second process that waits for `go`, so the parent can transfer ownership first, then
%% runs `F` and reports its `attempt/1` answer.
child(F) ->
    Self = self(),
    spawn(fun() ->
                  receive
                      go -> ok
                  end,
                  Self ! {child, attempt(F)}
          end).

%% The child's answer (bounded).
await() ->
    receive
        {child, R} -> R
    after 5000 -> timeout
    end.

%% ---------------------------------------------------------------------------------------
%% 1. The guard classes on a single stream
%% ---------------------------------------------------------------------------------------

%% The class of `deflateEnd/1` after one `deflate/3` with `Flush`, on a fresh level-default
%% stream. `zlib.erl:430-434` documents the `data_error` case: "this function throws a
%% `data_error` exception if the last call to `deflate/3` was not called with `Flush` set to
%% `finish`".
deflate_then_end(Flush) ->
    Z = zlib:open(),
    ok = zlib:deflateInit(Z),
    _ = zlib:deflate(Z, <<"hello">>, Flush),
    R = zlib:deflateEnd(Z),
    ok = zlib:close(Z),
    R.

%% A `deflate`-side `deflate_end` variant: `finish` first, then a `deflate/3` whose flush is not
%% `finish` — zlib's own `Z_STREAM_ERROR` ("user must not provide more input after the first
%% FINISH"), which is the only way the wrapper can reach `stream_error`.
after_finish(Flush) ->
    Z = zlib:open(),
    ok = zlib:deflateInit(Z),
    _ = zlib:deflate(Z, <<"hello">>, finish),
    zlib:deflate(Z, <<"more">>, Flush).

%% Open a stream, initialize it with `Init`, then run `Op` — the wrong-kind guard family: the
%% NIF's state precondition is what refuses the operation.
on_stream(Init, Op) ->
    Z = zlib:open(),
    ok = Init(Z),
    Op(Z).

%% Every guard class the wrapper's own exported surface can reach, one printed line each.
classes() ->
    %% 1. `deflateEnd/1` after a non-`finish` `deflate/3` — the documented class.
    p("deflate_none_then_deflateEnd", fun() -> deflate_then_end(none) end),
    p("deflate_sync_then_deflateEnd", fun() -> deflate_then_end(sync) end),
    p("deflate_full_then_deflateEnd", fun() -> deflate_then_end(full) end),
    %% 2. ... after `finish`, and with no `deflate/3` at all: both are `ok`.
    p("deflate_finish_then_deflateEnd", fun() -> deflate_then_end(finish) end),
    p("fresh_deflateEnd", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          R = zlib:deflateEnd(Z),
          ok = zlib:close(Z),
          R
      end),
    p("deflateEnd_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          _ = zlib:deflate(Z, <<"hello">>, finish),
          ok = zlib:deflateEnd(Z),
          R = zlib:deflateEnd(Z),
          ok = zlib:close(Z),
          R
      end),
    %% 3. A second `finish` — the oracle's own shape is `[]`, NOT `stream_error` (see the Rust
    %% suite's module docs); a non-`finish` flush after a `finish` IS `stream_error`.
    p("deflate_empty_finish_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          _ = zlib:deflate(Z, <<"hello">>, finish),
          R = zlib:deflate(Z, [], finish),
          ok = zlib:deflateEnd(Z),
          ok = zlib:close(Z),
          R
      end),
    p("deflate_none_after_finish", fun() -> after_finish(none) end),
    p("deflate_sync_after_finish", fun() -> after_finish(sync) end),
    p("deflate_data_finish_after_finish", fun() -> after_finish(finish) end),
    %% 4. The wrong-kind family: a deflate call on an inflate stream and every mirror.
    p("deflate_on_an_inflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:inflateInit(Z) end,
                    fun(Z) -> zlib:deflate(Z, <<"hello">>, none) end)
      end),
    p("inflate_on_a_deflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:deflateInit(Z) end,
                    fun(Z) -> zlib:inflate(Z, <<"hello">>) end)
      end),
    p("deflateEnd_on_an_inflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:inflateInit(Z) end, fun(Z) -> zlib:deflateEnd(Z) end)
      end),
    p("inflateEnd_on_a_deflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:deflateInit(Z) end, fun(Z) -> zlib:inflateEnd(Z) end)
      end),
    p("deflateReset_on_an_inflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:inflateInit(Z) end, fun(Z) -> zlib:deflateReset(Z) end)
      end),
    p("inflateReset_on_a_deflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:deflateInit(Z) end, fun(Z) -> zlib:inflateReset(Z) end)
      end),
    p("deflateParams_on_an_inflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:inflateInit(Z) end,
                    fun(Z) -> zlib:deflateParams(Z, 1, default) end)
      end),
    p("deflateSetDictionary_on_an_inflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:inflateInit(Z) end,
                    fun(Z) -> zlib:deflateSetDictionary(Z, <<"dict">>) end)
      end),
    p("inflateSetDictionary_on_a_deflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:deflateInit(Z) end,
                    fun(Z) -> zlib:inflateSetDictionary(Z, <<"dict">>) end)
      end),
    p("inflateGetDictionary_on_a_deflate_stream", fun() ->
          on_stream(fun(Z) -> zlib:deflateInit(Z) end, fun(Z) -> zlib:inflateGetDictionary(Z) end)
      end),
    %% 5. Closing.
    p("deflateEnd_then_close", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          _ = zlib:deflate(Z, <<"hello">>, finish),
          ok = zlib:deflateEnd(Z),
          zlib:close(Z)
      end),
    p("close_a_fresh_stream", fun() ->
          Z = zlib:open(),
          zlib:close(Z)
      end),
    p("close_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:close(Z),
          zlib:close(Z)
      end),
    p("close_twice_after_inflateInit", fun() ->
          Z = zlib:open(),
          ok = zlib:inflateInit(Z),
          ok = zlib:close(Z),
          zlib:close(Z)
      end),
    p("deflate_on_a_closed_stream", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          ok = zlib:close(Z),
          zlib:deflate(Z, <<"x">>, none)
      end),
    %% 6. A second initialization of the same handle.
    p("deflateInit_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          zlib:deflateInit(Z)
      end),
    p("inflateInit_after_deflateInit", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          zlib:inflateInit(Z)
      end),
    p("inflateInit_twice", fun() ->
          Z = zlib:open(),
          ok = zlib:inflateInit(Z),
          zlib:inflateInit(Z)
      end),
    %% 7. A term that is not a stream handle at all: the C's `enif_get_resource` miss.
    p("close_a_non_handle", fun() -> zlib:close(42) end),
    p("deflateInit_a_non_handle", fun() -> zlib:deflateInit(42) end),
    p("deflate_a_non_handle", fun() -> zlib:deflate(42, <<"x">>, none) end),
    p("inflate_a_non_handle", fun() -> zlib:inflate(42, <<"x">>) end),
    ok.

%% ---------------------------------------------------------------------------------------
%% 2. The controlling process
%% ---------------------------------------------------------------------------------------

%% The payload the ownership round trip compresses and recovers.
payload() ->
    <<"the zlib controlling-process payload">>.

%% `not_on_controlling_process` and `set_controlling_process/2`, both halves.
%%
%% The first NIF `zlib.erl` reaches is `getStash_nif/1` (`restore_progress/2`, before every
%% `deflate`/`inflate`), so a foreign process is refused there — before the stream's own state
%% is ever consulted, which one probe below pins by asking a foreign process for a
%% wrong-kind operation.
controlling() ->
    %% A foreign process may not `deflate`, `close`, `deflateInit` or `deflateEnd`.
    p("foreign_process_deflate", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          R = foreign(fun() -> zlib:deflate(Z, <<"x">>, none) end),
          ok = zlib:close(Z),
          R
      end),
    p("foreign_process_close", fun() ->
          Z = zlib:open(),
          R = foreign(fun() -> zlib:close(Z) end),
          %% the refusal must not have closed the stream: the controller still can
          {R, zlib:close(Z)}
      end),
    p("foreign_process_deflateInit", fun() ->
          Z = zlib:open(),
          R = foreign(fun() -> zlib:deflateInit(Z) end),
          ok = zlib:close(Z),
          R
      end),
    p("foreign_process_deflateEnd", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          R = foreign(fun() -> zlib:deflateEnd(Z) end),
          ok = zlib:deflateEnd(Z),
          ok = zlib:close(Z),
          R
      end),
    p("foreign_process_inflate_on_deflate_stream", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          R = foreign(fun() -> zlib:inflate(Z, <<"x">>) end),
          ok = zlib:close(Z),
          R
      end),
    %% A foreign process may not answer for a freshly opened (unused) stream either, and the
    %% controller check precedes the closed-state one.
    p("foreign_process_on_a_fresh_stream", fun() ->
          Z = zlib:open(),
          R = foreign(fun() -> zlib:deflate(Z, <<"x">>, none) end),
          ok = zlib:close(Z),
          R
      end),
    p("foreign_process_close_on_a_closed_stream", fun() ->
          Z = zlib:open(),
          ok = zlib:close(Z),
          foreign(fun() -> zlib:close(Z) end)
      end),
    %% `set_controlling_process/2`: the new owner may drive the stream and the previous owner
    %% may not — both halves of the transfer.
    p("transfer_new_owner_drives", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          Pid = child(fun() -> iolist_size(zlib:deflate(Z, <<"abc">>, none)) > 0 end),
          ok = zlib:set_controlling_process(Z, Pid),
          Refused = attempt(fun() -> zlib:deflate(Z, <<"abc">>, none) end),
          Pid ! go,
          {Refused, await()}
      end),
    %% ... and back: the child compresses, hands ownership back, and the parent finishes the
    %% stream and inflates the child's bytes with the SAME handle.
    p("transfer_back_round_trip", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          Self = self(),
          Pid = spawn(fun() ->
                              receive
                                  go -> ok
                              end,
                              C = iolist_to_binary([zlib:deflate(Z, payload(), none),
                                                    zlib:deflate(Z, [], finish)]),
                              ok = zlib:deflateEnd(Z),
                              ok = zlib:set_controlling_process(Z, Self),
                              Self ! {child, C}
                      end),
          ok = zlib:set_controlling_process(Z, Pid),
          Pid ! go,
          Compressed = await(),
          ok = zlib:inflateInit(Z),
          Back = iolist_to_binary(zlib:inflate(Z, Compressed)),
          ok = zlib:inflateEnd(Z),
          ok = zlib:close(Z),
          {byte_size(Back), Back =:= payload(), Back =:= Compressed}
      end),
    %% `set_controlling_process/2` to a pid that is not alive: `enif_get_local_pid` accepts a
    %% dead local pid, so the transfer succeeds — and the caller is then no longer the
    %% controller.
    p("transfer_to_a_dead_pid", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          Dead = spawn(fun() -> ok end),
          MRef = erlang:monitor(process, Dead),
          receive
              {'DOWN', MRef, process, Dead, _} -> ok
          after 5000 -> timeout
          end,
          R = attempt(fun() -> zlib:set_controlling_process(Z, Dead) end),
          After = attempt(fun() -> zlib:deflate(Z, <<"x">>, none) end),
          {R, After}
      end),
    %% A non-pid is `badarg`, and a refused transfer leaves the ownership unchanged.
    p("transfer_to_a_non_pid", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          R = attempt(fun() -> zlib:set_controlling_process(Z, not_a_pid) end),
          {R, zlib:close(Z)}
      end),
    %% Transferring to oneself keeps the ownership.
    p("transfer_to_self", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          ok = zlib:set_controlling_process(Z, self()),
          N = iolist_size(zlib:deflate(Z, <<"abc">>, finish)),
          ok = zlib:deflateEnd(Z),
          ok = zlib:close(Z),
          N > 0
      end),
    ok.

%% ---------------------------------------------------------------------------------------
%% 3. `set_controlling_process/2` across the stream's states
%% ---------------------------------------------------------------------------------------

%% The C imposes **no** state precondition on `set_controller_nif` (`zlib_nif.c:583-602`), so
%% the answer is `ok` even on a CLOSED stream. `tests/zlib_guards.rs` records that this
%% runtime raises `not_initialized` there and keeps this transcript so the boundary of the
%% divergence (only the CLOSED state, not the ended one) is explicit.
closed_controller() ->
    p("set_controlling_process_on_a_closed_stream", fun() ->
          Z = zlib:open(),
          ok = zlib:close(Z),
          zlib:set_controlling_process(Z, self())
      end),
    p("set_controlling_process_on_an_ended_stream", fun() ->
          Z = zlib:open(),
          ok = zlib:deflateInit(Z),
          _ = zlib:deflate(Z, <<"x">>, finish),
          ok = zlib:deflateEnd(Z),
          R = zlib:set_controlling_process(Z, self()),
          ok = zlib:close(Z),
          R
      end),
    p("set_controlling_process_on_a_fresh_stream", fun() ->
          Z = zlib:open(),
          R = zlib:set_controlling_process(Z, self()),
          ok = zlib:close(Z),
          R
      end),
    ok.
