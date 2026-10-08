%%% Oracle fixture for the `debug_line` opcode (184).
%%%
%%% Compiled with `+beam_debug_info`, the compiler emits a `debug_line/4`
%%% instruction (the breakpoint/live-register marker used by the debugger and
%%% `beam_debug_info`) before code for a source line; the loader lowers it to
%%% `Instr::DebugLine`.
%%%
%%% See `tests/fixtures/README.md` for the exact regeneration command.
-module(dbgline).
-export([f/1]).

f(X) ->
    Y = X + 1,
    Y * 2.
