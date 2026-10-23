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

%% The `erts_internal:term_to_string/1,2` cases `tests/term_to_string_oracle.rs`
%% cross-checks against the live system `erl` (compiled at test time with the system
%% `erlc`; never committed as a `.beam`).
%%
%% One case per line, in this order, so the suite can name the output's lines by
%% position. Every case is wrapped in
%%
%%     try erts_internal:term_to_string(T [, Limit]) catch error:R -> {error, R} end
%%
%% so the `badarg` arm renders as the deterministic value `{error,badarg}` on both
%% runtimes instead of a stacktrace on stderr plus a divergent exit code. Only the
%% reason is bound — a stacktrace would be multi-line and unaddressable.
%%
%% `show/1` writes the answer with `io:format("~s\r\n")` (a string: the rendering
%% itself, verbatim) or `io:format("~p\r\n")` (`{error,badarg}`), so the terminator
%% is the C's CRLF on both sides and one case is exactly one line.
%%
%%   1. `term_to_string/1` of an atom;
%%   2. … a float — printf's `%e` with six fraction digits;
%%   3. … a map — `#{k=>v}` with no spaces;
%%   4. … a printable list — the printable-string rule, `"hello"`;
%%   5. … a tuple;
%%   6. … a binary — `<<"xy">>`;
%%   7. … a native record (`records:create/4`) — `#Module:Name{field=value,…}`;
%%   8. … a nested mixed list (tuple + map + float + printable list);
%%   9. … a map holding a list holding a record;
%%  10. `term_to_string/2` with `undefined` (the C's `INT_MAX`/unlimited form) of a
%%      tuple — must equal case 5;
%%  11. … with `undefined` of a printable list — must equal case 4;
%%  12. `term_to_string({a,1}, 4)`  — a budget stopping inside a tuple;
%%  13. `term_to_string("hello", 6)` — the budget is exhausted by the list's own bytes,
%%      so **every** byte prints (closing quote included) and the `"... "` tail is
%%      appended anyway;
%%  14. `term_to_string("hello", 5)` — one byte less, so it stops **inside** the
%%      characters and abandons the closing quote;
%%  15. `term_to_string("hello", 7)` — past the end, so the full rendering with **no**
%%      tail;
%%  16. `term_to_string(Record, 16)` — stopping inside the record's field list;
%%  17. `term_to_string(Record, 17)` — one byte later, same field list;
%%  18. `term_to_string(Record, 5)`  — stopping inside the record's name;
%%  19. `term_to_string({a,1}, 2147483646)` — the widest accepted budget, renders in
%%      full;
%%  20. `term_to_string({a,1}, 2147483647)` — `INT_MAX` itself: `error:badarg`;
%%  21. `term_to_string({a,1}, 1)` — `1 < L < INT_MAX` excludes 1: `error:badarg`;
%%  22. `term_to_string({a,1}, 0)` — `error:badarg`;
%%  23. `term_to_string({a,1}, -1)` — `error:badarg`;
%%  24. `term_to_string({a,1}, foo)` — a non-numeric, non-`undefined` limit:
%%      `error:badarg`.
%%
%% # Why the record's field atoms are `ddlfa`/`ddlfb`
%%
%% A native record's definition stores its field keys sorted by **ascending atom
%% index** (`erl_record.c`'s `erts_cmp_flatmap_keys`) and `%T` prints that stored
%% order — so a multi-field record's rendering depends on the *per-VM* atom indices.
%% These two names are ones no OTP module pre-interns, and both runtimes intern them
%% from this very module's atom chunk, in the same order, when they load the same
%% `.beam`; the stored order is therefore the declaration order on both VMs, and the
%% rendering is comparable byte for byte. (`beam_bifs/tests/print_records_c_printf.rs`
%% is where that property is pinned on the `%T` side.)
%%
%% A `-record` declaration would not do: the OTP-29 native-record syntax needs
%% `-record #r{…}` and the record's *name* is then module-scoped, while
%% `records:create/4` reproduces exactly the term the `%T` renderer sees.
-module(term_to_string_terms).

-export([all/0]).

%% The two-field native record every record case renders. Called per case so no
%% case depends on another's heap.
rec() ->
    records:create(recmod, r, [{ddlfa, 1}, {ddlfb, 2}], #{is_exported => true}).

%% One case per line: a string is the rendering itself (`~s`, verbatim), anything
%% else is `{error, Reason}` under `~p`. The terminator is CRLF, so a line split on
%% `\r\n` addresses one case exactly.
show(V) when is_list(V) -> io:format("~s\r\n", [V]);
show(V) -> io:format("~p\r\n", [V]).

all() ->
    show(try erts_internal:term_to_string(hello) catch error:R1 -> {error, R1} end),
    show(try erts_internal:term_to_string(1.5) catch error:R2 -> {error, R2} end),
    show(try erts_internal:term_to_string(#{a => 1}) catch error:R3 -> {error, R3} end),
    show(try erts_internal:term_to_string("hello") catch error:R4 -> {error, R4} end),
    show(try erts_internal:term_to_string({a, 1}) catch error:R5 -> {error, R5} end),
    show(try erts_internal:term_to_string(<<"xy">>) catch error:R6 -> {error, R6} end),
    show(try erts_internal:term_to_string(rec()) catch error:R7 -> {error, R7} end),
    show(try erts_internal:term_to_string([{a, 1}, #{b => 2}, 1.5, "hi"]) catch error:R8 -> {error, R8} end),
    show(try erts_internal:term_to_string(#{rk => [rec()]}) catch error:R9 -> {error, R9} end),
    show(try erts_internal:term_to_string({a, 1}, undefined) catch error:R10 -> {error, R10} end),
    show(try erts_internal:term_to_string("hello", undefined) catch error:R11 -> {error, R11} end),
    show(try erts_internal:term_to_string({a, 1}, 4) catch error:R12 -> {error, R12} end),
    show(try erts_internal:term_to_string("hello", 6) catch error:R13 -> {error, R13} end),
    show(try erts_internal:term_to_string("hello", 5) catch error:R14 -> {error, R14} end),
    show(try erts_internal:term_to_string("hello", 7) catch error:R15 -> {error, R15} end),
    show(try erts_internal:term_to_string(rec(), 16) catch error:R16 -> {error, R16} end),
    show(try erts_internal:term_to_string(rec(), 17) catch error:R17 -> {error, R17} end),
    show(try erts_internal:term_to_string(rec(), 5) catch error:R18 -> {error, R18} end),
    show(try erts_internal:term_to_string({a, 1}, 2147483646) catch error:R19 -> {error, R19} end),
    show(try erts_internal:term_to_string({a, 1}, 2147483647) catch error:R20 -> {error, R20} end),
    show(try erts_internal:term_to_string({a, 1}, 1) catch error:R21 -> {error, R21} end),
    show(try erts_internal:term_to_string({a, 1}, 0) catch error:R22 -> {error, R22} end),
    show(try erts_internal:term_to_string({a, 1}, -1) catch error:R23 -> {error, R23} end),
    show(try erts_internal:term_to_string({a, 1}, foo) catch error:R24 -> {error, R24} end),
    ok.