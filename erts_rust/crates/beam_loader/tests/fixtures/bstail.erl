%%% Oracle fixture for the `bs_test_tail2` opcode (121).
%%%
%%% By default the `ssa_opt_bs_ensure` optimization pass rewrites a `bs_test_tail`
%%% into a `bs_match` `test_tail`/`ensure_exactly` command, so the generic opcode
%%% `bs_test_tail2/3` never reaches a `.beam`. Compiling with `+no_ssa_opt`
%%% disables that pass (`beam_ssa_codegen.erl` `NoBsMatch` path), so the standalone
%%% `bs_test_tail2 Fail Ctx Bits` instruction is emitted; the loader lowers it to
%%% `Instr::BsTestTail2`.
%%%
%%% The pattern `<<X:8>>` matches a binary of exactly one byte, which makes the
%%% compiler emit `bs_test_tail2` with `Bits = 0` after the 8-bit integer read.
%%%
%%% See `tests/fixtures/README.md` for the exact regeneration command.
-module(bstail).
-export([f/1]).

f(<<X:8>>) -> X.
