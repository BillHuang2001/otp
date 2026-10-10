# `beam_engine/src/families/exceptions` — the exception / raise instruction family

## Intent
Owns the in-VM **exception** instruction surface and the machine-side *handler*
opcodes (the pushing/popping/folding half of the catch-frame protocol; the
raise/unwind half lives in `src/machine/catch.rs`, which this family calls through
`Machine::{throw, throw_error, throw_with_trace}` — the one uniform raise entry
point).

Read `../../CONTEXT.md` first: it holds the crate-level FROZEN API surface, the
execution model (frames, dispatch, raise + catch-frame semantics) and the handoffs.
The per-opcode semantics and the full M1 gap list are in `../../INSTRUCTION_SURFACE.md`.

## API Surface
- `exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>` —
  the family's entry point, called by the `families::dispatch` router (which routes
  by the instruction's own `Instr` variant, so ownership is disjoint). `Ok(Some(step))`
  = executed, `Ok(None)` = not this family's variant, `Err` = raised.
- Opcodes owned: `try`/`catch` (push a catch frame), `try_case`/`try_end`/`catch_end`
  (the handler opcodes; each consumes exactly one frame), `badmatch` ⇒ `{badmatch,V}`,
  `case_end` ⇒ `{case_clause,V}`, `try_case_end` ⇒ `{try_clause,V}`, `badrecord` ⇒
  `{badrecord,V}`, `if_end` ⇒ the bare atom `if_clause`, `raise Trace Value` (re-raise
  with `Machine::pending_class`), `raw_raise` (the x(0)=class / x(1)=value /
  x(2)=trace protocol, incl. the "unknown class" `badarg` fall-through), and
  `build_stacktrace`.
- `raise.rs` (private helper module): `raise_error_tuple` / `raise_error_atom` (reason
  construction through the **rooted** builders, so a moving collector can run under the
  reserve), `install_class_atom` (never substitutes an atom the module cannot name) and
  `classify_class` (`raw_raise`'s class protocol).
- `stacktrace.rs`: `symbolic_stacktrace` — cooks a non-`NIL` raw stacktrace from the
  machine's **live call frames** (M1 keeps no raw `StackTrace` object). One rooted
  reservation for the whole node graph (`terms::slack`/`terms::sink`); `NIL` cooks to
  `NIL`, exactly as C does. Its non-`NIL` arm is **unreachable from compiler output
  today**: every engine raise lands with `x(2) = NIL` (`Machine::throw`/`throw_error`/
  `throw_with_trace` always pass `NIL`; the two forwarding sites `raise Trace Value` and
  `raw_raise` forward a value that is itself `NIL` inductively), so only the synthetic
  `tests/stacktrace.rs` fixtures reach it. It also resolves **every** frame against the
  single `module` argument (not each frame's own module context, unlike the uncaught
  path's `Machine::raise_stacktrace`) — dormant while the arm is unreachable.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only. An unsupported case is a typed
  `error::unsupported`; **never** a panic, a substituted atom, or a wrong result.
- Term construction/inspection is only `beam_term` + the crate's `terms` builders;
  the **rooted** twins (`*_rooted`) are what the instruction bodies here call.
- `try_case`/`catch_end` require the C landing convention `x(0) = THE_NON_VALUE`,
  `x(1) = reason`, `x(2) = trace`, `x(3) = class atom`; the handler opcode that runs
  next pops the frame — a raise *reads* the innermost frame but never pops it.
- **Cross-module:** a raise landing in a catch frame stamped with another module
  context is reported as `Step::Enter { context, func, pc }` (not `Step::Goto`) so the
  driver switches module context and re-binds the literal area. The family relies on
  `Machine::throw` (its `throw_with_trace` entry) for that decision; the symbolic frames
  of an *uncaught* raise are attached separately by the `Machine::with_uncaught_frames`
  funnel at the `step_instruction`/`drive` boundary. It re-derives neither.

## Test Strategy
Unit tests are in-module, split by responsibility under `tests/` (the module tree is
declared in `tests.rs`, which also holds the shared scaffolding: the `Fixture` module
builder, `no_labels`, `exec_one` and `assert_reason_tuple`; several submodules alias
`crate::terms::test_support as terms` for the ergonomic test-only builders).
- `handlers.rs` — the `try`/`catch` frame push, the handler opcodes, the `catch_end`
  fold (throw class, error/exit `{'EXIT',Reason}`, the missing-`EXIT`-atom typed error).
- `raising.rs` — the raising opcodes' reason expansion (caught and uncaught), the
  `raise` re-raise opcode, `raw_raise`'s class protocol and `badarg` fall-through.
- `stacktrace.rs` — `build_stacktrace`, incl. the NIL raw trace and the typed errors.
- `end_to_end.rs` — whole synthetic sequences.
- `cross_module.rs` — **cross-module containment**: one shared `beam_term::AtomTable`
  (`Universe`) so exception *class* atoms compare equal across modules, modules entered
  through `MinimalContext::with_external_resolver` (the `resolve_external_call` seam),
  and expectations taken from the system oracle (`erl`, OTP 29 / erts-17.1). Covers the
  `Apply` seam, `try ... of ... catch` (normal path, contained raise, the *uncaught*
  failing `of` clause), nested catches with an inner re-raise landing in an outer handler
  in another module context, a `throw` crossing two module boundaries (A → B → C), and
  compiler-generated raises (`badmatch`/`case_end`/`try_case_end`) contained across a
  module boundary. Each test asserts the exact landed value, the final
  `frame_depth`/`catch_frame_depth`/`current_context`, and the `RunOutcome`.
  `Universe::assert_sealed` fails loudly if a fixture `intern`s a name outside the shared
  table (that would give one name two different atom indices in two modules, which is the
  very divergence the shared table exists to rule out).

Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.

## See Also
- `../../machine/catch.rs` → the catch-frame stack, `throw`/`throw_with_trace`,
  `with_uncaught_frames`/`raise_stacktrace`, `land_exception`, `unwind_to_frame` (the raise
  half of this protocol).
- `../../CONTEXT.md` → crate contracts, the frozen seams, the execution model.
- `../../INSTRUCTION_SURFACE.md` → per-opcode semantics and the complete gap list.
