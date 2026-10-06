%%% Oracle fixture for `bs_create_bin` **`string` constant segments**.
%%%
%%% A `string` segment is emitted whenever a binary pattern concatenates a literal
%%% byte string with a dynamic part (`beam_ssa_codegen.erl:1970-1976`): the literal
%%% goes into the `StrT` string table and the segment carries its byte offset as the
%%% value operand and its byte count as the size operand.
%%%
%%% Every expected value in `tests/bitstrings_extra_string.rs` is derived from this
%%% module run under the system Erlang (the differential oracle); see
%%% `tests/fixtures/README.md` for the regeneration recipe.
-module(bsstring).

-export([prefix/1, mixed/1, two/1, reuse/1, one/1, combo/2, at_zero/1,
         empty_prefix/1, unaligned_tail/1]).

%% A string prefix followed by a whole binary — the canonical `<<"ab", X/binary>>`.
prefix(X) -> <<"ab", X/binary>>.

%% A string written at a non-byte-aligned position (4 bits of integer in front and
%% 4 bits behind), so the string is copied through the bit-by-bit path.
mixed(A) -> <<A:4, "hi", 3:4>>.

%% Two independent string constants around a value.
two(A) -> <<"ab", A:8, "cd">>.

%% The trailing string reuses a substring of the leading one, so its `StrT` offset
%% is not the start of the table.
reuse(X) -> <<"abc", X/binary, "bc">>.

%% A single-byte string.
one(A) -> <<"z", A:8>>.

%% A string between two dynamic segments.
combo(A, B) -> <<A:8, "xy", B/binary, "z">>.

%% A string at bit offset 0 (the `StrT` offset is 0) followed by an integer.
at_zero(A) -> <<"hi", A:8>>.

%% An empty literal piece before a dynamic part: the compiler drops the empty
%% segment, so the result is just the dynamic part.
empty_prefix(X) -> <<"", X/binary>>.

%% A string after a non-byte-aligned integer tail.
unaligned_tail(A) -> <<A:12, "Q">>.
