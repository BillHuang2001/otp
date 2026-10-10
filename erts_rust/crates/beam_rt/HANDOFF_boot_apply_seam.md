# Handoff: the `call_ext*`-encoded `erlang:apply/2,3` seam (owner: `../../crates/beam_engine`)

## Why this file exists
`beam_rt` cannot host a live `logger` process — and therefore cannot run the OTP lifecycle the way the
oracle does — until a **`beam_engine`** change lands. `beam_engine` is a sibling node of
`beam_rt`, so the change is escalated rather than made here. Everything below is measured on this
tree, not inferred; nothing here is a `beam_rt` TODO.

## The symptom
After `beam_rt` runs the real `bootstrap/bin/start.boot` to completion (all 22 directives, ending at
`{progress,started}`), `erlang:whereis(logger)` is `undefined`. The oracle leaves a live
`logger_server` registered as `logger` (a second `logger_server:start_link()` answers
`{error,{already_started,Pid}}`). Pinned red by
`src/boot/bootscript.rs::tests::the_real_boot_registers_a_live_logger_process` and
`tests/boot.rs::the_cli_boot_registers_a_live_logger_process` (both `#[ignore]`d, each beside an active
`…leaves_logger_unregistered_pending_the_apply_seam_gap` counterpart asserting `lg_is_pid=false`).
Their ignore reason names this file.

## Root cause (single broken dispatch)
Boot directive 13 is `{kernelProcess,logger,{logger_server,start_link,[]}}` →
`gen_server:start_link({local,logger},…)` → `gen:do_spawn` → `proc_lib:start_link(gen, init_it, …)` →
`erlang:spawn_opt(proc_lib, init_p, […], [link,monitor])`. The spawned child runs
`proc_lib:init_p_do_apply/3`, whose body the installed `erlc` compiles to the **non-tail**
`{call_ext,3,{extfunc,erlang,apply,3}}` inside a `try`.

1. the engine's `CallExt*` arm asks the host
   `RuntimeContext::resolve_external_call("erlang","apply",3)` (`src/context/mod.rs`), which answers
   `Ok(None)` because `erlang` ∈ `crate::native::FLOOR_MODULES`;
2. the engine therefore falls back to `call_import` → `RuntimeContext::call_bif("erlang","apply",3,…)`
   → the native floor's `apply_dynamic` (`src/native.rs`) → `call_bif(Mod,Fun,N,…)`;
3. that external-module route is `run_export` → `run_export_blocking` (`src/context/mod.rs`), whose
   `pump_once` returns `Ok(None)` when `may_pump_scheduler() == false` — which is exactly a scheduled
   process (`RuntimeContext::for_process`) — so `pump_until_returned` yields
   `Err(beam_engine::blocked())` (`beam_engine/src/error.rs`);
4. the child dies with the reason atom
   `unsupported_opcode: run: process blocked (no message available)` (a capability gap, so
   **uncatchable**), the `[link,monitor]` tears down the short-lived directive process, and
   `src/boot/drive.rs::start_kernel_process` correctly treats an already-terminated directive as `Ok`.
   The boot therefore still reaches `{progress,started}` while `logger` never registers.

Because `gen:init_it/7` registers the name **inside** the apply target, the child dies before
`register(logger, self())` is ever reached — nothing was registered and nothing needs unwinding.

## The fix is sufficient, not merely necessary (measured)
A fixture whose **first-level** dispatch to `gen:init_it/7` goes through the engine's
`apply`/`apply_last` family — the runtime-`M`/`F` path — runs the **whole** chain correctly, in both
the tail and the non-tail form: `logger` **is** registered (`whereis(logger)` is a live pid), the
`proc_lib` start ack arrives (`logger_server:init/1` returned `{ok,State}`, so the ETS config and
`logger_simple_h:adding_handler/1` all succeeded), and the child **stays alive** parked in its receive
(checked with `is_process_alive/1` after 600 ms). Controls agree with the oracle byte for byte.

⇒ There is **no second blocker** in `gen:init_it` / `gen:init_it2` / `gen_server:init_it` /
`logger_server:init/1`. The `call_ext*`-encoded `erlang:apply/3` is the sole broken dispatch. Ground
truth for the neighbouring instructions: `gen:init_it/7` is `{call,1,{gen,register_name,1}}` then
`{call_last,7,{gen,init_it2,7}}`, and `gen:init_it2/7` is `{apply_last,6,0}` — the working family.

## The change (`../../crates/beam_engine/src/interpreter/dispatch.rs`)
In the `Instr::CallExt` / `Instr::CallExtOnly` / `Instr::CallExtLast` arms, special-case the import
target `erlang:apply/2,3`, exactly as C does in `fixed_apply`'s `/* Handle apply of apply/3 … */`
special case (`erts/emulator/beam/beam_common.c:1539-1557`, reached through `$DISPATCH_EXPORT`; see
also `erts/emulator/beam/emu/instrs.tab:234-255`'s `$APPLY`/`$HANDLE_APPLY_ERROR` falling back to
`apply3_mfa`):

* decode `x(0) = Module`, `x(1) = Function`, `x(2) = Args` (and `Args = []` for `apply/2`);
* unpack the `Args` list into `x(0..N)` and place `Module`/`Function` at `x(N)` / `x(N+1)`;
* dispatch through the existing `families::funs::apply_target(machine, module, N, ctx)` under the
  **same** frame discipline each arm already applies to its seam path (`CallExt` stores the
  continuation `(func, pc+1, current_context)`; `CallExtOnly` stores nothing; `CallExtLast` pops the
  frame before entering), so the callee is entered **in-machine** as `Step::Enter` and a `receive`
  inside it suspends through the ordinary `RunOutcome::Wait` path;
* a non-atom `Module`/`Function` or a non-list / non-proper `Args` must raise the same **catchable**
  `badarg` the oracle does — `apply_target`'s `raise_badarg` already does this.

The engine's `apply`/`apply_last` family already resolves a runtime `M`/`F` through
`resolve_external_call` → `Step::Enter`, so this is a re-routing of the existing machinery, not new
semantics.

## Three in-crate retries that are UNSOUND — do not re-attempt them
Irrelevant once the engine fix lands, but recorded so no future `beam_rt` agent burns a round on them:

1. **Restart the `call_ext` from scratch on the next scheduler round.** A restart re-runs the
   callee's prologue: `gen:register_name` would then see its own name and fail `already_started`.
2. **Park and re-execute the instruction at `Machine::current_code()`.** The host cannot tell
   *which* `call_ext*` form dispatched it, and `CallExtLast` pops the frame **before** the call, so
   re-executing it double-pops (`pop_frame_checked`).
3. **Rely on `current_code` being instruction-exact.** The baseline JIT's is **block-granular**
   (`jit_set_position` records the block-start pc), so a park address is instruction-exact only when
   the failing call went through `jit_step_one` → `step_instruction`; forcing `Tier::Interpreter` on
   the park would itself be a behaviour change.

## How to verify the fix
1. `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine` stays green.
2. `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --no-fail-fast`
   stays green (baseline before the fix: **1319 passed / 0 failed / 3 ignored**).
3. Run the two `#[ignore]`d positives — `--lib the_real_boot_registers_a_live_logger_process` and
   `--test boot the_cli_boot_registers_a_live_logger_process` — and the runtime-valued-`apply`
   reproducer (`apply(M,F,As)` with runtime `M`/`F` where the callee blocks in `receive`, oracle
   `{got,<pid>}`): all must pass.
4. Then **delete** the two `…pending_the_apply_seam_gap` pins, remove their `#[ignore]` on the
   positives, and delete this file plus the `## Known Issues` / `## Notes for Agents` entries that
   point at it.

## Points that stay with `beam_rt` afterwards
* The `beam_rt`-side `resolve_external_call` still declines `erlang` (a `FLOOR_MODULES` member) — that
  is correct and unchanged: the `erlang:apply` routing decision belongs to the engine arm.
* A **scheduled** process's `call_fun`/`apply_fun` slice-context apply (`gen:call/4`'s fun
  application) is a separate, still-open gap recorded in `src/context/CONTEXT.md`.
