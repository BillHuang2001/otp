# `beam_rt` — boot subsystem (`src/boot.rs` + `src/boot/`)

## Intent
The **boot sequence** of the runtime (contract **A.7**, OTP-app contracts). `src/boot.rs` owns the
preloaded-module *name registry* (`PRELOADED_MODULE_NAMES` / `PreloadedModules`), the core-atom seed
(`CORE_ATOMS` / `seed_core_atoms`) and the version banner; `src/boot/preloaded.rs` (module
`crate::boot::preloaded`) is what actually **loads** the preloaded `.beam`s into the shared module
cache. `src/boot/bootscript.rs` (module `crate::boot::bootscript`) is the **OTP boot-script reader +
executor**: it parses a `.boot` file into a pure-Rust `BootScript` and executes its directives against
a `Runtime`. `src/boot/drive.rs` holds the **bounded scheduler drives** the two process directives
use. `src/boot.rs` is the module root — this directory holds the loader + boot-script + drive
submodules and their docs; do not look for `boot.rs` here.

## API Surface
- `boot.rs`: `PRELOADED_MODULE_NAMES` (the oracle's `erlang:pre_loaded/0` set **and order**),
  `CORE_ATOMS`, `seed_core_atoms(&mut AtomTable)`, `boot_banner()`, `erts_version()`,
  `PreloadedModules::{standard,is_preloaded,names,len,is_empty}`, and
  `pub(crate) use preloaded::load_preloaded`.
- `boot/preloaded.rs`: `pub(crate) fn load_preloaded(&mut CodeServer, &PreloadedModules) -> Result<usize>`
  — adds the system code path, then `CodeServer::load`s every registry name in order; returns the count
  loaded. `load_names` is the (private) inner loop, split out so the failure path is testable without a
  system install.
- `boot/bootscript.rs` (contract A.7, part 2): `BootScript { name, vsn, instructions }`, the directive
  enum `Instruction::{PreLoaded, Progress, Path, PrimLoad, KernelLoadCompleted, Module, KernelProcess,
  Apply}`, `Mfa { module, function, args }`, and the argument tree
  `Value::{Atom, Int, Str, List, Tuple, Other}`. Entry points `read(&Path) -> Result<BootScript>` (ETF
  decode via `beam_codec::decode_term` + a pure-Rust walk; no `Eterm` outlives the call) and
  `execute(&mut Runtime, &BootScript) -> Result<()>`.
  Boot-stop observability (additive): `pub struct BootStop { pub index: Option<usize>, pub total: usize,
  pub instruction: Option<Instruction>, pub error: BeamError }`, `BootStop::describe(&self) -> String`
  (a one-line summary: index/total + the failing directive as `M:F/A` or `{kernelProcess,Name,…}` + the
  error), `BootStop::describe_with(&self, &mut Runtime) -> String` (the same, but a **structured** raise
  reason is rendered as its real term through the runtime's printer — e.g. a structured
  `{badmatch, Value}` rather than `<structured reason>`; a non-structured or
  unprintable reason falls back to `describe`), and
  `pub fn execute_reporting(&mut Runtime, &BootScript) -> std::result::Result<(), BootStop>`
  — the **primary** implementation, which `execute` delegates to (`execute(..).map_err(|stop| stop.error)`),
  so the two can never drift. `index`/`instruction` are `None` when the runtime failed to boot before
  the first directive ran.
- `boot/drive.rs`: `start_kernel_process`, `apply_boot_call`, `drive_scheduler`, and the budgets
  `DRIVE_DEADLINE` (per `execute` pass), `KERNEL_BURST_DEADLINE`, `KERNEL_BURST_SLICES` (=1),
  `APPLY_BLOCK_SLICES`.
- Consumed by `Runtime::boot` (`../runtime.rs`), driven by `Runtime::boot_if_needed` on every public
  entry point; `Runtime::boot_from_script` drives `execute`; `Runtime::preloaded_loaded()` /
  `is_preloaded_loaded(name)` report the loaded set. `Runtime::boot_script_id` **retains the loaded
  script's `{Name,Vsn}`** (set by `boot_from_script`/`boot_from_script_reporting`, read by the
  `init:script_id/0` floor entry) so the script's identity survives the call. **The CLI really runs a
  boot script on every action but `-version`**: `../cli.rs`'s `boot_script_to_drive` resolves the script
  (the explicit `-boot FILE` value, else this runtime's **own** release's
  `bootstrap/bin/start.boot` — see below) and `boot_then` boots the 22 preloaded `.beam`s and then
  executes it.
  With the default route loading **this runtime's own** release's script, `Runtime::boot_script_id`
  and `erlang:system_info(otp_release)` agree (`{"Erlang/OTP","30"}` / `"30"`), so the differential
  case `boot_extra/init_script_id_names_the_running_otp_release` **agrees**. The `{path, …}` sink
  (`../boot/bootscript.rs`'s `Instruction::Path` arm) appends a directory **only when it really
  exists** — `code:add_patha/1` answers `{error,bad_directory}` for an absent one — so the primary
  bootstrap's *source-tree* `$ROOT/lib/kernel/ebin` does not land on `code:get_path/0` as a dangling
  entry when `$ROOT` is a versioned installation's root.
- `code:which/1` / `code:all_loaded/0` / `code:is_loaded/1` render `preloaded` from the **name registry**
  (`../native/code.rs`), not from this cache; `code:all_loaded/0` explicitly filters preloaded names out
  of the cache so it does not list them twice.

## The boot script
A `.boot` file is `binary_to_term` of `{script, {NameString, VsnString}, [Instruction, ...]}`. The real
`bootstrap/bin/start.boot` (OTP 30 / erts-17.1; `{"Erlang/OTP","30"}`, **22 instructions**) uses exactly
these eight directives, the full modelled set:

| Directive (arity) | `Instruction` | `execute` semantics |
|---|---|---|
| `{preLoaded,[Mod]}` (2) | `PreLoaded` | load each name from the code path (already loaded by `boot()` ⇒ a memoized no-op) |
| `{progress,Atom}` (2) | `Progress` | informational marker; never fails |
| `{path,[DirString]}` (2) | `Path` | expand a leading `$ROOT`, then append each dir to the code path |
| `{primLoad,[Mod]}` (2) | `PrimLoad` | load each name from the code path |
| `{kernel_load_completed}` (1) | `KernelLoadCompleted` | informational marker; never fails |
| `{module,Mod}` (2) | `Module` | load one name from the code path |
| `{kernelProcess,Name,{M,F,A}}` (3) | `KernelProcess` | materialize args, spawn, then drive a bounded burst until the **directive's MFA has returned** (its server has registered, acked and entered its loop; a run-queue drain or the burst deadline ends it too) so a later directive finds the server answering; then **unlink** it from the short-lived directive process |
| `{apply,{M,F,A}}` (2) | `Apply` | materialize args, call `M:F(A...)` through the normal dispatch |

- **`$ROOT`** is the Erlang root derived from `CodeServer::lib_root().parent()` (the same convention as
  the private `native/code.rs::code_root_dir`); a `$ROOT` that cannot be resolved (no
  `<lib>/<app>-<vsn>/ebin` on the code path) is a typed `error::message`.
- **Args are a literal-only tree.** `Value` covers atoms, small ints, Erlang strings (small-int lists,
  rendered `Str`), proper lists and tuples, nested. Anything else (pid/fun/float/big/binary/map/ref) is
  `Value::Other`, which `execute` refuses with the typed capability gap
  `unsupported: boot-script argument term outside the supported literal set` — never a fabricated term.
- **`execute` boots first** (`Runtime::boot`, idempotent) so `$ROOT` expansion and module loads work on
  a fresh runtime. It **stops at the first directive that cannot run** and returns that typed error.
- **The two process directives drive the scheduler — bounded, and they never hang.** A
  `{kernelProcess, …}` spawns and then runs a bounded burst that ends as soon as **the directive's MFA
  has returned** (its process leaves the table), the run queue drains, or the burst deadline expires —
  registration alone is *not* a readiness signal (`application_controller:init/2` registers first and
  sets up afterwards). A burst that ends without the name is deliberately **not** this directive's error
  (matching `init`, which never checks — the failure surfaces at the next dependent directive). An
  `{apply, …}` runs the export through the engine's blocking-aware entries and drives the scheduler
  whenever the call suspends, resuming it once a boot-spawned process answers. Both drives go through
  `drive_scheduler` over the crate-internal `Runtime::pump_scheduler_boot`, with per-call slice budgets
  between deadline checks and **one** wall-clock `DRIVE_DEADLINE` (10 s) shared by the whole `execute`
  pass, plus a 1 s `KERNEL_BURST_DEADLINE` cap per burst. An exhausted deadline is a typed
  `error::message`; a blocked apply whose drive ran nothing is the engine's typed `error::blocked()`; a
  drive stops as soon as a `pump_scheduler_boot` call reports `0`.
  A capability gap stays an `error::unsupported` raise.
  `pump_scheduler_boot` is `pump_scheduler` with an **imminent-only** sleep policy
  (`TimerWait::ImminentWithin(TIMER_PUMP_SLICE)`): a step sleeps the remaining time to the earliest
  pending deadline **only when that deadline is within one pump slice (100 ms)** and returns `0` at once
  otherwise, so a boot-armed timer a second (or an hour) away — the `gen_server` `WaitTimeout`s the
  boot's parked processes hold — costs the drive nothing. Every already-due timer still fires
  (`fire_due_timers` runs before the wait), and a boot-spawned process parked on a short
  `receive … after T` (T ≤ the window) is still slept to and resumed. The boot's blocking calls are
  `infinity` receives, so nothing a directive synchronously awaits lies beyond a slice.

### Surviving a `{kernelProcess, …}`'s directive exit (the unlink)
This runtime runs a `{kernelProcess, …}` MFA in a **short-lived** process that exits when the MFA
returns, whereas a real `init` runs it in the **one** long-lived `init` process
(`erts/preloaded/src/init.erl:1520`; its `start_in_kernel/5` is **not exported** from `init.beam`, so it
cannot be called). The kernel servers that matter (`logger_server:start_link/0` via `gen_server`, and
`application_controller:start/1`'s `spawn_link`ed `init/2`) are `proc_lib`/`gen_server` children of that
process: such a server **traps exits** and terminates when its parent exits with `normal` (`proc_lib`'s
parent-exit semantics). Left linked, the short-lived directive's ordinary return tears its own server
down and releases the name — which is why a `gen_server:call` to it then fails with `noproc`.
`start_kernel_process` therefore **unlinks** the server from the directive as soon as `whereis(Name)`
resolves, while the directive is still alive (it is blocked in `receive` until the server acks): an
unlink beats the exit because the server's `{'EXIT', _, normal}` is never produced, and a `{'EXIT',…}`
already queued cannot be undone by a later unlink — hence `KERNEL_BURST_SLICES = 1` (the burst advances
one slice, then re-checks `whereis` at the loop top, landing in the window where the server has
registered but the directive has not resumed). A directive that has **already** terminated
(`heart`'s `start/0` returns after it `spawn`s a process it does **not** link) is skipped, not an error;
`unlink(a, a)` for a self-registering directive is the process table's documented no-op.

## Where the real `start.boot` gets now
Executing the real script (`bootstrap/bin/start.boot`, 22 instructions) with the CLI-equivalent
configuration **runs all 22 directives to completion**, ending at the final `{progress,started}`
(0-based directive index 21 of 22); the CLI exits **0** on every action but `-version` (each drives the
resolved script) and the in-crate executor
(`boot_from_script` → `execute`) returns `Ok` (`BootStop` is `None`).
The tail's 0-based indices: **12** `{kernelProcess,heart,{heart,start,[]}}`,
**13** `{kernelProcess,logger,{logger_server,start_link,[]}}`, **14**
`{kernelProcess,application_controller,{application_controller,start,[KernelApp]}}`, **15**
`{progress,init_kernel_started}`, **16** `{apply,{application,load,[{application,stdlib,Props}]}}`,
**17** `{progress,applications_loaded}`, **18**
`{apply,{application,start_boot,[{application,kernel,Props},permanent]}}`, **19**
`{apply,{application,start_boot,[{application,stdlib,Props},permanent]}}`, **20**
`{apply,{c,erlangrc,[]}}`, **21** `{progress,started}` (the final marker).
All 22 directives run and the three `{kernelProcess, …}` startups (12–14) succeed.
The tail's blockers are served **in-node** from the native floor (`RuntimeContext::try_native`) or
runtime state, then fall through to the registry unchanged for everything else.
The ones this tail needs: `process_flag` for the bare `monitor_nodes` atom and the
`{monitor_nodes, Opts}` tuple flag (`../native/process.rs::process_flag_native`, the shape
`net_kernel:monitor_nodes/2` at `net_kernel.erl:673` and `global:init/1` at `global.erl:802` require);
`erlang:nodes/2` (`../native/net.rs`, called by `global:init/1`'s
`nodes(visible, #{connection_id => true})` at `global.erl:810`); `prim_tty:tty_select/2` over the port
poller (`../native/prim_tty.rs`); `process_flag(message_queue_data, on_heap | off_heap)` (parsed and
accepted); and `erlang:system_flag(system_logger, _)` (`../seq_trace.rs`).
`erts_internal:scheduler_wall_time/1` is served by the `RuntimeHooks` bridge (`../context/runtime_hooks.rs`):
its caller `kernel_sup`'s `kernel_refc` child calls `resource(scheduler_wall_time, Enable)`
(`lib/kernel/src/kernel_refc.erl:141-142`), and the bridge answers the previous state (sampling itself
is not implemented, but the exchange is faithful), so `kernel_refc` does not `start_error`.
`erlang:hibernate/0` is an OTP-30 **instruction** the C rewrites from `call_ext erlang:hibernate/0` to
`i_hibernate` (`erts/emulator/beam/emu/ops.tab:896-898`); the native floor models it
(`../native/process.rs`) as "continue" — it answers the atom `true` (the C's `erts_hibernate` only
shrinks the heap and clears the ACTIVE flag; the caller's own following `receive` does the blocking) —
so `gen_server:loop_hibernate/4` (`lib/stdlib/src/gen_server.erl:2338`) does not crash
`kernel_safe_sup`/`logger_sup`.
- **Directive 20's file consult completes.** `c:erlangrc/0`'s file consult is
  `gen_server:call(file_server_2, {open,<the `$HOME` erlangrc path>,[read]}, infinity)`
  (`lib/stdlib/src/c.erl:1049-1083` → `file:path_eval/2`). `beam_rt`'s runtime-native
  `file_server_2` service ([`../native/file_server.rs`](../native/CONTEXT.md)) is spawned unregistered
  (`Runtime::new` gates it on `RuntimeConfig::drives_a_boot_script()`) on every route that drives a boot
  script — every CLI action but `-version`, plus an explicit `-boot FILE` — so the booted
  `kernel_sup`'s own `file_server_2` child owns the name; that child answers the request
  with `{error,enoent}` for a missing config file, which `c:erlangrc/0` treats as success, so the walk
  proceeds to `{progress,started}`. `c:erlangrc/0` likewise **returns** `{error,enoent}` without
  raising for a missing config file (verified: `beam_rt -eval 'c:erlangrc()'` →
  `{error,enoent}`, the oracle's own answer).
- **The CLI and the in-crate executor agree — both complete.** The CLI
  (`beam_rt -noshell -boot bootstrap/bin/start`, run from the repository root) exits **0** with empty
  **stdout** and empty **stderr**, and so does every other action but `-version` when its own
  instruction prints nothing; no crash report is logged.
  `tests/boot.rs`'s `the_cli_runs_the_real_boot_script_to_completion` asserts `stdout == ""` **and**
  `stderr == ""`, which both hold. The in-crate executor (`boot_from_script` → `execute`) returns
  `Ok`, so `BootStop` is `None` and `execute_reporting` reports the completion.
- **The configuration is part of the measurement.** `init:get_arguments/0` reports the whole command
  line, and the executor tests build their runtime with their own `system_runtime()` helper — the
  CLI's own parser (`crate::cli::parse`, not `parse_with_env`: a test must not depend on `ERL_*FLAGS`)
  over the CLI argv `beam_rt -noshell -boot bootstrap/bin/start` — and `tests/boot.rs` mirrors it
  (`boot_cli_runtime`). The **four automatic flags** (`root`/`home`/`progname`/`bindir`) are served
  by `init:get_argument/1` *and* prepended to `init:get_arguments/0`
  (`../native/init.rs::get_arguments`, in the oracle's order) — and they are not a boot determinant:
  the real script reaches the same completion under a `RuntimeConfig::default()` runtime too
  (`tests/a9_native_floor.rs::script_id_is_the_loaded_boot_scripts_real_header`, which drives the
  script to report `init:script_id/0`).
- **The three `{kernelProcess, …}` directives (12–14) run and their servers survive.** Each burst
  drives until its directive's MFA returns, so `whereis(application_controller)` answers a live,
  registered server — the directive ran `application_controller:start/1` to its `{ok, AC}` return
  (`application_controller.erl:203-211`), i.e. the AC completed `init/2` (`:531-558`) and reached
  `gen_server:enter_loop`. Those servers are the ones directive 18 then consults.
  `heart` is not registered, legitimately: `heart:start/0` returns `ignore`.
- **`logger` IS registered — a live `logger_server` named `logger`.** Instruction 13's
  `{kernelProcess,logger,{logger_server,start_link,[]}}` leaves a live `logger_server` registered as
  `logger` (directive 13's registered server outlives its short-lived directive process — the unlink
  above), matching
  the oracle (a second `logger_server:start_link/0` reports
  `{error,{already_started,Pid}}`).
  `logger_server:start_link/0` → `gen_server:start_link({local,logger}, …)` → `gen:start/6` →
  `proc_lib:start_link/5` spawns the child and parks the caller in `proc_lib:sync_start/2`; the child
  is entered at `proc_lib:init_p_do_apply/3`, whose `try … apply(M,F,A) …` body `erlc` compiles to a
  **non-tail** `{call_ext,3,{extfunc,erlang,apply,3}}` (the `call_ext*` target is a literal atom even
  when the applied module is a variable).
  The engine's `call_ext*` arms special-case `erlang:apply/3` with C's `fixed_apply` treatment
  (`beam_engine/src/interpreter/dispatch.rs::resolve_call_ext_apply`): it decodes `x(0)=Module`,
  `x(1)=Function`, `x(2)=Args`, walks `Args` into `x(0..N)`, places `Module`/`Function` at
  `x(N)`/`x(N+1)`, and dispatches the callee **in-machine** as `Step::Enter` under the arm's own
  `CallExt*` frame discipline. The callee is reached through two nested `apply/3` levels
  (`proc_lib:init_p_do_apply/3` → `gen:init_it/7` → `gen_server:init_it/6`) in **one** process, so a
  blocking `receive` suspends *this* process through the ordinary `RunOutcome::Wait` path instead of
  folding into the typed `error::blocked()`; `gen_server:init_it` reaches its
  `register(logger, self())` and the directive's parked `sync_start/2` (timeout `infinity`) resumes.
  The `start_kernel_process` unlink (above) then keeps the registered server alive past the
  short-lived directive process.
  Pinned by `bootscript.rs::tests::the_real_boot_registers_a_live_logger_process` and, at the CLI, by
  `../tests/boot.rs::the_cli_boot_registers_a_live_logger_process` — both **pass**; the registration
  survives the whole 22-directive run to `{progress,started}`.
- **The kernel application starts at directive 18 and stays up.** Directive 18's
  `application:start_boot/2` reaches `application_controller:start_boot_application/2`, whose
  `application:load(kernel)` returns `{error,{already_loaded,kernel}}` (the AC's own `init/2` loaded
  kernel at `:531-558`), so it calls
  `gen_server:call(application_controller, {start_application,kernel,permanent}, infinity)`; the
  `handle_call({start_application,…})` (`:697`) → `check_start_cond` → `spawn_starter/3` (`:1443`) →
  `application_master:start_link/2` (`proc_lib:start_link`) handshake completes, so the directive's
  `apply` returns `Ok` — and the `permanent` kernel application stays up: its `kernel_safe_sup` /
  `logger_sup` children do not crash on `erlang:hibernate/0`, which the native floor serves. The seam
  that lets the application master's callback fun be entered is `RuntimeContext::resolve_local_fun`
  (`../context/execution.rs`): it resolves an **external** fun (`fun M:F/A`, `EXPORT_EXT`) to the callee
  module's export label through the `external_fun_target`/`external_fun_name` helpers
  (`../context/apply.rs`), so the engine enters e.g. `gen_server`'s callback fun **in-machine** instead
  of falling back to the value-returning `apply_fun`, which nested a fresh `Machine` under
  `may_pump_scheduler() == false` → `../context/pump.rs` refuses → the uncatchable `blocked()`.
- **Directive 19's stdlib start runs and the walk continues.** `application:start_boot/2`
  reaches `application_controller:start_boot_application/2`, and the `application_controller` started
  by directive 14 (`{kernelProcess,application_controller,…}` — the controller itself, not a child of
  the kernel application's supervision tree) is still registered, so its
  `gen_server:call(application_controller, {load_application,stdlib}, infinity)` answers and the walk
  reaches directive 20 and the final `{progress,started}`. Pinned by
  `../tests/boot.rs::the_boot_chains_kernel_processes_survive_their_directive_processes`, which asserts
  `whereis(application_controller)` is a live pid after the last directive. The detail is in
  [`../M1_GAPS.md`](../M1_GAPS.md) *A.7/A.9 boot*.

### The completion-asserting tests (all pass)
The three `cargo test --lib boot::` tests (`the_real_start_boot_executes_every_directive_to_progress_started`,
`the_real_start_boot_completes_through_execute_reporting`, `the_real_boot_registers_a_live_logger_process`)
and the two `cargo test --test boot` tests (`the_real_start_boot_runs_to_completion`,
`the_cli_runs_the_real_boot_script_to_completion`) assert the FULL 22-directive completion (ending at
`{progress,started}`) and **pass**; `tests/a9_native_floor.rs::script_id_is_the_loaded_boot_scripts_real_header`
passes too. The directive-12–14 `{kernelProcess, …}` registrations and the CLI `logger` registration all
survive to completion.

### A `-boot` value's shape does not affect the outcome
The boot is **not** sensitive to the byte length or the canonical form of the `-boot` value:
`$P/bootstrap/bin/start`, `$P/bootstrap/./bin/start`, `$P/bootstrap//bin/start` and
`$P/bootstrap/../bootstrap/bin/start` all carry the real script to completion (exit 0).
**What guarantees it is the runner's parked-root publication.** A suspended process's machine lives
in `RunnerState.engines`, and a machine's registers are only a *copy* of the process's root set; the
runner therefore publishes the parked machine's whole live root set into `Process::regs` — and its
live pending-exception reason as an extra root — at **both** park arms and re-syncs the machine from
the process's authoritative root set on resume (`../runner/run.rs::publish_parked_roots` /
`resume_from_process_roots`). Without that pair a
collection driven while the process was parked — `Process::deliver`'s exit cascade is **not** gated
by `Runtime::gc_process_enabled` — would relocate the heap against a partial root set and leave a
stale word in `x`, which is what argv length used to perturb.
**The escape-store stabilization is honest.** `../runtime/gc.rs::try_stabilize_escaped` copies a
runtime-level store's term onto the pointer-stable arena, propagates a **genuine** copy failure as
the emulator's `error:system_limit` (never storing an un-copied term) and trips the `disable_gc`
valve **only** for the documented off-heap-payload capability gap, where the original term is kept
because it cannot be duplicated.

### Remaining gaps (owner per link)
1. **`beam_engine`/`beam_process` — a scheduled `compile:file/2` runs to completion and agrees with the oracle.** The incremental io-device read (`file:read/2`, `file:read_line/1`, `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4`) completes from a **scheduled** process and agrees with the oracle (`../tests/scheduled_io_device.rs`, `../tests/io_scan.rs`), as does a `receive … after T` timeout reached through a nested run.
   Both A.9 `selfhost/*` corpus cases — `selfhost/in_process_compile_file_no_spawn_deterministic` and `selfhost/scheduled_process_compile_file_deterministic` — **agree** with the oracle on both execution tiers. The corruption they once reported was a **signal-driven** collection of a woken monitor watcher (`beam_process`'s process-termination cascade, `crates/beam_process/src/ptab.rs` → `enqueue_down_signal` → `copy_reason_onto` → `grow_receiver_for_signal`), declined by the runner's park-window relocation exemption; the parked machine's live `Machine::pending_exception` reason remains a real GC root across that window (`../runner/run.rs::publish_parked_roots`, read back on resume by `RuntimeContext::resync_parked_roots`). The decline predicate is still owed to `ptab.rs`'s signal sites. [`../M1_GAPS.md`](../M1_GAPS.md)'s OTP-lifecycle survey carries the detail.
2. **`beam_rt` `../native/` — the io-device read path completes from library code on both routes.**
   `{get_until,Enc,_Prompt,Mod,Func,XtraArgs}` and `{get_line,Enc,_Prompt}` are both real request arms (`../native/file_server.rs` dispatches each to `get_until::serve`/`get_line::serve`), and the service answers with the tag paired to the request, so `file:read/2`, `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4` and `file:read_line/1` complete and agree with the oracle from the pumped `-eval` route (`../tests/io_scan.rs`) and from a **scheduled** `run` process (`../tests/scheduled_io_device.rs`), and `io:get_line/2` runs through the same `{get_line,…}` arm (the colocated `file_server/get_line/tests.rs`).
   The residual io-device divergence is a `{get_chars,…}` on a `{encoding,unicode}` **stream** counting BYTES where the oracle counts CHARACTERS, documented in `../native/file_server/CONTEXT.md`.
   Residual typed gaps in that family include a request `Enc` that is neither `latin1` nor `unicode` (`../native/file_server/get_line.rs`'s request-encoding check, `../native/file_server/get_until.rs`'s) and — for `{get_until,…}` only — a user-supplied continuation whose own code blocks in a `receive` (`../native/file_server/get_until.rs`, module header "A blocking continuation is a typed gap").
   The served device surface and the full gap list are itemised in `../native/CONTEXT.md`.

## The system code path resolution (`../codeserver.rs`)
`CodeServer::add_system_paths` resolves the *system* ebin set through `codeserver.rs::resolve_system_ebins`:
`$ERTS_STDLIB_DIR` when set, else the native filesystem probe `native_system_ebins` (over `native_roots` /
`system_ebins_under`), else the `erl` shell-out `erl_system_ebins` as the documented **last resort**
(`erl -noshell -eval 'code:lib_dir(...)'`).
The native probe reads the *standard OTP installation layout* — `<root>/lib/<app>-<vsn>/ebin`, or an OTP
source tree's `<root>/lib/<app>/ebin`, and the `erts` application at `<root>/lib/erts-<vsn>/ebin`,
`<root>/erts-<vsn>/ebin` or `<root>/erts/preloaded/ebin`. A candidate root is accepted only when it really
holds the probe module (`lists.beam` decides — stdlib is the hard requirement, `kernel`/`erts` are
best-effort, exactly as in the oracle's answer), and the directories keep the `SYSTEM_APPS` order
(`stdlib`, `kernel`, `erts`).
Candidate roots, most specific first: the `erl` on `$PATH` — **resolved to a filesystem path and never
executed**, so the installation this runtime is meant to be interchangeable with wins — then this
executable's ancestors (an OTP source tree above `erts_rust/target/…`), then `$ERL_TOP`, then
`/usr/lib/erlang`, `/usr/local/lib/erlang`, `/usr/lib64/erlang`, `/opt/erlang`; every seed is also tried as
`<seed>/lib/erlang` (the `--prefix` release root, e.g. the Nix store layout).
The `erl` shell-out is kept because no filesystem probe can see *every* installation: a version manager
(`asdf`, `kerl`) may expose a **shim script** on `$PATH` while keeping the release under a layout reachable
from neither the shim nor this executable, and a wrapper `erl` may live outside the release it starts.
Asking `erl` is authoritative, and a path is never fabricated: if neither probe produces a stdlib the
resolution fails with the typed `cannot locate the system code path: …` error (exit 1). Consequence: a
normal installation pays **no** `erl` start-up (measured on this host: a bare `-eval` invocation now
takes ≈0.44 s wall, since it drives the default boot script on every action, where the resident `erl`
shell-out alone costs ≈0.5 s and is still avoided).
**The resolved root is the consumed SYSTEM install, not this runtime's own tree.** `native_roots`
seeds candidate roots with the `erl` on `$PATH` first (canonicalized, never executed), then
`current_exe()` ancestors, `$ERL_TOP` and the standard prefixes; `bootstrap/` is never a code-path
candidate root (it holds the primary bootstrap, not an app layout). So `CodeServer::lib_root()` is the
*system* install's `<lib>` and
`../cli.rs::boot_file_root` (its parent) is the system install root — used for an explicit `-boot`
value and for a boot script's `$ROOT` expansion, exactly as before.
The **default** route (no `-boot`) does *not* go through it: `../cli.rs::boot_script_to_drive` prefers
`CodeServer::own_bootstrap_boot_script()` — the committed `bootstrap/bin/start.boot` of the source tree
the binary runs from — and only falls back to `-boot start` (resolved to `<systemRoot>/bin/start.boot`)
when no source tree is reachable. The default route therefore drives the repo's OTP-30 script (header
`{"Erlang/OTP","30"}`), so `init:script_id/0` (`../native/init.rs::script_id_0` ←
`Runtime::boot_script_id`) agrees with `erlang:system_info(otp_release)`
(`../../beam_common/src/version.rs::OTP_VERSION`, `"30"`). An explicit `-boot FILE` still drives *that*
script and reports its header (the consumed OTP-29 install's `{"Erlang/OTP","29"}` on this host).

## Constraints
- **No silent skip.** A preloaded `.beam` that is missing or malformed is a typed `BeamError` (a missing
  one is the oracle's bare `error:undef`); the preloaded `.beam`s ship in the **erts** application's
  `ebin`, resolved at run time by `CodeServer::add_system_paths`. The boot script follows the same rule: a
  `primLoad`/`module`/`preLoaded` name on no code path is the code server's `error:undef`, an unknown
  directive is a typed `unsupported`, and a malformed image is a typed `error::message` (never a partial
  parse, never a wrong value).
- **Load is driven by the registry**, never a second copy of the list; `PreloadedModules` stays the single
  source of truth for the set and its order.
- Additive only: `PRELOADED_MODULE_NAMES`, `CORE_ATOMS`, `seed_core_atoms` and `Runtime::preloaded()`
  keep their existing semantics/return types; `boot()` stays idempotent and returns `Result<()>`;
  `execute_reporting` is purely additive (`execute` keeps its signature).
- **The atom seed is the materialization source for every atom-valued answer**, so `CORE_ATOMS` is
  **append-only**: the group order fixes the atom indices, and a seeded *prefix* must resolve every
  name it contains identically (asserted by `boot.rs`'s `core_atoms_seed_is_idempotent_and_complete`
  + `seed_core_atoms_interns_the_exception_reason_and_class_atoms`). A name an `InfoValue::Atom` can
  emit but the seed lacks surfaces as `term not found` (`BeamError::TermNotFound`) instead of the
  answer, which is why each appended group exists: the C `error_atom` table
  (`beam_common.c:382-406`), the stacktrace auxiliaries `file`/`line`, the node atom
  `nonode@nohost`, the `bm`/`ac` pattern tags, the `get_module_info/1,2` item and `Attr`/`CInf`
  compiler keys, the `+sbt` scheduler bind-type names `erlang:system_info(scheduler_bind_type)`
  reports (`spread`, `processor_spread`, `thread_spread`, `thread_no_node_processor_spread`,
  `no_node_processor_spread`, `no_node_thread_spread`, `no_spread` — `unbound`, the `u` spelling and
  the no-`+sbt` default, is seeded through `INFO_ATOMS`), and the `RuntimeHooks` timer tag `timeout`.
- **Drives stay bounded** — an exhausted deadline is an honest typed error, never a spin.
- **The `DRIVE_DEADLINE` bounds the outer drive, not a nested blocking pump.** A
  `{kernelProcess, …}` burst and an `{apply, …}`'s `Wait` arm both go through
  `drive_scheduler`, which checks the deadline between slices. An execution that suspends
  *inside* a `RuntimeContext` blocking pump instead — `RunOutcome::Wait` handled by
  `run_export_blocking`/`run_label_blocking` (`../context/mod.rs`) — is bounded by that pump's
  own `BLOCKING_PUMP_DEADLINE` (checked between pump rounds, ending in a typed diagnostic
  rather than a spin) and, earlier, by a pump that reports `0`; the nested cap is documented in
  `../context/CONTEXT.md`.
- `bootscript.rs` is ~1855 lines (production lines 1–724, the rest its oracle test module) — the
  crate's documented long-file precedent; `drive.rs` is ~423 lines. Both are `#![deny(unsafe_code)]`;
  no `unwrap()` on runtime data.

## Test Strategy
- `boot/preloaded.rs`: `boot_loads_every_preloaded_module_into_the_cache` (all 22 cached;
  `erlang`/`init`/`erts_internal` present), `a_second_load_is_a_no_op` (idempotent), and
  `a_missing_preloaded_module_is_a_typed_error_not_a_silent_skip` (empty code path ⇒ `error::undef`,
  nothing cached). System-install tests **skip loudly** (clear `eprintln!` + early `return`) when no
  Erlang is installed — never silently pass.
- `boot/bootscript.rs`: `reads_the_real_start_boot_into_the_oracle_instruction_sequence` (parses the
  committed `bootstrap/bin/start.boot`; pins `{"Erlang/OTP","30"}`, 22 instructions and the instruction
  list); the bounded prefix test `the_real_start_boot_prefix_loads_its_prim_load_batches`; 
  `a_synthetic_script_executes_end_to_end` (compiles a fixture with `erlc`, writes a synthetic
  `.boot` with `erl`, asserts the `apply` step really ran); `a_boot_spawned_process_answers_a_blocked_apply`
  (pins `drive.rs`: a `{kernelProcess, …}` burst and a later `{apply, …}` blocked on it, resumed by a
  scheduler drive, its dictionary side effect read back through `runtime.eval`); 
  `a_malformed_boot_script_is_a_typed_error`; the full-script pins
  `the_real_start_boot_completes_through_execute_reporting` (asserts all 22 directives ending at
  `{progress,started}`) and
  `the_real_boot_registers_a_live_logger_process` (instruction 13 leaves a live `logger` registered, and
  a second `logger_server:start_link/0` reports `{error,{already_started,Pid}}`); the burst pins
  `a_kernel_process_burst_waits_for_the_directive_to_return` (a fixture whose `serve/0` `spawn_link`s a
  server that registers the directive's name **first** and only then does its real setup, and which
  registers a *second* visible name only after the server acks — so a later `{apply, …}` answers `acked`
  only when the burst drove the directive's MFA to its end; it fails with `not_acked` against a burst
  that returns at the first `whereis` resolution) and
  `a_kernel_process_registers_its_own_directive_name` (a synthetic boot whose `{kernelProcess,Name,…}`
  entry registers `Name` itself and then parks in a plain `receive`, so that burst ends on the
  run-queue drain, later resolved via `whereis`), plus `registering_a_live_boot_name_is_badarg`.
  Every test reads the committed script via
  `concat!(env!("CARGO_MANIFEST_DIR"), "/../../../bootstrap/bin/start.boot")`; system-install tests
  **skip loudly** when `erl`/`erlc` or the file are absent. The in-crate tests hand that path to `read`,
  never as a `-boot` CLI value, so its shape is not a factor here.
- **The system-toolchain spawns are fixture generators, not live comparators; only three exist, all in
  `bootscript.rs`.** `have_system_erlang()` (`erl -noshell -eval halt(0).`, PATH lookup, line 741)
  probes availability; `build_fixture` compiles a fixture with `erlc -o <dir> <src>` (line 1365) and
  writes the synthetic `.boot` with `erl -noshell -eval <expr … halt(0).>` (line 1388). Every spawn is
  blocking (`Command::output()`); bounding is the tool's own `halt(0).`/exit self-termination, not a
  poll/kill, and the result is only checked for `status.success()` (stderr quoted in the skip line).
  **No test compares live oracle stdout** — `reads_the_real_start_boot_into_the_oracle_instruction_sequence`
  compares the parsed script against a hard-coded golden vector, and every other assertion is on runtime
  state (`runtime.module`, `whereis`, the `runtime.eval` side effects). No spawn sites in `boot.rs`,
  `drive.rs` or `preloaded.rs` (whose `system_server()` skips on a `CodeServer::add_system_paths()`
  error, a pure filesystem probe).
- `boot/drive.rs`: **no in-file `#[cfg(test)]` module** (0 `#[test]`s) — the bounded burst/`drive_scheduler`
  logic is pinned indirectly by `bootscript.rs`'s synthetic-script tests
  (`a_boot_spawned_process_answers_a_blocked_apply`, `a_kernel_process_registers_its_own_directive_name`,
  `a_kernel_process_burst_waits_for_the_directive_to_return`, `registering_a_live_boot_name_is_badarg`)
  and by `../tests/boot.rs` (`the_boot_chains_kernel_processes_survive_their_directive_processes`).
  The **imminent-only wait policy** the drives now use is pinned in `../runtime/scheduler/tests.rs`:
  `the_imminent_only_wait_policy_sleeps_for_an_imminent_deadline_only` (the `TimerWait::ImminentWithin`
  unit: no deadline → `None`; an hour out → `None`; 20 ms out → the remaining time, clamped to the
  window; one wait per pass), `a_boot_drive_step_does_not_sleep_on_a_far_future_deadline` (a
  `pump_scheduler_boot` step with a 1 h timer returns `0` in well under 100 ms, timer untouched) and
  `a_boot_drive_step_sleeps_to_an_imminent_deadline_and_fires_it` (a 5 ms timer is slept to, the parked
  receiver is resumed and the message delivered).
- In-file `#[test]` counts: `bootscript.rs` **12** (lines 725–1855), `preloaded.rs` **3** (lines 75–143),
  `drive.rs` **0** — directory total **15**; the module root `../boot.rs` adds **9** (its own
  `#[cfg(test)] mod tests`).
- `../runtime.rs`: `boot_is_idempotent_and_reports_itself` pins `preloaded_loaded()` = the whole registry;
  `boot_from_script_reporting` is the boot-stop-position entry point, and the in-crate boot-script suite
  exercises the `execute_reporting`/`BootStop` report API a genuine future stop would use.
- `../tests/boot.rs` (integration, owned by this node): `start_boot()` builds the `-boot` value as a
  canonical absolute path (`<repo>/bootstrap/bin/start`) from `CARGO_MANIFEST_DIR`'s ancestors and, with
  the CLI-equivalent configuration, drives the real script to completion
  (`the_real_start_boot_runs_to_completion` asserts the full 22-directive run to `{progress,started}`;
  the `primLoad` batches through index 17 really loaded); the
  `-boot` CLI drive of a synthetic script; the CLI running the real `start.boot`
  (`the_cli_runs_the_real_boot_script_to_completion` asserts exit **0** with empty stdout/stderr);
  a synthetic-probe boot (the real script's first 20 directives + a `probe:run/0`
  `apply`, built with the oracle) proving the `application_controller` kernel process survives its
  short-lived directive process (`whereis` = a live pid); the CLI proving the real boot leaves a
  live `logger` registered (`the_cli_boot_registers_a_live_logger_process`); and a bare `-noshell`
  invocation with no `-boot` (exit **0** with empty stdout and stderr, its default boot script
  producing no output).

## Notes for Agents — the `file_server_2` service
The runtime-native **`file_server_2` service** (`../native/file_server.rs`) is spawned unregistered on every route that drives a boot script, and supplies the service under that name only where the runtime drives none.
The service is a `beam_process::Process` born `Runnable` and never enqueued (the `init`/`self_pid` precedent), so the runner never executes it and `ProcessEntry` needs no new variant.
`Runtime::service_file_requests` drains its mailbox as an additive `scheduler_pass` step and delivers the replies, which is what lets a `gen_server:call(file_server_2, …)` to the native service complete.
**It is registered under `file_server_2` only where the runtime drives no boot script.** `Runtime::new` consults `RuntimeConfig::drives_a_boot_script()` and, whenever it holds — every CLI action but `-version`, plus an explicit `-boot FILE` — spawns the service **unregistered** (`FileServices::start_unclaimed`) so that the booted `kernel_sup`'s own `file_server_2` child owns the name — an initial child start that finds the name already taken aborts that supervisor, and `kernel` is `permanent`.
On such a route the boot chain's final `{apply,{c,erlangrc,[]}}` directive (index 20) is answered by that booted child — OTP's own `file_server`/`file_io_server` Erlang — not by the native service; `c:erlangrc/0`'s `gen_server:call` at instruction 20 completes through it.
The request/reply surface, the errno mapping, the io-device and the remaining gaps live in [`../native/CONTEXT.md`](../native/CONTEXT.md)'s `file_server.rs` subsection.
- **The native service is used instead of the real `file_server`.** The real Erlang
  `file_server`/`file_io_server` needs the wider `prim_file` NIF surface this runtime does not serve;
  the native `std::fs`-backed service answers the requests it can honour faithfully and returns the
  typed `unsupported` gap for the rest (below).
- **The native service is deliberately narrow.** It serves the `file_server`/`file`/`io` requests a
  native `std::fs` host (or, for the two incremental readers, the engine) can answer faithfully —
  including the read-mode surface (`getopts`/`{setopts, Opts}`) and the write path — and returns the
  typed `unsupported` gap for the rest: `pread`/`pwrite`/`advise`/`allocate`/…, a non-`latin1`
  `{get_chars,…}`/`{put_chars,…}`, a `{setopts,…}` selecting an unmodelled `utf16`/`utf32` encoding,
  and a request `Enc` that is neither `latin1` nor `unicode`.

## Routing Table
- `../boot.rs` → registry + core-atom seed + banner (module root; also `mod preloaded` + `mod bootscript` + `mod drive`)
- `./preloaded.rs` → the real preloaded `.beam` loading (`load_preloaded`/`load_names`)
- `./bootscript.rs` → the OTP boot-script reader (`read`/`BootScript`/`Instruction`/`Mfa`/`Value`) + executor (`execute`/`execute_reporting`/`BootStop`)
- `./drive.rs` → the bounded scheduler-drive helpers (`drive_scheduler`/`start_kernel_process`/`apply_boot_call`, plus `DRIVE_DEADLINE`/`KERNEL_BURST_*`/`APPLY_BLOCK_SLICES`)
- `../runtime.rs` → `Runtime::boot` / `boot_if_needed` / `preloaded_loaded` / `is_preloaded_loaded`, and the boot-script glue `boot_from_script` / `load_code_module` / `add_code_path` / `register_boot_process` / `unlink_processes`
- `../cli.rs` → `boot_script_to_drive`/`boot_then` (the resolved boot script every action but `-version` drives) and the `Action::Boot` arm that drives `boot_from_script`
- `../codeserver.rs` → `CodeServer::{add_system_paths, load, is_loaded, loaded}` (the cache this feeds)
- `../native/code.rs` → the `code:*` surface that reports `preloaded` from the name registry (not owned here)
- `../native/file_server.rs` → the native `file_server_2` service, registered under that name only where the runtime drives no boot script (not owned here; see `../native/CONTEXT.md`)
- `../context/mod.rs` → `RuntimeContext::run_export`/`run_export_blocking` and `resolve_external_call` (the in-machine seam the boot's `{apply, …}` chains are entered through; read-only from this node)
