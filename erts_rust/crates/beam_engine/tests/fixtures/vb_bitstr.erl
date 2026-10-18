%% Oracle fixture for the **bit-syntax match variants** that the
%% real-`.beam` variant-proof suite (`tests/real_beam_variant_proofs/bitstrings.rs`)
%% proves: `bs_match`, `bs_get_position`, `bs_set_position`, `bs_get_tail`,
%% `bs_get_binary2`, `bs_skip_bits2` and `bs_get_utf16`.
%%
%% The committed `vb_bitstr.beam` is produced by the system `erlc` (OTP 29 /
%% erts-17.1) exactly as `tests/fixtures/README.md` prescribes (build from a
%% neutral directory with `+deterministic` so no worktree path is baked in).
%%
%% Every expected value in the test suite is the **real system `erl` oracle's**
%% answer for the same call; nothing is guessed.
%%
%% ## What each export exercises (verified with `beam_disasm:file/1`)
%%
%% * `flat/1` — `<<A:16, B:16, R/binary>>` in a function head. The two
%%   literal-size reads and the tail become ONE flattened command list
%%   (`{test,bs_start_match3,…}` + `{bs_match, {f,Fail}, Ctx, {commands,
%%   [{ensure_at_least,32,8}, {integer,…}, {integer,…}, {get_tail,…}]}}`), so it
%%   drives the `bs_match` instruction; the `bs_get_position` the compiler emits
%%   to be able to rewind also executes on its success path.
%% * `rewind/1` — a two-clause `case` whose FIRST clause carries the literal
%%   prefix `"ab"`: when that clause fails, the emitted fail block runs
%%   `bs_set_position Ctx Pos` (restoring the saved position) and re-matches from
%%   there, so `bs_set_position` executes on a **success path** for a subject that
%%   does not start with `"ab"`.
%% * `whole/2` — `<<C:N/binary, R/binary>> = B, {B, C, R}`: a **dynamic** sized
%%   binary segment (`{test,bs_get_binary2,…}`) whose original subject `B` is also
%%   returned. The compiler degenerates that occurrence of `B` to a rewind plus a
%%   tail read, emitting `bs_set_position Ctx Start` followed by a **standalone
%%   `bs_get_tail`** on the success path (the tail read of `R` itself stays a
%%   `bs_match` command, which is why the same function covers both shapes).
%% * `sized/2` — `<<X:N/binary, R/binary>>` inside a `case`, i.e. the same sized
%%   segment with a `case_end` failure path.
%% * `skip/2` — `<<_:N/unit:8, R/binary>>`, a **dynamic** skip:
%%   `{test,bs_skip_bits2, {f,Fail}, [Ctx, Size, 8, Flags]}`.
%% * `utf16/1` — `<<C/utf16>>` in a function head:
%%   `{test,bs_get_utf16, {f,Fail}, [Ctx, Live, Flags, C]}`.
%%
%% ## Why `bs_get_tail` is the interesting one
%%
%% The bit-syntax code generator folds a tail read into the `bs_match` command
%% list whenever it can (`{get_tail,…}`), so the standalone `bs_get_tail`
%% instruction does NOT run for the obvious `case B of <<_:8, R/binary>> -> R end`.
%% It is emitted where the tail read cannot be a command — here because the same
%% match context must first be rewound to yield the whole subject — or on a
%% failure path. Both routes are covered: `whole/2` (success) and the fail block of
%% `sized/2`/`skip/2` (raise).
%%
%% The `sized/2`, `skip/2` and `rewind/1` wrappers use `case` (not extra function
%% heads) so a short/malformed subject lands on the engine's `case_end` handler and
%% raises `{case_clause, Subject}` instead of a bare `function_clause`, which is
%% what the malformed-input proof compares against the oracle.
-module(vb_bitstr).
-export([flat/1, rewind/1, whole/2, sized/2, skip/2, utf16/1]).

%% A flattened literal-size match: `bs_match` (+ the emitted `bs_get_position`).
flat(<<A:16, B:16, R/binary>>) ->
    {A, B, R}.

%% A literal prefix in the first `case` clause: the fail block rewinds with
%% `bs_set_position` and matches again from the restored position.
rewind(B) ->
    case B of
        <<"ab", R/binary>> -> {ab, R};
        <<_, R/binary>> -> {other, R}
    end.

%% A dynamic sized-binary read whose whole subject is also returned: the subject's
%% occurrence compiles to `bs_set_position` + a standalone `bs_get_tail`.
whole(B, N) ->
    <<C:N/binary, R/binary>> = B,
    {B, C, R}.

%% `bs_get_binary2` with a `case_end` failure path.
sized(B, N) ->
    case B of
        <<X:N/binary, R/binary>> -> {X, R}
    end.

%% `bs_skip_bits2` with a dynamic size.
skip(B, N) ->
    case B of
        <<_:N/unit:8, R/binary>> -> {N, R}
    end.

%% `bs_get_utf16` from a function-head pattern.
utf16(<<C/utf16>>) ->
    C.
