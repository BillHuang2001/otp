# `beam_engine/src/families/floats` — the floating-point instruction family

## Intent
Owns the float-register (FR) opcodes: `fmove`/`fconv` (copy + conversion) and the arithmetic
`fadd`/`fsub`/`fmul`/`fdiv`/`fnegate`. It reads and writes the machine's FR file, converts
small ints and bignums to `f64`, and turns every arithmetic failure into either the instruction's
`fail` branch or the `badarith` raise.

Read `../CONTEXT.md` first: it holds the family seam's contract (`exec(...) -> Result<Option<Step>>`,
`Ok(Some)` = executed, `Ok(None)` = not this variant, `Err` = raised), the disjoint
variant-ownership rule and the shared capability-gap list. The crate-level execution model is in
`../../CONTEXT.md`, the per-opcode semantics in `../../../INSTRUCTION_SURFACE.md`.

## Owned variants (7 — routed here by `../mod.rs::dispatch`)
| Variant | C body | Semantics |
|---|---|---|
| `Fmove` | `fload`/`fstore` (`emu/ops.tab:1337-1338`) | copy a float between an FR slot and a value register / float literal |
| `Fconv` | `i_fconv` (`emu/float_instrs.tab:38-52`) | small int / float / bignum (`big_to_f64`) → FR; otherwise `badarith` |
| `Fadd`/`Fsub`/`Fmul`/`Fdiv` | `i_f{add,sub,mul,div}` (`:62-76`) | `Dst = Src1 op Src2`; a **non-finite** result is `badarith` |
| `Fnegate` | `i_fnegate` (`:78-84`) | `Dst = -Src`; no finiteness check (negating a finite float is finite) |

## Routing Table
- `mod.rs` — `exec` (the dispatch), the shared operand seam `read_value` (a bare
  `Operand::Literal` is the `fmove {float,K}` source, resolved from the module's decoded literal
  area; everything else goes through `read_operand`), and the shared failure semantics
  `arithmetic_failure` (fail-branch vs raise) + `raise_badarith`.
- `arith.rs` — the four binary bodies (one shared `binary`) and `negate`.
- `convert.rs` — `fmove` and `fconv` (its private `Conv` enum is C `i_fconv`'s success/failure).
- `tests.rs` — the per-variant unit suite (see Test Strategy).

## Representation: the FR file holds `Eterm` term slots
The C FR file is raw doubles (`FloatDef f_reg_array[MAX_REG]`). Here the machine's FR file holds
**`Eterm` term slots** (`Machine::get_fr`/`set_fr`), i.e. a slot holds a *boxed float term*, so
every read converts the slot with `crate::terms::float_value` and every write stores a boxed float
built by `crate::terms::make_float_rooted`. A malformed module's non-float slot is therefore
detected as the arithmetic failure rather than being reinterpreted as bits. This file is the
family that makes the substitution observable, so the `RootSet` the machine publishes includes the
whole FR file (`../../CONTEXT.md` §"The GC root-set contract").

## Failure semantics (never a panic, never a fabricated value)
- A **non-float FR slot** or a **non-finite result** (including `x/0.0` ⇒ `inf` and `0.0/0.0` ⇒
  `NaN`) is the instruction's *arithmetic failure*: a live `fail` label makes it a guard branch
  (`Step::Goto`), `NO_FAIL` makes it the `badarith` **raise**. C's `$BADARITH0`
  (`emu/macros.tab:285-288`) goes `goto find_func_info` → `handle_error` → `next_catch`, so a live
  `try`/`catch` catches it; the engine reaches that through `Machine::throw_error`. The compiler
  only emits `NO_FAIL` for these opcodes, so the live-label branch is engine-side robustness.
- `Fconv` carries **no** fail label: a non-numeric source and a bignum that is not a finite double
  (`big_to_f64`'s C `< 0` failure: `bitlen > 1024`, or a rounding up to `2^1024`) both raise
  `badarith`. A representable bignum converts to the nearest double, ties to even, exactly as the
  oracle does.
- **The raise tag and class atoms resolve through `crate::machine::namespace_atom`** — the host's
  live atom namespace (`ExecutionContext::atoms`) first, the running module's load-time snapshot
  second. A namespace that can name **neither** `badarith` nor (for the class) `error` leaves the
  typed `BeamError::BadArith` / an uninstalled class atom: the engine never substitutes an atom
  index it did not read from the table using it.
- An operand the seam cannot evaluate (out-of-range register, unsupported operand kind) propagates
  unchanged — a malformed/unsupported program, deliberately **not** masked as `badarith`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; no crate-local error enum, no panic, no
  `unwrap()` on loaded data, no wrong result.
- Term construction/inspection is **only** `beam_term`/`crate::terms` (float bits via
  `make_float_rooted`/`float_value`, bignum conversion via `beam_term::number::big_to_f64`); no
  float layout is re-derived here. The family is pure safe Rust (no `#[allow(unsafe_code)]`).
- Files ~1000 lines; split by responsibility (`mod.rs` seam + failure semantics, `arith.rs`,
  `convert.rs`, `tests.rs`).

## Test Strategy
`tests.rs` drives every float instruction straight through `exec` over a `Machine` + the
`MinimalContext` (no loader involved), with a `module_with_raise_atoms` fixture for the catch
cases, and adds two end-to-end programs through `Machine::run_export`:
- success paths — `fmove` both directions, `fconv` of a small int / a float / a representable
  bignum / a small int beyond `2^53` (nearest-double rounding), the four binary ops, `fnegate`;
- failure paths — division by zero, a non-finite result, a non-float operand, `fconv` of a
  non-number, the typed errors for an out-of-range FR slot and a missing literal, and the
  non-`badarith` propagation of a non-register operand;
- the raise's catchability — landing in a live catch frame with the C convention in `x(0..=3)`,
  the live-`fail`-label branch instead of a raise, and the typed `BeamError::BadArith` when no
  catch frame is live;
- the seam contract — `Ok(None)` for an instruction no float variant claims.

Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.

## See Also
- `../CONTEXT.md` → the family seam, ownership rule, `NO_FAIL` route, capability gaps.
- `../../machine/` → the FR file (`get_fr`/`set_fr`/`fr_mark`) and `throw_error`.
- `../../../terms/` → `make_float_rooted`/`float_value`, `test_support::{make_float, make_integer}`.
- `../../../../../beam_term/src/number.rs` → `big_to_f64`.
- `../../../../../../erts/emulator/beam/emu/float_instrs.tab` + `big.c:2412` → the C ground truth.
