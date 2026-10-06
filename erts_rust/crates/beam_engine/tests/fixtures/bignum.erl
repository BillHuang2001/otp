%% Test fixture for compiled bignum arithmetic in `beam_engine`.
%%
%% Two shapes are covered:
%%
%% * functions that return a big **integer literal** as a compiler operand. The
%%   compiler emits such a value inline in the instruction stream
%%   (`move {integer, V} {x,0}`) for the range the loader accepts
%%   (`[2^59, 2^63)` — the loader's `read_tagged` caps a magnitude at 8 bytes), so
%%   the engine must materialise it as an exact bignum through
%%   `terms::make_integer`;
%% * functions that **overflow at runtime** from arguments (so the compiler cannot
%%   constant-fold them) — the arithmetic routes through
%%   `ExecutionContext::call_bif`.
%%
%% Deliberately NO constant-foldable overflow expression here: `erlc` folds e.g.
%% `123456789012345678 * 98` into a 9-byte operand, which `beam_loader` rejects
%% with `system_limit` at load time.
-module(bignum).

-export([
    lit_2pow59/0,
    lit_2pow59_plus1/0,
    lit_nines/0,
    lit_2pow61/0,
    lit_neg_2pow59/0,
    lit_neg_nines/0,
    lit_neg_2pow59_minus1/0,
    max_small/0,
    min_small/0,
    add_to_big/1,
    big_minus/1,
    scale_big/1,
    add/2,
    sub/2,
    mul/2
]).

%% 2^59 — the smallest value above MAX_SMALL (2^59 - 1). A one-digit bignum.
lit_2pow59() -> 576460752303423488.

%% 2^59 + 1 — a one-digit bignum whose digit is not a power of two.
lit_2pow59_plus1() -> 576460752303423489.

%% Just below 2^60; still a one-digit bignum.
lit_nines() -> 999999999999999999.

%% 2^61 — a one-digit bignum above 2^60.
lit_2pow61() -> 2305843009213693952.

%% MIN_SMALL itself (a small int) — the boundary of the negative literal range.
lit_neg_2pow59() -> -576460752303423488.

%% A negative one-digit bignum.
lit_neg_nines() -> -999999999999999999.

%% MIN_SMALL - 1 — the smallest negative value that must become a bignum.
lit_neg_2pow59_minus1() -> -576460752303423489.

%% MAX_SMALL (2^59 - 1) — the largest small int, still an immediate.
max_small() -> 576460752303423487.

%% MIN_SMALL (-2^59) — the smallest small int, still an immediate.
min_small() -> -576460752303423488.

%% Big **literal operands** feeding a runtime BIF (the argument is dynamic, so the
%% expression cannot be folded): the engine must materialise the bignum from the
%% code stream before `call_bif` ever sees it.
add_to_big(X) -> 576460752303423488 + X.

big_minus(X) -> 999999999999999999 - X.

scale_big(X) -> 2305843009213693952 * X.

%% Runtime overflow from arguments: the result grows past the small range in the
%% BIF (`gc_bif '+'`), so the compiler cannot fold it.
add(A, B) -> A + B.

sub(A, B) -> A - B.

%% Two-digit bignum results for bignum arguments.
mul(A, B) -> A * B.
