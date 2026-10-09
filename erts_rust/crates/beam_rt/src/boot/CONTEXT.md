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
  error), and `pub fn execute_reporting(&mut Runtime, &BootScript) -> std::result::Result<(), BootStop>`
  — the **primary** implementation, which `execute` delegates to (`execute(..).map_err(|stop| stop.error)`),
  so the two can never drift. `index`/`instruction` are `None` when the runtime failed to boot before
  the first directive ran.
- `boot/drive.rs`: `start_kernel_process`, `apply_boot_call`, `drive_scheduler`, and the budgets
  `DRIVE_DEADLINE` (per `execute` pass), `KERNEL_BURST_DEADLINE`, `KERNEL_BURST_SLICES` (=1),
  `APPLY_BLOCK_SLICES`.
- Consumed by `Runtime::boot` (`../runtime.rs`), driven by `Runtime::boot_if_needed` on every public
  entry point; `Runtime::boot_from_script` drives `execute`; `Runtime::preloaded_loaded()` /
  `is_preloaded_loaded(name)` report the loaded set. **The CLI really runs a boot script**: a `-boot
  FILE` invocation selects the `Action::Boot` arm (`../cli.rs::drive`), which calls `Runtime::boot()`
  (the 22 preloaded `.beam`s) and then `Runtime::boot_from_script(&path)`.
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
| `{kernelProcess,Name,{M,F,A}}` (3) | `KernelProcess` | materialize args, spawn, then drive a bounded burst so the process registers `Name` **itself**; then **unlink** it from the short-lived directive process |
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
  `{kernelProcess, …}` spawns and then runs a bounded burst that ends as soon as the directive's `Name`
  resolves through `whereis`, the run queue drains, or the burst deadline expires; a burst that ends
  without the name is deliberately **not** this directive's error (matching `init`, which never checks —
  the failure surfaces at the next dependent directive). An `{apply, …}` runs the export through the
  engine's blocking-aware entries and drives the scheduler whenever the call suspends, resuming it once a
  boot-spawned process answers. Both drives go through `drive_scheduler` over the crate-internal
  `Runtime::pump_scheduler`, with per-call slice budgets between deadline checks and **one** wall-clock
  `DRIVE_DEADLINE` (10 s) shared by the whole `execute` pass, plus a 1 s `KERNEL_BURST_DEADLINE` cap per
  burst. An exhausted deadline is a typed `error::message`; a blocked apply whose drive ran nothing is
  the engine's typed `error::blocked()`; a drive stops as soon as a `pump_scheduler` call reports `0`.
  A capability gap stays an `error::unsupported` raise.

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

## Where the real `start.boot` stops now
Execute the real script (`bootstrap/bin/start.boot`, 22 instructions) and it runs **instructions 0–15**
(every `primLoad` batch and the three `kernelProcess` directives, each driven to its process's own
registration) and stops at **instruction 16** (0-based). The tail's 0-based indices:
**12** `{kernelProcess,heart,{heart,start,[]}}`, **13** `{kernelProcess,logger,{logger_server,start_link,[]}}`,
**14** `{kernelProcess,application_controller,{application_controller,start,[KernelApp]}}`,
**15** `{progress,init_kernel_started}`, **16** the `apply` below.

```
{apply,{application,load,[{application,stdlib, Props}]}}
```

- **Failing operation:** `application:load/1`.
- **Exit reason:** `unsupported: opcode: run: process blocked (no message available)` — the engine's
  typed `beam_engine::blocked()`. The CLI (`beam_rt -noshell -boot bootstrap/bin/start`) prints exactly
  `beam_rt: unsupported: opcode: run: process blocked (no message available)` and exits **3**. The
  in-crate executor agrees: `BootStop::describe()` →
  `boot script stopped at instruction 16/22: application:load/1: error unsupported_opcode: run: process blocked (no message available)`.
- **Prerequisite now in place:** with the unlink, `whereis(application_controller)` answers a live pid
  after the boot (the server survives its directive process). `heart` and `logger` are not registered —
  both legitimately (heart returns `ignore`; logger never reaches registration). Instructions 17–21 are
  unreached.

### Remaining blocker chain (owner per link)
1. **`beam_rt` / `./drive.rs::apply_boot_call` — the stop's fold is `drive.rs:322`, the only writer on
   this path.** The top-level machine really suspends at the `application:load/1` chain's `gen:do_call/4`
   `receive` (`application.erl:216,269` → `application_controller.erl:220-221` → `gen_server:call/3` →
   `gen:call/4` → `gen:do_call/4`, all entered **in-machine** via `resolve_external_call`/`Step::Enter`),
   so `RunOutcome::Wait { func, pc }` reaches `drive.rs:318`'s
   `drive_scheduler(runtime, deadline, APPLY_BLOCK_SLICES)`. The drive loops `pump_scheduler(256)` until
   one call reports `0` (`drive.rs:247-250`); `ran == 0` (nothing ran ⇒ the run queue drained with no
   live port and no pending timer) hits `drive.rs:319-323`
   `if ran == 0 { return Err(beam_engine::blocked()); }`. `beam_engine::blocked()` renders
   `unsupported: opcode: run: process blocked (no message available)` (CLI exit 3).
   **Correction:** the previously recorded blocker — `RuntimeContext::run_export_blocking`
   (`context/mod.rs:627-651`, folds at `:641`/`:645`) — is **not** this stop; that entry was observed
   only for `gen:init_it/7` on the kernel processes' own start path. A nested `call_bif`-routed callee
   would have folded there (its message is byte-identical), but the top-level machine really suspended,
   so this stop is `drive.rs:322`.
2. **`beam_rt` / `./drive.rs::start_kernel_process` (`drive.rs:185-213`) — the burst's stop criterion,
   the ordering divergence behind the non-answering `application_controller`.** The burst
   `return Ok(())` the instant `whereis(name)` resolves (`drive.rs:195-206`), before pumping further. But
   `application_controller:init/2` opens with `register(?AC, self())` (`application_controller.erl:531`),
   so with `KERNEL_BURST_SLICES = 1` the burst can return with the server registered but its init —
   `process_flag`, `check_conf`, `ets:new(ac_tab,…)`, `make_appl(Kernel)`, `load(S, KAppl)`,
   `Init ! {ack,…}`, `gen_server:enter_loop/4` (`application_controller.erl:532-558`) — incomplete. The
   oracle has no such race: `init` runs `{kernelProcess, …}` as `apply(Mod,Fun,Args)` **synchronously**
   (`init.erl:1520-1522`) and `application_controller:start/1` blocks in `receive` until its child acks
   (`application_controller.erl:204-211`), so real `init` reaches instruction 16 only after the ack (the
   server has entered `enter_loop`). **Fix shape:** unlink on the first `whereis` resolution (unchanged),
   then keep driving — bounded by the existing run-queue-drain (`pump_scheduler == 0`) and
   `KERNEL_BURST_DEADLINE` stops — until the **directive process terminates** (its MFA returned) rather
   than returning at the name. Necessary; **not proven sufficient**: a server merely
   registered-but-not-in-loop would still be run to completion by the index-16 drive, so the fold also
   needs the server non-runnable then. `bootscript.rs::the_real_start_boot_reports_its_stop_through_execute_reporting`
   pins only that `whereis(application_controller)` resolves **after** the stop (alive + registered),
   which a parked-but-alive server satisfies. If the server is parked on a genuine mid-init block, the
   burst change cannot unblock it and that block is outside this node.
3. **`beam_rt` / `tests/**` (owner: the parent node).** `tests/boot.rs::the_cli_reports_the_boot_scripts_honest_stop_as_a_typed_error`
   (line 507) still asserts the OLD stop — exit **1** with `noproc` in stderr. The stop is now
   exit **3** with `unsupported: … blocked`; the assertion must be updated to the new honest stop.
4. Benign remnants for instruction 16: `heart:init/2`'s `catch start_portprogram()` does not contain
   `check_start_heart/0`'s `throw(no_heart)`; `init:get_argument/1` is empty in the boot path.

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
- **Drives stay bounded** — an exhausted deadline is an honest typed error, never a spin.
- `bootscript.rs` is ~1580 lines (production ~700) with its oracle test module; `drive.rs` is ~350 lines.
  `#![deny(unsafe_code)]`; no `unwrap()` on runtime data.

## Test Strategy
- `boot/preloaded.rs`: `boot_loads_every_preloaded_module_into_the_cache` (all 22 cached;
  `erlang`/`init`/`erts_internal` present), `a_second_load_is_a_no_op` (idempotent), and
  `a_missing_preloaded_module_is_a_typed_error_not_a_silent_skip` (empty code path ⇒ `error::undef`,
  nothing cached). System-install tests **skip loudly** (clear `eprintln!` + early `return`) when no
  Erlang is installed — never silently pass.
- `boot/bootscript.rs`: `reads_the_real_start_boot_into_the_oracle_instruction_sequence` (parses the
  committed `bootstrap/bin/start.boot`; pins `{"Erlang/OTP","30"}`, 22 instructions and the instruction
  list); the bounded prefix test `the_real_start_boot_prefix_loads_its_prim_load_batches`; 
  `the_real_start_boot_stops_honestly_at_the_first_unbootable_directive` (asserts the stop is a typed
  `BeamError::Raise` on an `Instruction::Apply` at index ≥ 16 — it is 16 — and that the `primLoad`
  modules loaded); `the_real_start_boot_reports_its_stop_through_execute_reporting` (the `execute_reporting`
  API: `stop.total == instructions.len()`, `stop.index` `Some(index)` with `index >= 16`,
  `stop.instruction` is `Apply`, and `whereis(application_controller)` is a live pid — pinning the
  unlink); `a_synthetic_script_executes_end_to_end` (compiles a fixture with `erlc`, writes a synthetic
  `.boot` with `erl`, asserts the `apply` step really ran); `a_boot_spawned_process_answers_a_blocked_apply`
  (pins `drive.rs`: a `{kernelProcess, …}` burst and a later `{apply, …}` blocked on it, resumed by a
  scheduler drive, its dictionary side effect read back through `runtime.eval`); 
  `a_malformed_boot_script_is_a_typed_error`; and the registration pins
  `a_kernel_process_registers_its_own_directive_name` (a synthetic boot whose `{kernelProcess,Name,…}`
  entry registers `Name` itself, later resolved via `whereis`) and `registering_a_live_boot_name_is_badarg`.
  Every test reads the committed script via
  `concat!(env!("CARGO_MANIFEST_DIR"), "/../../../bootstrap/bin/start.boot")`; system-install tests
  **skip loudly** when `erl`/`erlc` or the file are absent.
- `../runtime.rs`: `boot_is_idempotent_and_reports_itself` pins `preloaded_loaded()` = the whole registry.
- `../tests/boot.rs` (integration, owned by the parent node): the real-script stop, the `-boot` CLI drive
  and the preloaded-only default. NOTE: `the_cli_reports_the_boot_scripts_honest_stop_as_a_typed_error`
  currently asserts the stale exit-1/`noproc` stop and fails against the current exit-3/`blocked` stop.

## Routing Table
- `../boot.rs` → registry + core-atom seed + banner (module root; also `mod preloaded` + `mod bootscript` + `mod drive`)
- `./preloaded.rs` → the real preloaded `.beam` loading (`load_preloaded`/`load_names`)
- `./bootscript.rs` → the OTP boot-script reader (`read`/`BootScript`/`Instruction`/`Mfa`/`Value`) + executor (`execute`/`execute_reporting`/`BootStop`)
- `./drive.rs` → the bounded scheduler-drive helpers (`drive_scheduler`/`start_kernel_process`/`apply_boot_call`, plus `DRIVE_DEADLINE`/`KERNEL_BURST_*`/`APPLY_BLOCK_SLICES`)
- `../runtime.rs` → `Runtime::boot` / `boot_if_needed` / `preloaded_loaded` / `is_preloaded_loaded`, and the boot-script glue `boot_from_script` / `load_code_module` / `add_code_path` / `register_boot_process` / `unlink_processes`
- `../cli.rs` → the `Action::Boot` arm (`-boot FILE`) that drives `boot_from_script`
- `../codeserver.rs` → `CodeServer::{add_system_paths, load, is_loaded, loaded}` (the cache this feeds)
- `../native/code.rs` → the `code:*` surface that reports `preloaded` from the name registry (not owned here)
- `../context/mod.rs` → `RuntimeContext::run_export`/`run_export_blocking` and `resolve_external_call` (the in-machine seam the `application:load/1` chain is entered through); `run_export_blocking` is **not** the current stop's fold (read-only from this node)
