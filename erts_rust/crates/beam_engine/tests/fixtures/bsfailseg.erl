%% Oracle fixture for the `bs_create_bin` **segment-failure surface**: the
%% `{Class, Reason}` pair a caught construction failure produces, so the reason
%% atoms `badarg` and `system_limit` are pinned end to end against the real `erl`.
%%
%% The committed `bsfailseg.beam` is produced by the system `erlc` (OTP 29 /
%% erts-17.1) exactly as `tests/fixtures/README.md` prescribes (build from a
%% neutral directory so no worktree path is baked in); the expected values in
%% `tests/bitstrings_create_fail.rs` are the real system `erl` oracle's answers
%% for the same inputs.
%%
%% ## Why `pinned_atoms/0` exists
%%
%% A compiled module does **not** carry the runtime's exception reason atoms in
%% its atom chunk: `beam_lib:chunks("bsfailseg.beam",[atoms])` shows no `badarg`
%% and no `system_limit` (the real runtime keeps them globally interned,
%% `erts/emulator/beam/beam_common.c:382-406`). The engine resolves a failure's
%% reason atom from the *running module's* atom table and refuses to substitute a
%% different atom, so a construction failure in a module that cannot *name* the
%% atom degrades to a typed `BeamError` instead of a `{error, Atom}` term.
%% `pinned_atoms/0` returns the list `[badarg, badmatch, system_limit, error]`,
%% which interns exactly those atoms into this module's atom chunk, so every
%% failure below lands as a real `{error, ReasonAtom}` 2-tuple.
%%
%% ## What each export exercises
%%
%% Every function is a single `bs_create_bin` whose failure is triggered from an
%% argument (verified with `beam_disasm:file/1`); the `try ... catch C:R ->
%% {C, R} end` hands the raised `{Class, Reason}` pair back to the caller.
%%
%% * `bin_one/1` (`<<X:1/binary>>`) — a `binary` segment needing one byte of a
%%   shorter source (`src_bits < nbits` -> badarg).
%% * `bin_all/1` (`<<X/binary>>`) — the whole-tail `binary` segment (succeeds for
%%   any bitstring source).
%% * `bin_all_unit/1` (`<<X/binary-unit:8>>`) — the whole-tail unit check
%%   (`erts_bs_put_binary_all`: a source whose bit size is not a multiple of the
%%   unit -> badarg).
%% * `bin_fixed/1` (`<<X:8/binary>>`) — a fixed-width `binary` segment, the
%%   unit-8 spelling of `bin_one/1`.
%% * `bin_size/2` (`<<X:S/binary>>`) — a **dynamic** segment size: a negative
%%   size -> badarg (`emu/bs_instrs.tab:423`).
%% * `bin_big_size/1` (`<<X:(1 bsl 59)/binary>>`) — a **positive bignum** size
%%   (`2^59 = MAX_SMALL + 1`) -> system_limit (`emu/bs_instrs.tab:438`).
%% * `utf8/1`, `utf16/1`, `utf16le/1`, `utf32/1` — a codepoint outside the valid
%%   scalar range (`-1`, a lone surrogate, or above `16#10FFFF`) or a non-integer
%%   value -> badarg (`erl_bits.c:1162`/`:1222`, `erts_bs_put_utf8`/`_utf16`).
%% * `int_size/2` (`<<V:S/integer>>`) — the same dynamic-size validation as
%%   `bin_size/2` on an `integer` segment (`-1` -> badarg, a positive bignum ->
%%   system_limit).
%% * `int_fixed/1` (`<<V:3/integer>>`) — a fixed 3-bit `integer` segment: a
%%   non-integer value -> badarg (`erts_bs_put_integer_*`, `erl_bits.c:926`).
%% * `float_size/2` (`<<F:S/float>>`) — a dynamic float width: a width other than
%%   16/32/64 (including 0) -> badarg, `32` -> the 4-byte IEEE-754 binary32
%%   (`erts_bs_put_float`, `erl_bits.c:1341`/`:1449`).
-module(bsfailseg).

-export([pinned_atoms/0,
         bin_one/1, bin_all/1, bin_all_unit/1, bin_fixed/1, bin_size/2,
         bin_big_size/1,
         utf8/1, utf16/1, utf16le/1, utf32/1,
         int_size/2, int_fixed/1,
         float_size/2]).

%% The reason/class atoms the construction failures below need to *name*; a
%% compiled module does not otherwise carry them (see the header comment).
pinned_atoms() -> [badarg, badmatch, system_limit, error].

%% `binary` segments.
bin_one(X) -> try <<X:1/binary>> catch K:R -> {K, R} end.
bin_all(X) -> try <<X/binary>> catch K:R -> {K, R} end.
bin_all_unit(X) -> try <<X/binary-unit:8>> catch K:R -> {K, R} end.
bin_fixed(X) -> try <<X:8/binary>> catch K:R -> {K, R} end.
bin_size(X, S) -> try <<X:S/binary>> catch K:R -> {K, R} end.
bin_big_size(X) -> try <<X:(1 bsl 59)/binary>> catch K:R -> {K, R} end.

%% `utf8`/`utf16`/`utf32` segments (the codepoint is the value operand).
utf8(Cp) -> try <<Cp/utf8>> catch K:R -> {K, R} end.
utf16(Cp) -> try <<Cp/utf16>> catch K:R -> {K, R} end.
utf16le(Cp) -> try <<Cp/utf16-little>> catch K:R -> {K, R} end.
utf32(Cp) -> try <<Cp/utf32>> catch K:R -> {K, R} end.

%% `integer` segments: a dynamic size and a fixed width.
int_size(V, S) -> try <<V:S/integer>> catch K:R -> {K, R} end.
int_fixed(V) -> try <<V:3/integer>> catch K:R -> {K, R} end.

%% `float` segments: a dynamic width.
float_size(F, S) -> try <<F:S/float>> catch K:R -> {K, R} end.
