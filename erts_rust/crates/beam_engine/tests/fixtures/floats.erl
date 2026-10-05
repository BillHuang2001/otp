%% Test fixture for the `beam_engine` floating-point instruction family.
%%
%% It exercises every opcode the family owns — `fmove`, `fconv`, `fadd`, `fsub`,
%% `fmul`, `fdiv` and `fnegate` — plus the float-register (FR) file pressure that
%% makes the compiler allocate several FR temporaries.
%%
%% The guards are deliberate: the compiler only emits the dedicated float opcodes
%% once type analysis proves the operands are floats (`is_float/1` in a guard does
%% that), so `add(A, B) -> A + B.` would compile to `gc_bif '+'` and never reach
%% this family. `beam_disasm` over the compiled fixture confirms the opcode mix.
-module(floats).

-export([add/2, sub/2, mul/2, divide/2, negate/1, mixed/1, temps/4, conv/1, scale/2]).

%% Binary arithmetic: `fmove {x,N} {fr,N}` twice, one float opcode, then
%% `fmove {fr,0} {x,0}` to box the result.
add(A, B) when is_float(A), is_float(B) -> A + B.

sub(A, B) when is_float(A), is_float(B) -> A - B.

mul(A, B) when is_float(A), is_float(B) -> A * B.

%% A zero divisor is a non-finite result -> `badarith` (the opcode carries no
%% fail label).
divide(A, B) when is_float(A), is_float(B) -> A / B.

negate(A) when is_float(A) -> -A.

%% A float *literal* source for `fmove` (`{float,2.0}`) plus two FR temps.
mixed(A) -> A * 2.0 + 1.0.

%% Four FR temporaries (FR0..FR3) — the FR-file pressure case.
temps(A, B, C, D) when is_float(A), is_float(B), is_float(C), is_float(D) ->
    A * B + C * D - A.

%% `fconv` from an untyped argument (the compiler emits `fconv {x,0} {fr,0}`
%% without a type test) followed by a float literal.
conv(I) -> I * 1.0.

%% `fconv` from a provably-integer argument.
scale(I, F) when is_integer(I), is_float(F) -> I * F.
