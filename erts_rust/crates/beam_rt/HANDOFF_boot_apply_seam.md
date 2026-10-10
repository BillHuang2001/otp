# Handoff: two engine-side apply seams (owner: `../../crates/beam_engine`)

This file covers **two** related engine-side **apply** seams that `beam_rt` cannot close on its own.
Both are `beam_engine` changes — `beam_engine` is a sibling node of `beam_rt`, so the change is
escalated rather than made here, and nothing here is a `beam_rt` TODO.

* **Seam 1 — the `call_ext*`-encoded `erlang:apply/2,3`.** The `call_ext*` arms fall back to a
  *synchronous* BIF route for `erlang:apply/2,3`, so a callee that blocks in `receive` folds into the
  typed `blocked()` gap and the `logger` boot directive dies before registering. This is what keeps
  `beam_rt` from hosting a live `logger` process.
* **Seam 2 — the scheduled-process `call_fun`/`call_fun2`.** The `CallFun`/`CallFun2` arms delegate to
  `ExecutionContext::apply_fun`, a *value*-returning seam, so a closure that blocks can only ever be
  entered on a fresh pumpable machine — a **scheduled** process folds the suspension into the same
  typed `blocked()` gap.

Everything below is measured on this tree, not inferred.

## Seam 1 — the `call_ext*`-encoded `erlang:apply/2,3`

### Why Seam 1 matters
`beam_rt` cannot host a live `logger` process — and therefore cannot run the OTP lifecycle the way the
oracle does — until this `beam_engine` change lands.

### The symptom
After `beam_rt` runs the real `bootstrap/bin/start.boot` to completion (all 22 directives, ending at
`{progress,started}`), `erlang:whereis(logger)` is `undefined`. The oracle leaves a live
`logger_server` registered as `logger` (a second `logger_server:start_link()` answers
`{error,{already_started,Pid}}`). Pinned red by
`src/boot/bootscript.rs::tests::the_real_boot_registers_a_live_logger_process` and
`tests/boot.rs::the_cli_boot_registers_a_live_logger_process` (both `#[ignore]`d, each beside an active
`…leaves_logger_unregistered_pending_the_apply_seam_gap` counterpart asserting `lg_is_pid=false`).
Their ignore reason names this file.

### Root cause (single broken dispatch)
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

### The fix is sufficient, not merely necessary (measured)
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

### The change (`../../crates/beam_engine/src/interpreter/dispatch.rs`)
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

### Three in-crate retries that are UNSOUND — do not re-attempt them
Irrelevant once the engine fix lands, but recorded so no future `beam_rt` agent burns a round on them
(the same three are unsound for Seam 2 — see below):

1. **Restart the `call_ext` from scratch on the next scheduler round.** A restart re-runs the
   callee's prologue: `gen:register_name` would then see its own name and fail `already_started`.
2. **Park and re-execute the instruction at `Machine::current_code()`.** The host cannot tell
   *which* `call_ext*` form dispatched it, and `CallExtLast` pops the frame **before** the call, so
   re-executing it double-pops (`pop_frame_checked`).
3. **Rely on `current_code` being instruction-exact.** The baseline JIT's is **block-granular**
   (`jit_set_position` records the block-start pc), so a park address is instruction-exact only when
   the failing call went through `jit_step_one` → `step_instruction`; forcing `Tier::Interpreter` on
   the park would itself be a behaviour change.

### How to verify Seam 1
1. `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine` stays green.
2. `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --no-fail-fast`
   stays green (baseline before the fix: **1319 passed / 0 failed / 3 ignored**).
3. Run the two `#[ignore]`d positives — `--lib the_real_boot_registers_a_live_logger_process` and
   `--test boot the_cli_boot_registers_a_live_logger_process` — and the runtime-valued-`apply`
   reproducer (`apply(M,F,As)` with runtime `M`/`F` where the callee blocks in `receive`, oracle
   `{got,<pid>}`): all must pass.
4. Then **delete** the two `…pending_the_apply_seam_gap` pins, remove their `#[ignore]` on the
   positives, and delete/trim the `## Known Issues` / `## Notes for Agents` entries that point here.

## Seam 2 — the scheduled-process `call_fun`/`call_fun2`

### The mechanism
`ExecutionContext::apply_fun(fun, args) -> Result<Eterm>` (`beam_rt`'s impl:
`src/context/mod.rs`'s `apply_fun`) is a **synchronous, value-returning** seam, so it cannot express
"enter the callee in the *calling* machine". Its `Some(LocalFunTarget{..})` arm enters a `.beam` local
(`make_fun3`) closure's label through `RuntimeContext::run_label_blocking` — a **fresh nested
`Machine`** driven by `pump_until_returned` over a bounded scheduler pump
(``RUN_EXPORT_PUMP_SLICES = 64``, `BLOCKING_PUMP_DEADLINE = 10 s`).

`pump_once` returns early `Ok(None)` when `may_pump_scheduler() == false` — which is **exactly** a
scheduled process (`RuntimeContext::for_process` sets it `false`; a plain `RuntimeContext` defaults it
`true`). `pump_until_returned` then folds the suspension into `Err(beam_engine::blocked())` = the
reason atom `unsupported_opcode: run: process blocked (no message available)`, an **uncatchable**
capability gap. So the *same* closure apply works on the `-eval`/single-pass/boot routes (whose
context **may** pump) and fails as soon as the workload is a **scheduled process**.

The engine reaches the seam from `../../crates/beam_engine/src/families/funs/mod.rs`: **both**
`Instr::CallFun { arity }` and `Instr::CallFun2 { .. }` do
`let result = ctx.apply_fun(fun, &args)?; machine.set_x(0, result)?;` — a synchronous seam returning a
*value*. Contrast the sibling `apply`/`apply_last` family (same file), which routes through
`apply_target` → `ctx.resolve_external_call(m,f,a)` → `Step::Enter` and therefore *can* suspend
in-machine.

### Measured evidence (this tree; OTP 29.1.1 / erts-17.1)
Three fixture modules compiled with the system `erlc` into one scratch dir; both runtimes run the
*same* compiled code. `saphelper:call/1` is where the literal `{call_fun,0}` lives; `gen:call/4`'s
non-pid branch builds a `make_fun3` closure and applies it through `do_for_proc/2`.

```erlang
%% saphelper.erl
-module(saphelper).
-export([call/1]).
call(F) -> F().
```
```erlang
%% sapwait.erl
-module(sapwait).
-export([wait/0, sender/2]).
wait() ->
    spawn(sapwait, sender, [self(), 42]),
    saphelper:call(fun() -> receive X -> X end end).
sender(To, Msg) -> To ! Msg.
```
```erlang
%% sapname.erl
-module(sapname).
-export([run/0, loop/0]).
run() ->
    Pid = spawn(sapname, loop, []),
    register(sapname_loop, Pid),
    gen:call(sapname_loop, tag, hello, infinity).
loop() ->
    receive
        {tag, {From, Mref}, Req} -> From ! {Mref, Req}, loop()
    end.
```

| probe | system `erl` oracle | `beam_rt -eval '…'` (may-pump) | `beam_rt -pa DIR run <beam> <spec>` (scheduled) |
|---|---|---|---|
| `sapwait:wait/0` | stdout `42`, exit 0 | stdout `42`, exit 0 | **stdout empty, exit 3**, stderr `beam_rt: unsupported: opcode: run: process blocked (no message available)` |
| `sapname:run/0` | stdout `{ok,hello}`, exit 0 | stdout `{ok,hello}`, exit 0 | **stdout empty, exit 3**, same stderr line |

Invocations: scheduled `beam_rt -pa DIR run DIR/<m>.beam <m>:<f>/0`; may-pump
`beam_rt -pa DIR -eval 'X = <m>:<f>(), io:format("~w~n",[X]).'`; oracle
`erl -noshell -pa DIR -eval 'io:format("~w~n",[<m>:<f>()]), halt(0).'`.

The `-eval` column agreeing with the oracle is the key isolation: the *same* code path works when the
context may pump, so the failure is specific to the scheduled slice. (Incidental and out of scope
here: `gen:call` with a *finite* timeout instead hits a separate `receive … after T` gap →
`beam_rt: badarg`.)

### C ground truth
`call_fun` (`erts/emulator/beam/beam_common.c:1605`) sets up the free variables in
`reg[i + arity] = funp->env[i]` and returns the fun body's `code_ptr`; the instruction then dispatches
to it **in the same process/machine** — `emu/instrs.tab:351-377`'s `$CALL_FUN`, `i_call_fun`,
`i_call_fun_last` are `call_fun(c_p, $Fun, reg, THE_NON_VALUE)` followed by `$DISPATCH_FUN(next)`. So a
`receive` inside the closure suspends the **calling** process, exactly like the in-machine `call_ext`
seam — never a nested machine.

### The change belongs to `beam_engine` (no sound in-crate fix)
The seam's shape is the blocker: `apply_fun` returns a *value*, so the host has no channel to ask the
engine to enter the callee in-machine. And a scheduled context's `RuntimeContext::for_process` holds
the pass's `ProcessTable` (the table is moved out of the `Runtime` for the duration of the pass), so
re-entering `pump_scheduler` from within a slice is **structurally impossible**. Hence there is **no
sound `beam_rt` fix** — the change is engine-side, in the `CallFun`/`CallFun2` arms.

The sibling `apply`/`apply_last` family already proves the in-machine route works for a runtime `M`/`F`
(`apply_target` → `resolve_external_call` → `Step::Enter`), so the mechanism exists; only the
`call_fun*` arms do not use it.

### Likely shape of the fix
Add a **defaulted** `ExecutionContext` seam the engine consults **first** in the `CallFun`/`CallFun2`
arms — e.g. `resolve_local_fun(fun, arity) -> Result<Option<(Arc<Module>, Label, u32)>>` (or the
existing `local_fun_target` shape) — so the engine can enter the closure's label **in-machine** with
its own frame discipline (`Step::Enter`, a continuation for the non-tail `call_fun`, the popped frame
for `call_fun_last`) exactly as the `apply` family does. When the host declines (`Ok(None)`), the arm
falls back to `apply_fun` byte-for-byte. The closure's `uniq`/`index` → defining module + entry label
resolution is a **host** capability (it needs the loaded module set), which is why the seam must exist
on `ExecutionContext` rather than being derived inside the engine.

### Three in-crate retries that are UNSOUND — do not re-attempt them
The same three hacks from Seam 1 are unsound here for the same reasons: restarting the `call_fun`
re-runs the closure prologue; park-and-re-execute cannot tell which `call_fun*` form dispatched it
(`call_fun_last` pops the frame before the call, so a re-execute double-pops); and the baseline JIT's
`current_code` is block-granular so a park address is not instruction-exact. Do not attempt a
sentinel-error hack either.

### How to verify Seam 2 (status: closed or open)
`tests/scheduled_apply_seam.rs` is the pin. Its **active** case
`a_scheduled_process_folds_the_closure_apply_into_the_typed_blocked_gap` asserts today's exact outcome
(the scheduled `run` exits 3 with the identical stderr line, while the `-eval` counterpart exits 0
with the oracle's line — isolating the gap to the scheduled context). Its **`#[ignore]`d positive**
`a_scheduled_process_applies_a_blocking_closure_and_answers_the_oracle` asserts the target behaviour
(scheduled `run` exits 0 and prints the oracle's own line). When the engine fix lands: un-`#[ignore]`
the positive, **delete** the negative pin, and drop the `## Known Issues` entry in
`beam_rt/CONTEXT.md` that points here.

## Points that stay with `beam_rt` afterwards
* The `beam_rt`-side `resolve_external_call` still declines `erlang` (a `FLOOR_MODULES` member) — that
  is correct and unchanged: the `erlang:apply` routing decision belongs to the engine arm (Seam 1).
* A **scheduled** process's `call_fun`/`call_fun2` apply is **Seam 2** above — an engine change, not a
  `beam_rt` gap. Once the engine consults the host's closure-resolution seam first, `beam_rt`'s
  `RuntimeContext` implements it over the loaded module set; the value-returning `apply_fun` remains
  the fallback for the fun kinds with no local label (external/`-eval`).
