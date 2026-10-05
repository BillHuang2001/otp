%% Records fixture for `beam_engine`'s `records` instruction family.
%%
%% Compiled with the *system* `erlc` (OTP 29 / erts-17.1), the differential oracle.
%% See `README.md` for the regeneration recipe; in short, build from a neutral
%% directory so no worktree path is baked into the committed `.beam`:
%%
%%   mkdir -p "$TMPDIR/fixgen"
%%   cp erts_rust/crates/beam_engine/tests/fixtures/records.erl "$TMPDIR/fixgen/"
%%   env -C "$TMPDIR/fixgen" erlc records.erl
%%   cp "$TMPDIR/fixgen/records.beam" erts_rust/crates/beam_engine/tests/fixtures/records.beam
%%
%% Every export is compiled to the `update_record` instruction; the record tuple
%% `#r{}` is `{r, A, B, C}` (arity 4, `r` at element 0).
-module(records).

-export([update_a/1, update_b/1, update_a_c/1, chain/1, keep/1, non_record/1,
         badrecord_tag/0]).

-record(r, {a, b, c}).

%% Single field replaced, destination == source register (`reuse` hint).
update_a(R) -> R#r{a = 10}.

%% Single field replaced, a different field (`reuse` hint).
update_b(R) -> R#r{b = 20}.

%% Two fields replaced at once (`copy` hint; the update offsets are 2 and 4).
update_a_c(R) -> R#r{a = 11, c = 33}.

%% The record is built locally, so it is unique and the compiler marks the
%% update `inplace`; the interpreter always copies, which is observationally
%% equivalent for a list of updates like this.
chain(A) -> R = #r{a = A}, R#r{b = A + 1}.

%% The updated record is a *fresh* tuple: the source `R` must be unchanged.
keep(R) -> {R#r{a = x}, R}.

%% A non-record argument fails the compiler's `is_tagged_tuple` guard, so the
%% generated `badrecord` fallback raises `{badrecord, Value}` — the compiled
%% `update_record` itself is never reached with a non-tuple source.
non_record(V) -> V#r{a = 1}.

%% The interpreter resolves a raise's reason-tag atom from the *running module's*
%% atom table (the `badrecord` opcode carries no atom operand, so `non_record/1`
%% alone would not intern it). This export keeps the `badrecord` atom in the
%% module so the uncaught `non_record/1` raise is nameable as `{badrecord, V}`;
%% the oracle value is the atom `badrecord`.
badrecord_tag() -> badrecord.
