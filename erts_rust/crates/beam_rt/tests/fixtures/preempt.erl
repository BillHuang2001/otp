%%% The pre-emption/interleaving fixture (`tests/preempt.rs`).
%%%
%%% Rebuilt with the system `erlc` and committed as `preempt.beam`, using the same
%%% deterministic scratch-dir recipe as the sibling committed fixtures (see this
%%% directory's `README.md`):
%%%
%%%     SCRATCH="$TMPDIR/beam_rt_preempt_fix"
%%%     rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
%%%     cp preempt.erl "$SCRATCH/gen/"
%%%     ( cd "$SCRATCH/gen" && erlc +deterministic preempt.erl )
%%%     cp "$SCRATCH/gen/preempt.beam" preempt.beam
%%%
%%% The runtime's reduction budget is charged at **two** places (`beam_rt/src/runner/`):
%%% at every interpreted instruction, through the engine's `reduce` hook, and at the
%%% heap-admission points (`test_heap`/`allocate_heap`). Interpreted code therefore
%%% pre-empts at instruction granularity, and allocating code additionally pre-empts at
%%% its allocations.
%%%
%%% `alloc_loop/2` is an ALLOCATING tail-recursive loop: every iteration builds one
%%% cons cell (`[N | Acc]`), so the compiler emits a `test_heap` before that
%%% allocation. Its per-iteration allocation thus gives it a heap-admission charge in
%%% addition to the per-instruction charge, so a call with an `N` far above the
%%% 4000-reduction slice budget is pre-empted (yielded) part-way through and only
%%% finishes over several slices.
%%%
%%% `spin/2` is a NON-allocating tail-recursive counter: it does only small-integer
%%% arithmetic and a tail call, so the compiler emits no `test_heap` and no
%%% `allocate_heap`. It is the loop `tests/preempt.rs` uses to pin that a
%%% non-allocating loop IS pre-empted on the interpreter tier (through the
%%% per-instruction `reduce` charge, not through any heap-admission point). The
%%% residual limit is the native baseline tier, whose compiled blocks never call
%%% `reduce` mid-block and so charge only at a deopt / heap-admission boundary.
%%%
%%% `quick/0` is short and non-allocating: one slice is more than enough for it to
%%% return, so it is the process a budget-ignoring runner could never let run while
%%% a first, CPU-bound process monopolises the scheduler.
-module(preempt).
-export([alloc_loop/1, quick/0, spin/1, spin/2]).

alloc_loop(0, Acc) -> Acc;
alloc_loop(N, Acc) -> alloc_loop(N - 1, [N | Acc]).

alloc_loop(N) -> alloc_loop(N, []).

spin(0, Count) -> Count;
spin(N, Count) -> spin(N - 1, Count + 1).

spin(N) -> spin(N, 0).

quick() -> done.
