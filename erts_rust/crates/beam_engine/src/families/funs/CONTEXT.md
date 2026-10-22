# `beam_engine::families::funs` — fun / closure instruction family

## Intent
Owns the closure and fun-application opcodes of the typed-IR engine:
`Instr::MakeFun3`, `CallFun`, `CallFun2`, `Apply`, `ApplyLast` (ownership table:
`../mod.rs:166-171`).
Builds local closures and dispatches fun/`M:F` application; `call_fun`/`call_fun2` are the
**closure-apply** seam (`ExecutionContext::resolve_local_fun`) and `apply`/`apply_last` are the
**dynamic-entry** point of the cross-module-call seam.
Ground truth (read-only, never modify): `emu/instrs.tab` (`i_make_fun3` :545,
`i_call_fun`/`i_call_fun_last` :363/:371, `apply`/`apply_last` :298/:306) and
`beam_common.c` (`call_fun` :1604, `fixed_apply` :1528, `apply` :1433, `expand_error_value` :754).

## API Surface (`mod.rs`)
- `pub(crate) fn exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>` — `mod.rs:140`; the per-family seam contract (`Ok(Some)` executed / `Ok(None)` not my variant / `Err` raised).
- `mod.rs:304` `register_args` — collect `x(0)..x(arity-1)`.
- `mod.rs:671` `apply_target` — the `fixed_apply` shape (`module=x(arity)`, `function=x(arity+1)`); its host `call_bif` `Err` is routed through `Machine::raise_bif_error_with_head` (`mod.rs:766-775`) and returned as `ApplyTarget::Raised`, never a raw error.
- `mod.rs:627` `enum ApplyTarget { Called(Eterm), Enter{context,func,pc}, Raised(Step) }` — `Raised` covers both a non-atom-target `badarg` and a host BIF error from the applied target.
- `mod.rs:796` `raise_badarg` — catchable `badarg` raise for a non-atom `apply` target.
- `mod.rs:825` `atom_name` — resolves a runtime atom **value** through `ExecutionContext::atom_name` (the live-namespace seam, defaulting to the running module's own table); typed `BadArg` for a non-atom or an id the host cannot name.
- `MakeFun3` (`:153`) builds via `terms::make_fun_rooted`; header arity = `descriptor.arity - num_free`.
- `CallFun`/`CallFun2` (`:194`/`:206`) hand a re-readable `FunSource` (`:320`) to the shared `call_fun` (`:519`), which classifies the target's shape and **then** routes through the in-machine closure-apply seam `ExecutionContext::resolve_local_fun` (see below); `ExecutionContext::apply_fun` is the value-returning fallback when the host declines.
- `mod.rs:366` `reason_atom` / `:376` `raise_reason` / `:396` `raise_badfun` / `:428` `raise_badarity` — the structured target-shape reasons (see below).

## The structured target-shape reasons (`{badfun,Fun}` / `{badarity,{Fun,Args}}`)
- `call_fun` classifies the call target **before** the seam, exactly as C's `call_fun` (`beam_common.c:1604-1622`) does, so the oracle's structured `error` reasons are reproduced instead of the bare `badarg`/`badarity` a host seam can only report:
  - the target is not a fun ⇒ `{badfun, Fun}` (`EXC_BADFUN` + `expand_error_value`'s `TUPLE2(am_badfun, fvalue)`, `beam_common.c:754-768`), built by `raise_badfun` (`:396`);
  - the target IS a fun but its **header arity** (`terms::fun_arity` — C's `fun_arity(funp)`, the right field for *both* a `make_fun3` closure and an external `fun M:F/A`) differs from the call's arity ⇒ `{badarity, {Fun, Args}}` (`EXC_BADARITY`), `Args` being the **actual** argument list `x(0..arity)`, built by `raise_badarity` (`:428`).
- **The arity test is UNCONDITIONAL; only the structured *reason* depends on the tag atoms.** `call_fun` always
  evaluates `terms::fun_arity(fun)? != arity` and, on a mismatch, always raises through `raise_badarity`. C's
  **global** `error_atom` table always names `badfun`/`badarity`, so C always produces the structured reason; this
  engine's atoms are module-local, so a module that cannot name `badarity` gets the typed
  `unsupported("call_fun: no 'badarity' atom in the module")` — a degraded *diagnostic*, never a skipped check and
  never a substituted atom. Gating the comparison on the lookup would forward a mismatched-arity closure to
  `ExecutionContext::resolve_local_fun`, and a host that does not re-validate would enter the closure body with the
  wrong argument count — a wrong value rather than a typed error. Every `beam_rt` process names both tags
  (`beam_rt::boot::CORE_ATOMS` seeds `badfun`/`badarity`), so the real runtime always takes the structured-reason
  path. Pinned by `tests.rs::{call_fun_with_a_wrong_arity_fun_raises_badarity,
  a_wrong_arity_fun_never_reaches_the_seam_when_the_module_cannot_name_badarity}`.
- The `badfun` half **does** stay gated (`mod.rs`: `raise_badfun` only when the module names `badfun`), and that is
  safe rather than a gap: a non-fun target cannot be resolved by the closure-apply seam at all, so an atom-poor
  module declines the classification and falls through to the seam → the typed `unsupported_opcode: apply_fun`
  instead of a substituted atom — never a wrong value.
- Both reasons ride the machine's structured-reason path (`machine.throw_error`) with class `error` (`raise_reason`, `:376`), so a live `try`/`catch` observes `{error,…}` and an uncaught raise carries the reason term in the raise's live-reason slot.
- Both are ONE rooted reservation: `{badfun,Fun}` = a 2-tuple plus the target's operand slack; `{badarity,{Fun,Args}}` = a 2-tuple + the inner `{Fun,Args}` 2-tuple + `2·arity` list cells + the slack, carved via `Slack::tuple`/`Slack::list` (`raise_badarity`, `:428-452`). The target is re-read from the machine's post-collection state through `FunSource::read_slack` — never held in a Rust local across the reservation.
- The tag atoms (`badfun`/`badarity`) are resolved from the running module's own atom table; ERTS resolves them from its **global** `error_atom` table (neither name is interned by a module's own source — verified with `beam_lib:chunks(F,[atoms])`), and `beam_rt::boot::CORE_ATOMS` seeds both. A module that names the tag raises the structured reason; one that cannot name it reports the typed `unsupported` naming the missing atom — never a substituted index.
- Oracle-verified (OTP 29.1.1 / erts-17.1): `f(F) -> F(1).` with `f(5)` ⇒ `{error,{badfun,5}}`, `f(notafun)` ⇒ `{error,{badfun,notafun}}`; `am1(F,A) -> F(A).` with `am1(fun(X,Y) -> X end, one)` ⇒ `{error,{badarity,{#Fun<…>,[one]}}}`; the external `fun lists:reverse/2` called with one argument ⇒ `{error,{badarity,{fun lists:reverse/2,[one]}}}`, while a matching-arity external fun call is *dispatched* (⇒ `function_clause`) — i.e. the header-arity check is exactly what gates it.

## The in-machine closure-apply seam (`CallFun`/`CallFun2` → `resolve_local_fun`)
- **Every seam-read term is re-derived from the machine after the seam.** `resolve_local_fun`,
  `apply_fun_outcome` and `resolve_external_call` are `&mut dyn ExecutionContext` host calls: a host may
  allocate through a rooted door while serving one, and a **collecting** host relocates exactly the frozen
  `RootSet` (`x[..live]`, the whole Erlang stack, the FR file) — which is where the closure, its captures and
  the call's arguments live. So `call_fun` reads the target through its re-readable source (`FunSource::read`
  — `x(arity)` or the `call_fun2` operand) **again after** the seam, re-collects `args` with
  `register_args(machine, arity)` after it, and `terms::fun_free`'s borrow is taken from the post-seam read
  with nothing allocation-capable between it and the `set_x` writes (a register write is not a heap admission
  point). `apply_target` likewise builds the `call_bif` argument list only **after** the cross-module seam
  declines, and re-reads the callee's two atom words from `x(arity)`/`x(arity+1)` after the dispatch before the
  `undef` head-item builder uses them. Pinned by `tests/fun_seam_rooting.rs` (a semi-space copying mock that
  collects *inside* the seam and poisons the region it vacated).
- `CallFun`/`CallFun2` dispatch through the shared `call_fun` (`mod.rs:519`), which consults
  `ExecutionContext::resolve_local_fun` **FIRST** (`mod.rs:553`) — after the shape check above, so only a
  well-formed, matching-arity target reaches the host. On `Ok(Some((callee, label)))` the engine enters the
  lambda body **in this machine**, exactly as C's `call_fun` dispatches to the fun body's code in the same
  process:
  - it copies the closure's free variables into `x(arity..arity + num_free)` (C's `reg[i + arity] = funp->env[i]` order — arguments first, captures after) (`mod.rs:562-567`);
  - it registers `callee` as a module context and resolves `label` against that module's **own** label map, so an unknown label is `error::unknown_label`, never a jump into the wrong module (`mod.rs:568-572`);
  - it stores the caller's next instruction `Continuation::Address { func, pc: pc + 1, context: current_context() }` in the live frame and reports `Step::Enter` (`mod.rs:576-585`).
  A `receive` inside the closure then suspends **this** process (an ordinary `RunOutcome::Wait`), and a raise inside it lands in the caller's live `try`/`catch`.
- `Ok(None)` (the seam's default — a host without the seam) goes to the outcome-aware `ExecutionContext::apply_fun_outcome` (`mod.rs:607-620`), whose default forwards to the value-returning `ExecutionContext::apply_fun` and wraps the value, so a host without the channel is byte-identical; the fun kinds with no local label (an external `fun M:F/A` and the `-eval` evaluator's closure) take that fallback. That call is handed the **re-derived** pair (the target from its source, the arguments from `x(0..arity)`), never the pre-seam copies. This site IS re-execution-idempotent (pure operand reads; the one write, `set_x(0, …)`, is success-only), so a `FunOutcome::Suspend` becomes `Step::Suspend { func, pc }` — the instruction left un-applied and the host re-consulted on resume, which must **continue** the pending application rather than restart it (the resume re-executes the instruction, so it re-derives both words again). `apply_target`'s BIF dispatch is the deliberate opposite (class B: a typed error, because `apply/3` has already rewritten `x(0..arity)`).
- Every error — the seam's own (`mod.rs:622`) and the fallback's (`mod.rs:619`) — is routed through the catch-aware funnel `Machine::raise_bif_error`, so a routable raise lands in a live `try`/`catch`; a genuine capability gap (`error::unsupported`) stays the uncatchable typed error. The shape check above only replaces the *bare* `badarg`/`badarity` the seam used to produce.
- The seam's `Step::Enter` is folded by the module-context-aware driver exactly like the cross-module-call seam below (switch context + re-bind the literal area); a `Step::Wait` from inside the closure leaves its module context for `resume_outcome`.

## The cross-module call seam (load-bearing — verify here, do not trust summaries)
- `apply`/`apply_last` resolve their target through `ExecutionContext::resolve_external_call` **FIRST** (`mod.rs:701`), which answers with the callee's **shared `Arc<Module>`** + its export entry `Label`. `apply_target` registers that module as a context and returns `ApplyTarget::Enter{context,func,pc}` (`mod.rs:704-709`). Only the seam's `Ok(None)` default falls back to the synchronous `ctx.call_bif` (`mod.rs:743-751`) → `ApplyTarget::Called`; a **host BIF error** there is routed through `Machine::raise_bif_error_with_head` and returned as `ApplyTarget::Raised` (`mod.rs:772-774`).
- `Instr::Apply`'s `Enter` arm stores the **caller's** return address `(func, pc+1, current_context)` in the live frame, then returns `Step::Enter` (`mod.rs:222-257`) — the same discipline as a local `Instr::Call`, so a `receive` inside the callee suspends **this** process and its `return` unwinds into the caller's frame.
- `Instr::ApplyLast`'s `Enter` arm returns `Step::Enter` after the frame is already popped (`mod.rs:273`, `:291-293`), so the callee runs on the caller's frame.
- The seam's `Step::Enter` is folded by the module-context-aware driver (`../../../src/machine/drive.rs:140-158`: switch context + re-bind the literal area); the callee's `Step::Wait` becomes `RunOutcome::Wait` (`drive.rs:159-161`), leaving the suspended frame's module context for `resume_outcome`.
- A `raise` from a cross-module callee lands in the **caller's** catch frame: the landing is reported as `Step::Enter` into the handler's context (see `../exceptions/tests/cross_module.rs:578`, whose `guarded_caller` builds the `proc_lib:init_p_do_apply/3` `try apply/3 catch ... end` shape).
- The `Instr::CallExt*` core arms (NOT this family) use the same seam: each arm (`dispatch.rs:474-497` for `CallExt`, `:498-517` for `CallExtOnly`, `:518-568` for `CallExtLast`) hands off to the shared `call_ext_dispatch` funnel (`dispatch.rs:986-1036`), which tries `resolve_call_ext_apply` (the `erlang:apply/3` special case, `dispatch.rs:785`), then `resolve_call_ext_send` (`dispatch.rs:914`), then `resolve_external_entry` → `resolve_external_call` (`dispatch.rs:720-741`); `apply_import_target` (`dispatch.rs:1114`) applies the arm's own `ImportFrame` discipline to the resolved target.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; this node has no `unsafe`.
- Errors are `beam_common::{Result, BeamError}` only — never a panic, never a wrong result; a non-atom target is the C's catchable `badarg` raise (`raise_badarg`), or the typed `BeamError::BadArg` when no catch frame / the `badarg` atom is absent.
- A **host BIF error** from an applied target (`apply_target`'s `call_bif` `Err`) is likewise the C's **catchable** raise: it is routed through `Machine::raise_bif_error` into the caller's live `try`/`catch` (`ApplyTarget::Raised`), matching C's `$HANDLE_APPLY_ERROR` → `handle_error` → `next_catch`. Only engine-diagnostic `Err`s from *outside* the host BIF call (a bad register, the resolver / label resolution) keep their `?` and escape as the typed error.
- Term construction/inspection only via `crate::terms` / `beam_term`.

## Test Strategy (`tests.rs`, hand-built synthetic modules; the loader is not involved)
- `MakeFun3`: `make_fun3_builds_a_closure_with_the_descriptor_fields` (:128), `..._with_no_free_variables` (:166), `..._with_a_missing_descriptor_is_a_typed_error` (:196), `..._with_a_free_variable_count_mismatch_is_a_typed_error` (:222).
- `call_fun`/`call_fun2`: `call_fun_dispatches_the_fun_in_x_arity` (:254), `call_fun_without_an_apply_hook_is_a_typed_unsupported` (:284), `call_fun_with_an_out_of_range_fun_register_is_a_typed_error` (:315), `call_fun2_dispatches_the_fun_in_its_operand` (:338).
- **The structured target-shape reasons** (helper `reason_value` :378 reads the `{Tag,Value}` out of a landed raise; `call_fun_caller` :397 builds `f(F) -> F(1)`): `call_fun_with_a_non_fun_target_raises_badfun` (:410 — `x(1) = 5` ⇒ `x(1) = {badfun,5}`), `call_fun2_with_a_non_fun_operand_raises_badfun` (:454 — the operand route, proving the reason re-reads the operand through the slack), `call_fun_with_a_wrong_arity_fun_raises_badarity` (:506 — a 2-ary closure called with 1 arg ⇒ `{badarity,{Fun,[one]}}`, with a counting `resolve_local_fun` installed to prove the arity guard precedes the seam), `a_wrong_arity_fun_never_reaches_the_seam_when_the_module_cannot_name_badarity` (:570 — the same mismatch on an atom-poor module ⇒ the typed `unsupported("call_fun: no 'badarity' atom …")` and a seam consultation count of 0), `call_fun_with_a_matching_arity_still_reaches_the_apply_fun_fallback` (:884 — the regression guard: a CORRECT-arity fun is still dispatched/declined, never shape-rejected), `call_fun_without_the_badfun_atom_declines_to_the_apply_fun_fallback` (:922 — a module that cannot name `badfun` declines the classification and reaches the trait-default `apply_fun` ⇒ the exact `unsupported_opcode: apply_fun`), `call_fun_with_a_non_fun_without_a_catch_frame_keeps_the_live_reason` (:969 — the uncaught path carries the term in the raise's live-reason slot).
- The three seam-routing tests (`a_resolve_local_fun_error_lands_in_a_live_catch_frame` :1944, `..._without_a_catch_frame_is_the_typed_error` :2002, `an_unsupported_capability_gap_from_the_seam_stays_uncatchable` :2049) install a **matching-arity** closure, so the seam (not the new shape check) is the error source under test.
- `apply` synchronous + badarg raise: `apply_dispatches_module_and_function_from_x_arity` (:1029), `apply_with_a_non_atom_module_is_a_typed_error` (:1056), `apply_badarg_lands_in_a_live_catch_frame` (:1084), `apply_last_badarg_lands_in_a_live_catch_frame` (:1122), `apply_badarg_without_a_catch_frame_is_the_typed_error` (:1152), `apply_badarg_without_the_badarg_atom_falls_back_to_the_typed_error` (:1177).
- `apply_last` frame/continuation: `apply_last_pops_the_frame_and_returns_through_the_continuation` (:1203), `apply_last_reports_a_context_switch_when_the_continuation_is_in_another_module` (:1271), `apply_last_at_the_bottom_frame_finishes_the_run` (:1331).
- **Cross-module seam** (the key coverage): `apply_enters_a_resolved_callee_in_the_same_machine` (:1527), `apply_into_a_cross_module_receive_suspends_and_resumes` (:1583 — drives `run_label_outcome`→`RunOutcome::Wait{0,0}` then a send + `resume_outcome`→`RunOutcome::Returned`), `apply_without_the_seam_keeps_the_synchronous_bif_path` (:1620), `apply_last_pops_its_frame_then_enters_a_resolved_callee` (:1668).
- Router declination: `non_fun_variants_return_none` (:1377).
- The `TestMailbox` stub (`:2110`) implements the receive seam for the cross-module receive test; markers are the typed `unsupported`.
- **The seam's rooting invariant is pinned from the integration suite**: `tests/fun_seam_rooting.rs` drives `resolve_local_fun`/`apply_fun_outcome`/`resolve_external_call` through a semi-space copying mock that collects *inside* the seam and poisons the region it vacated, then asserts the values the engine publishes afterwards are the **moved** terms (see *The in-machine closure-apply seam* above). The mock carries its own raw-word access with `// SAFETY:` rationale, exactly like the two existing mock collectors (`tests/rooted_alloc.rs`, `tests/raise_rooting.rs`): `#![deny(unsafe_code)]` governs the library crate, and these are separate test binaries. A test-only stand-in, because `MinimalContext` does not override `alloc_words_rooted` (a never-collecting arena) and the fun family's seams take no `RootSet`, so the mock reaches the live root set through `Machine::with_roots(RootSet::ALL_LIVE, …)`.

## Known Issues / Notes for Agents
- **The external-`fun M:F/A` arity shape is pinned in-node.** The header-arity classification is engine-side and kind-agnostic (`terms::fun_arity` reads the header field, which `beam_term::fun::write_fun_external` populates exactly as `write_fun_local` does); `crate::terms::make_fun_external` (`terms/fun.rs`, `#[cfg(test)]`-only) materializes a real `EXPORT_EXT` term and the `funs/tests.rs` external-fun cases pin both the dispatch (matching arity) and the `{badarity,{Fun,Args}}` (mismatched) routes.
- **Raise-tag atoms resolve through `machine::namespace_atom`** (`ctx.atoms(module)` first, the running module's load-time snapshot second), never through a bare `module.atom_*` lookup: `badarg` (`apply` non-atom module/function), `badfun` (a non-fun target), and the `error` class atom when the raise is uncaught. A tag the namespace cannot name is the typed `unsupported(...)`, never a substituted atom index.
- **A raise/receive from a cross-module callee IS propagated, not folded into `blocked()`.** Each `Instr::CallExt*` arm resolves the seam before the synchronous `call_bif` (`interpreter/dispatch.rs:474-497` for `CallExt`), so a callee suspend or raise reaches the driver. `blocked()` is only the **non-blocking** entries' report for a `receive` with no message (`crate::error::blocked`); a host that pumps (beam_rt) or a caller driving `resume_outcome` gets `RunOutcome::Wait`.
- Non-zero `wait_timeout` has **no host timer** — it suspends like `wait` and resumes only on a message (`../receive.rs:147-163`, `../CONTEXT.md` Known Issues). Not cross-module-specific.

## `call_fun`/`call_fun2` structured reasons: full detail (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **`call_fun`/`call_fun2` reports the oracle's structured reason for a malformed target, classified from the running module's own atom table.** The target shape is classified **before** the host seam, exactly as C's `call_fun` (`beam_common.c:1604-1622`) does: a target that is not a fun ⇒ `error:{badfun, Fun}` (`families/funs/mod.rs::raise_badfun`, C's `EXC_BADFUN`), and a fun whose header arity (`terms::fun_arity`, C's `fun_arity(funp)`) differs from the call's arity ⇒ `error:{badarity, {Fun, Args}}` (`raise_badarity`, C's `EXC_BADARITY`, `Args` the actual argument list). Both reasons are built through the rooted builders from the running module's atom table (the tag via `reason_atom`), never a fabricated or substituted atom. Residual: a module that cannot name the tag declines to the host *closure-apply* seam (`resolve_local_fun` ⇒ the value-returning `apply_fun` fallback), whose `Err` is routed through `Machine::raise_bif_error` -- so such an atom-poor module reproduces neither the structured reason nor a distinctive capability-gap sentinel; `beam_rt` seeds `badfun`/`badarity` in its core-atom set, so every real-runtime process takes the structured-reason path.

## See Also
- `../CONTEXT.md` → the family seam router + ownership + capability-gap list.
- `../../../CONTEXT.md` → the crate API surface (the `ExecutionContext` seam, Handoff #10 cross-module call, Handoff #1 receive/cross-module suspension).
- `../exceptions/tests/cross_module.rs` → the raise-side containment tests.
- `../../../src/machine/drive.rs` → how `Step::Enter`/`Step::Wait` are folded.
