%%% Oracle fixture for the `executable_line` opcode (183).
%%%
%%% Compiled with `+line_coverage`, the compiler emits an `executable_line/2`
%%% instruction for every executable source line (used by `cover`/`eunit` to map
%%% an instruction index to a line); the loader lowers it to
%%% `Instr::ExecutableLine`.
%%%
%%% See `tests/fixtures/README.md` for the exact regeneration command.
-module(linecov).
-export([f/1, g/0]).

f(X) when X > 0 -> X;
f(_) -> 0.

g() ->
    A = 1,
    B = 2,
    A + B.
