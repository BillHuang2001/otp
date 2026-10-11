# `beam_engine::families::funs` — fun / closure instruction family

## Intent
Owns the closure and fun-application opcodes of the typed-IR engine:
`Instr::MakeFun3`, `CallFun`, `CallFun2`, `Apply`, `ApplyLast` (ownership table:
`../mod.rs:146-150`).
Builds local closures and dispatches fun/`M:F` application; `call_fun`/`call_fun2` are the
**closure-apply** seam (`ExecutionContext::resolve_local_fun`) and `apply`/`apply_last` are the
**dynamic-entry** point of the cross-module-call seam.
Ground truth (read-only, never modify): `emu/instrs.tab` (`i_make_fun3` :545,
`i_call_fun`/`i_call_fun_last` :363/:371, `apply`/`apply_last` :298/:306) and
`beam_common.c` (`call_fun` :1604, `fixed_apply` :1528, `apply` :1433).

## API Surface (`mod.rs`)
- `pub(crate) fn exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>` — `mod.rs:123`; the per-family seam contract (`Ok(Some)` executed / `Ok(None)` not my variant / `Err` raised).
- `mod.rs:275` `register_args` — collect `x(0)..x(arity-1)`.
- `mod.rs:413` `apply_target` — the `fixed_apply` shape (`module=x(arity)`, `function=x(arity+1)`); its host `call_bif` `Err` is routed through `Machine::raise_bif_error` (`mod.rs:464-467`) and returned as `ApplyTarget::Raised`, never a raw error.
- `mod.rs:369` `enum ApplyTarget { Called(Eterm), Enter{context,func,pc}, Raised(Step) }` — `Raised` covers both a non-atom-target `badarg` and a host BIF error from the applied target.
- `mod.rs:488` `raise_badarg` — catchable `badarg` raise for a non-atom `apply` target.
- `mod.rs:507` `atom_name` — atom-index → name via the running module's table (typed `BadArg` otherwise).
- `MakeFun3` (`:135`) builds via `terms::make_fun_rooted`; header arity = `descriptor.arity - num_free`.
- `CallFun`/`CallFun2` (`:171`/`:181`) route through the in-machine closure-apply seam `ExecutionContext::resolve_local_fun` **first** (see below); `ExecutionContext::apply_fun` is the value-returning fallback when the host declines.

## The in-machine closure-apply seam (`CallFun`/`CallFun2` → `resolve_local_fun`)
- `CallFun`/`CallFun2` dispatch through the shared `call_fun` (`mod.rs:313`), which consults `ExecutionContext::resolve_local_fun` **FIRST** (`mod.rs:323`). On `Ok(Some((callee, label)))` the engine enters the lambda body **in this machine**, exactly as C's `call_fun` dispatches to the fun body's code in the same process:
  - it copies the closure's free variables into `x(arity..arity + num_free)` (C's `reg[i + arity] = funp->env[i]` order — arguments first, captures after) (`mod.rs:329-333`);
  - it registers `callee` as a module context and resolves `label` against that module's **own** label map, so an unknown label is `error::unknown_label`, never a jump into the wrong module (`mod.rs:334-338`);
  - it stores the caller's next instruction `Continuation::Address { func, pc: pc + 1, context: current_context() }` in the live frame and reports `Step::Enter` (`mod.rs:342-351`).
  A `receive` inside the closure then suspends **this** process (an ordinary `RunOutcome::Wait`), and a raise inside it lands in the caller's live `try`/`catch`.
- `Ok(None)` (the seam's default — a host without the seam) keeps the value-returning `ExecutionContext::apply_fun` fallback byte-identical (`mod.rs:355-363`); the fun kinds with no local label (an external `fun M:F/A` and the `-eval` evaluator's closure) take that fallback.
- Every error — the seam's own (`mod.rs:364`) and the fallback's (`mod.rs:362`) — is routed through the catch-aware funnel `Machine::raise_bif_error`, so a routable raise lands in a live `try`/`catch`; a genuine capability gap (`error::unsupported`) stays the uncatchable typed error.
- The seam's `Step::Enter` is folded by the module-context-aware driver exactly like the cross-module-call seam below (switch context + re-bind the literal area); a `Step::Wait` from inside the closure leaves its module context for `resume_outcome`.

## The cross-module call seam (load-bearing — verify here, do not trust summaries)
- `apply`/`apply_last` resolve their target through `ExecutionContext::resolve_external_call` **FIRST** (`mod.rs:441`), which answers with the callee's **shared `Arc<Module>`** + its export entry `Label`. `apply_target` registers that module as a context and returns `ApplyTarget::Enter{context,func,pc}` (`mod.rs:442-447`). Only the seam's `Ok(None)` default falls back to the synchronous `ctx.call_bif` (`mod.rs:464-466`) → `ApplyTarget::Called`; a **host BIF error** there is routed through `Machine::raise_bif_error` and returned as `ApplyTarget::Raised` (`mod.rs:466`).
- `Instr::Apply`'s `Enter` arm stores the **caller's** return address `(func, pc+1, current_context)` in the live frame, then returns `Step::Enter` (`mod.rs:209-224`) — the same discipline as a local `Instr::Call`, so a `receive` inside the callee suspends **this** process and its `return` unwinds into the caller's frame.
- `Instr::ApplyLast`'s `Enter` arm returns `Step::Enter` after the frame is already popped (`mod.rs:243`, `:262-264`), so the callee runs on the caller's frame.
- The seam's `Step::Enter` is folded by the module-context-aware driver (`../../../src/machine/drive.rs:140-158`: switch context + re-bind the literal area); the callee's `Step::Wait` becomes `RunOutcome::Wait` (`drive.rs:159-161`), leaving the suspended frame's module context for `resume_outcome`.
- A `raise` from a cross-module callee lands in the **caller's** catch frame: the landing is reported as `Step::Enter` into the handler's context (see `../exceptions/tests/cross_module.rs:578`, whose `guarded_caller` builds the `proc_lib:init_p_do_apply/3` `try apply/3 catch ... end` shape).
- The `Instr::CallExt*` core arms (NOT this family) use the same seam: each calls `resolve_call_ext_apply` (the `erlang:apply/3` special case, `dispatch.rs:689`) and then `resolve_external_entry` → `resolve_external_call` (`dispatch.rs:361-379` for `CallExt`, `:400-408` for `CallExtOnly`, `:455-464` for `CallExtLast`; `resolve_external_entry` at `dispatch.rs:624-645`); `apply_import_target` (`dispatch.rs:778`) applies the arm's own `ImportFrame` discipline to the resolved target.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; this node has no `unsafe`.
- Errors are `beam_common::{Result, BeamError}` only — never a panic, never a wrong result; a non-atom target is the C's catchable `badarg` raise (`raise_badarg`), or the typed `BeamError::BadArg` when no catch frame / the `badarg` atom is absent.
- A **host BIF error** from an applied target (`apply_target`'s `call_bif` `Err`) is likewise the C's **catchable** raise: it is routed through `Machine::raise_bif_error` into the caller's live `try`/`catch` (`ApplyTarget::Raised`), matching C's `$HANDLE_APPLY_ERROR` → `handle_error` → `next_catch`. Only engine-diagnostic `Err`s from *outside* the host BIF call (a bad register, the resolver / label resolution) keep their `?` and escape as the typed error.
- Term construction/inspection only via `crate::terms` / `beam_term`.

## Test Strategy (`tests.rs`, hand-built synthetic modules; the loader is not involved)
- `MakeFun3`: `make_fun3_builds_a_closure_with_the_descriptor_fields` (:128), `..._with_no_free_variables` (:166), `..._with_a_missing_descriptor_is_a_typed_error` (:196), `..._with_a_free_variable_count_mismatch_is_a_typed_error` (:222).
- `call_fun`/`call_fun2`: `call_fun_dispatches_the_fun_in_x_arity` (:254), `call_fun_without_an_apply_hook_is_a_typed_unsupported` (:279), `call_fun_with_an_out_of_range_fun_register_is_a_typed_error` (:301), `call_fun2_dispatches_the_fun_in_its_operand` (:324).
- `apply` synchronous + badarg raise: `apply_dispatches_module_and_function_from_x_arity` (:373), `apply_with_a_non_atom_module_is_a_typed_error` (:400), `apply_badarg_lands_in_a_live_catch_frame` (:428), `apply_last_badarg_lands_in_a_live_catch_frame` (:466), `apply_badarg_without_a_catch_frame_is_the_typed_error` (:496), `apply_badarg_without_the_badarg_atom_falls_back_to_the_typed_error` (:521).
- `apply_last` frame/continuation: `apply_last_pops_the_frame_and_returns_through_the_continuation` (:547), `apply_last_reports_a_context_switch_when_the_continuation_is_in_another_module` (:615), `apply_last_at_the_bottom_frame_finishes_the_run` (:675).
- **Cross-module seam** (the key coverage): `apply_enters_a_resolved_callee_in_the_same_machine` (:870), `apply_into_a_cross_module_receive_suspends_and_resumes` (:925 — drives `run_label_outcome`→`RunOutcome::Wait{0,0}` then a send + `resume_outcome`→`RunOutcome::Returned`), `apply_without_the_seam_keeps_the_synchronous_bif_path` (:962), `apply_last_pops_its_frame_then_enters_a_resolved_callee` (:1009).
- Router declination: `non_fun_variants_return_none` (:721).
- The `TestMailbox` stub (`:1429`) implements the receive seam for the cross-module receive test; markers are the typed `unsupported`.

## Known Issues / Notes for Agents
- **No TODO/FIXME/limitation marker in this node** — the cross-module seam is implemented on both the `Apply`/`ApplyLast` path here and the `CallExt*` path (`interpreter/dispatch.rs`), and adopted by the `beam_rt` host (`RuntimeContext::resolve_external_call`).
- **A raise/receive from a cross-module callee IS propagated, not folded into `blocked()`.** Each `Instr::CallExt*` arm resolves the seam before the synchronous `call_bif` (`interpreter/dispatch.rs:361-379` for `CallExt`), so a callee suspend or raise reaches the driver. `blocked()` is only the **non-blocking** entries' report for a `receive` with no message (`crate::error::blocked`); a host that pumps (beam_rt) or a caller driving `resume_outcome` gets `RunOutcome::Wait`.
- Non-zero `wait_timeout` has **no host timer** — it suspends like `wait` and resumes only on a message (`../receive.rs:147-163`, `../CONTEXT.md` Known Issues). Not cross-module-specific.

## See Also
- `../CONTEXT.md` → the family seam router + ownership + capability-gap list.
- `../../../CONTEXT.md` → the crate API surface (the `ExecutionContext` seam, Handoff #10 cross-module call, Handoff #1 receive/cross-module suspension).
- `../exceptions/tests/cross_module.rs` → the raise-side containment tests.
- `../../../src/machine/drive.rs` → how `Step::Enter`/`Step::Wait` are folded.
