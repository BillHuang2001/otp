%% Data-access oracle fixture for `beam_engine` (the real-`.beam` variant proofs,
%% `tests/real_beam_variant_proofs/data.rs`).
%%
%% Compiled with the *system* `erlc` (OTP 29 / erts-17.1), the differential
%% oracle; see `README.md` for the regeneration recipe, in short:
%%
%%   mkdir -p "$TMPDIR/fixgen"
%%   cp erts_rust/crates/beam_engine/tests/fixtures/vb_data.erl "$TMPDIR/fixgen/"
%%   env -C "$TMPDIR/fixgen" erlc +deterministic vb_data.erl
%%   cp "$TMPDIR/fixgen/vb_data.beam" erts_rust/crates/beam_engine/tests/fixtures/vb_data.beam
%%
%% ## Why this module exists
%%
%% `tests/fixtures/lists.beam` already compiles to `get_list`/`get_hd`/`get_tl`,
%% but its module is *named* `lists`, so it shadows the system `lists` module on
%% the oracle side (`-pz` appends the fixture directory, the stdlib wins) and
%% `lists:head/1` does not exist there — no oracle answer is obtainable. This
%% fixture carries the same shapes under a name no OTP application uses
%% (`vb_data`), so both the engine and a live `erl` can be asked for the same
%% call, and it adds the record shapes (`update_record`, `badrecord`) so the whole
%% data family is proven from one compiled file.
-module(vb_data).
-export([split/1, head/1, tail/1, update_a/1, update_a_c/1, non_record/1,
         badrecord_tag/0]).
-record(r, {a, b, c}).
%% get_list: split a cons cell into its head and tail (both are used, so the
%% compiler keeps the single `get_list` rather than a `get_hd`/`get_tl` pair).
split(L) ->
    case L of
        [H | T] -> {H, T}
    end.
%% get_hd: only the head is used, so the compiler emits `get_hd`.
head(L) ->
    case L of
        [H | _T] -> H
    end.
%% get_tl: only the tail is used, so the compiler emits `get_tl`.
tail(L) ->
    case L of
        [_H | T] -> T
    end.
%% update_record: one field replaced, destination == source register (`reuse`).
update_a(R) -> R#r{a = 10}.
%% update_record: two fields replaced at once (`copy` hint, offsets 2 and 4).
update_a_c(R) -> R#r{a = 11, c = 33}.
%% badrecord: a non-record argument fails the compiler's `is_tagged_tuple`
%% guard, so the generated `badrecord` fallback raises `{badrecord, Value}`.
non_record(V) -> V#r{a = 1}.
%% The `badrecord` opcode carries no atom operand, so the interpreter names the
%% `{badrecord, Value}` reason from the *running module's* atom table; this export
%% keeps that atom in the module (the oracle value is the atom `badrecord`).
badrecord_tag() -> badrecord.
