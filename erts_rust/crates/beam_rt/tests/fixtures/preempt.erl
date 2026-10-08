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
%%% `alloc_loop/2` is an ALLOCATING tail-recursive loop: every iteration builds one
%%% cons cell (`[N | Acc]`), so the compiler emits a `test_heap` before that
%%% allocation. That is the one per-instruction seam the runtime's reduction budget
%%% is charged at (`beam_rt/src/runner.rs`), so a call with an `N` far above the
%%% 4000-reduction slice budget is pre-empted (yielded) part-way through and only
%%% finishes over several slices. A loop with **no** allocation has no admission
%%% point and is not pre-empted (an honest, documented Milestone-1 limit).
%%%
%%% `quick/0` is short and non-allocating: one slice is more than enough for it to
%%% return, so it is the process a budget-ignoring runner could never let run while
%%% a first, CPU-bound process monopolises the scheduler.
-module(preempt).
-export([alloc_loop/1, quick/0]).

alloc_loop(0, Acc) -> Acc;
alloc_loop(N, Acc) -> alloc_loop(N - 1, [N | Acc]).

alloc_loop(N) -> alloc_loop(N, []).

quick() -> done.
