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
  = executed, `Ok(None)` = not this family's variant, `Err` = raised. Every owned
  opcode is position-independent, so `func`/`pc` are unused (kept because the
  signature is the frozen family-router contract).
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
- `callee.rs`: the synthesized **callee head item** of an `undef` raise —
  `{CalleeModule, CalleeFunction, ArgList, []}` (`is_unresolved_undef`,
  `import_callee_atoms`, `import_callee_head_item`, `callee_head_item`). Real OTP names an
  unresolvable call target **only** through the cooked trace's innermost item (with the
  *argument list*), and `logger_server:call_h/4` matches on exactly that shape, but the
  engine's raise snapshot is `(context, func, pc)` metadata and can only emit an arity — so
  the failing call sites (`call_ext`'s import table + `x(0..arity)`; the `apply` register
  shape) materialize the one item the snapshot cannot. The item is built in **one** rooted
  reservation (`2*argc + 5` words) through `terms::reserve_slack` + the `Sink`, reading the
  callee's **real** arguments from `x(0..argc)` *after* it (reserve → read → write). It is
  built only for a bare `undef`, only with a live catch frame, and only when the arguments
  are readable — otherwise `Ok(None)` and today's unmodified raise. `Location` is the empty
  list (the callee does not exist, so there is no source position).
- `stacktrace.rs`: `symbolic_stacktrace(machine, module, ctx, head_operand, head)` — cooks
  the machine's raise-time snapshot into the symbolic `[{M,F,A,Location}]` list from
  `Machine::raise_snapshot()` (the **pre-unwind** `(context, func, pc)` chain every
  `Machine::throw_with_trace` rebuilds at raise time: the raising frame first, then every
  live caller innermost-first). **Each frame resolves in its OWN module context**: context
  `0` against the `module` argument, but only when `Machine::current_context()` is also `0`
  (otherwise the handler runs in a callee context and the argument is the *callee's* module,
  so the frame is skipped); a non-zero context `id` through `Machine::module_context(id)`. A
  frame whose context cannot be resolved is **skipped** (best-effort metadata, like the
  uncaught path's `Machine::raise_stacktrace`); a frame whose module IS resolved but whose
  function index is out of range is a typed `unsupported`. An **empty** snapshot — or one
  whose every frame is skipped — cooks to the **empty list**, the honest answer, unless a
  head item is present. One rooted reservation for the whole node graph
  (`terms::slack`/`terms::sink`); the resolved `Arc<Module>` handles stay alive in a local
  vector across it, because each item shape borrows its module's `Line` chunk. When `head`
  is set the cooking exception carries a synthesized **callee head item** as its raw trace
  (`callee.rs` above): the reservation is sized **`+2`** for one extra cons cell, and the
  head word is re-read **after** the reservation through `head_operand` (the very register
  `build_stacktrace` read the raw trace from) and consed in front of the carved frames, so a
  head-bearing raise cooks to the **one-element** list `[Item]` even over an empty frame
  chain.

## The raise's raw trace: `NIL`, or a synthesized callee head item
C's raw trace is an **opaque object of saved code pointers** (`c_p->ftrace`) with no engine
equivalent, and the oracle's own cooked `S` for `try ... catch C:R:S -> S end` additionally
holds host `init:*` frames that live *outside* this engine's run. A snapshot-cooked trace is
therefore a strictly-**partial** list that would still differ from the oracle's, so landing it
would trade this documented omission for a different, silently-partial value. So on the
compiled path the raw trace word is `NIL` — **except** for the `undef` raised by an
unresolvable `call_ext*`/`apply` target, whose failing call site publishes its synthesized
`{M,F,ArgList,[]}` item as the raw trace (the one item the snapshot cannot express). In both
cases the compiled handler moves the raw trace from `x(2)` into `x(0)`, which is the operand
`build_stacktrace` reads (`Instr::BuildStacktrace`), and the head item is recognised through
the `raise_head_item`/`pending_callee_head` latch pair rather than by inspecting the word.
C's `add_stacktrace` stays omitted and `catch_end` semantics are unchanged.

## The two one-shot latches that carry the head item
The head item is **not** a machine-held heap term: it travels as the raise's own raw-trace
word, so it sits inside the frozen root set (published in `x(2)`, moved to `x(0)` by the
compiled handler) and is relocated in place by every collection.
- `pending_callee_head` is armed by `Machine::raise_bif_error_with_head` →
  `throw_with_trace_headed` **immediately before** the `throw_with_trace` that actually
  lands, never at the call site (a raise with no live catch frame propagates as a bare `Err`
  and never reaches `throw_with_trace`, so the head is dropped and the latch stays clear).
- `Machine::throw_with_trace`'s **first** act is
  `raise_head_item = std::mem::take(&mut pending_callee_head)`, so `raise_head_item` always
  describes the raise that landed. `Instr::BuildStacktrace` consumes it once via
  `Machine::take_raise_head_item()`; a handler clause that ignores `S` never reaches
  `build_stacktrace`, so the latch is still set when the *next* raise lands and is overwritten
  there — it can never leak into another raise. `Machine::reset` clears both.

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
- `callee.rs` — the synthesized callee head item from a hand-built `Module` + a `BifHook`:
  the built item's shape/fields, the `Ok(None)` gates (not a bare `undef`, no live catch
  frame, unreadable argument register) and the latch hygiene (`pending_callee_head` /
  `raise_head_item` armed only by a landing `throw_with_trace`, cleared by `reset`).
- `oracle.rs` — the same mechanism end to end on a **real `erlc`-compiled `.beam`**
  (`p3`, loaded through `beam_loader::load_file`) and cross-checked against the **system
  oracle**: a direct `call_ext` to a missing function of an existing module, both compiled
  `apply/3` routes (the `call_ext erlang:apply/3` encoding and the BEAM `apply`
  instruction), a missing module, the latch-hygiene pair (an `undef` whose handler ignores
  `S`, then an unrelated raise that uses `S`), a head-less raise, the compiler's
  `move {x,2} -> {x,0}` + `build_stacktrace` handler shape (pinned on the loaded IR *and*
  on `erlc -S` text), and **tier equivalence** of the cooked head item on
  `Tier::Interpreter` vs `Tier::BaselineJit`. Every expectation is measurement-derived and
  each test **skips loudly** when `erlc`/`erl` are absent (the discipline of
  [`crate::machine::deadline_tests`]). The engine's cooked list is asserted to be a
  **non-empty prefix** of the oracle's (host `erl_eval`/`init` callers lie outside the run).
- `raising.rs` — the raising opcodes' reason expansion (caught and uncaught), the
  `raise` re-raise opcode, `raw_raise`'s class protocol and `badarg` fall-through.
- `stacktrace.rs` — `build_stacktrace`: the `NIL` raw trace (`NIL` out), the
  raise-snapshot ordering (a frame the raise *unwound* is still present, proving the
  snapshot is cooked rather than the live stack), a snapshot frame resolved in its own
  module context (with a context-`0` frame skipped from a non-zero handler context),
  the source-location item, and the typed errors.
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
