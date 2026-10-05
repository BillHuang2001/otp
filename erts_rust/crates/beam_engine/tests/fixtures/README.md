# `beam_engine` test fixtures

`floats.beam` is the committed oracle fixture for the floating-point instruction
family (`src/families/floats.rs`), used by `tests/floats.rs`. Like the
`beam_loader` fixtures it is a **real `.beam`** produced by the system Erlang
toolchain (OTP 29 / `erts-17.1`), which is also the differential oracle.

## Regeneration

Build it from a *neutral* directory so no worktree path is baked into the
committed binary (the `CInf`/`Dbgi` chunks record the source path):

```sh
mkdir -p "$TMPDIR/floatsgen"
cp erts_rust/crates/beam_engine/tests/fixtures/floats.erl "$TMPDIR/floatsgen/"
env -C "$TMPDIR/floatsgen" erlc floats.erl
cp "$TMPDIR/floatsgen/floats.beam" erts_rust/crates/beam_engine/tests/fixtures/floats.beam
```

Any byte change in the fixture invalidates the expected values in `tests/floats.rs`;
re-derive them with `beam_disasm:file("floats.beam")` plus the oracle, do **not**
guess.

## `floats.erl` — what it covers

Ordinary Erlang whose compiled output exercises every opcode the family owns. The
`is_float/1` guards are what make the compiler emit the *dedicated* float opcodes
(`add(A, B) -> A + B.` without them compiles to `gc_bif '+'` and never reaches the
family).

| function | opcodes exercised |
|---|---|
| `add/2`, `sub/2`, `mul/2`, `divide/2` | `fmove {x,N} {fr,N}` → `fadd`/`fsub`/`fmul`/`fdiv` → `fmove {fr,0} {x,0}` |
| `negate/1` | `fmove`, `fnegate`, `fmove` |
| `mixed/1` (`A * 2.0 + 1.0`) | `fconv`, two `fmove {float,K}` **literals**, `fmul`, `fadd` |
| `temps/4` (`A*B + C*D - A`) | four FR temporaries (FR0..FR3), `fmul`×2, `fadd`, `fsub` |
| `conv/1` (`I * 1.0`) | `fconv` from an untyped register, a float literal, `fmul` |
| `scale/2` | `fconv` from a provably-integer register next to an `fmove` |

`divide/2` with a zero divisor is a **non-finite** result, so it is the fixture's
`badarith` case (`fdiv` carries no fail label); `conv([])` is the `fconv`
non-numeric case. Both were checked against the oracle:

```
1> floats:divide(1.0, 0.0).
** exception error: an error occurred when evaluating an arithmetic expression
2> floats:conv([]).
** exception error: an error occurred when evaluating an arithmetic expression
```

Note `add(1, 2)` (an integer argument) hits the failed `is_float` guard, which jumps
to the function's `func_info` label — the oracle raises `function_clause` there, but
the M1 interpreter treats `func_info` as a no-op, so that path is deliberately *not*
exercised by `tests/floats.rs`.
