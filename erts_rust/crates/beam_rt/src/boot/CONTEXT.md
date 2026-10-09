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
  reason is rendered as its real term through the runtime's printer — e.g.
  `{noproc,{gen_server,call,[file_server_2,…]}}` rather than `<structured reason>`; a non-structured or
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
  `drive_scheduler` over the crate-internal `Runtime::pump_scheduler`, with per-call slice budgets
  between deadline checks and **one** wall-clock `DRIVE_DEADLINE` (10 s) shared by the whole `execute`
  pass, plus a 1 s `KERNEL_BURST_DEADLINE` cap per burst. An exhausted deadline is a typed
  `error::message`; a blocked apply whose drive ran nothing is the engine's typed `error::blocked()`; a
  drive stops as soon as a `pump_scheduler` call reports `0`.
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
Execute the real script (`bootstrap/bin/start.boot`, 22 instructions) and it runs **instructions 0–19**
(every `primLoad` batch, the three `kernelProcess` directives and the three earlier `apply` directives)
and stops at **instruction 20** (0-based), `{apply,{c,erlangrc,[]}}`. The tail's 0-based indices:
**12** `{kernelProcess,heart,{heart,start,[]}}`, **13**
`{kernelProcess,logger,{logger_server,start_link,[]}}`, **14**
`{kernelProcess,application_controller,{application_controller,start,[KernelApp]}}`, **15**
`{progress,init_kernel_started}`, **16** `{apply,{application,load,[{application,stdlib,Props}]}}`,
**17** `{progress,applications_loaded}`, **18**/ **19**
`{apply,{application,start_boot,[{application,kernel|stdlib,Props},permanent]}}`, **20**
`{apply,{c,erlangrc,[]}}` (the stop), **21** `{progress,started}`.
- **The CLI and the in-crate executor stop at the same directive, with the same exit class.** The CLI
  (`beam_rt -noshell -boot bootstrap/bin/start`, run from the repository root) exits **1** with
  `{noproc,{gen_server,call,[file_server_2,{open,"$HOME/.erlang",[read]},infinity]}}` — `file_server_2`
  is simply never registered, so `c:erlangrc/0`'s file consult fails. The in-crate executor
  (`execute_reporting` + `BootStop::describe_with`) reports the **same** term:
  `boot script stopped at instruction 20/22: c:erlangrc/0: {noproc,{gen_server,call,[file_server_2,…]}}`
  — the same directive and the same exit class.
- **The configuration is part of the measurement.** The stop depends on the runtime's **complete**
  configuration, above all the command line `init:get_arguments/0` reports. The executor tests build
  their runtime with their own `system_runtime()` helper — the CLI's own parser (`crate::cli::parse`,
  not `parse_with_env`: a test must not depend on `ERL_*FLAGS`) over the CLI argv
  `beam_rt -noshell -boot bootstrap/bin/start` — and `tests/boot.rs` mirrors it (`boot_cli_runtime`),
  because a `RuntimeConfig::default()` runtime (an empty `init_args`, so `init:get_arguments/0` is `[]`
  where the oracle also reports `root`/`bindir`/`progname`/`home`) stops three instructions earlier, at
  instruction 16 (`application:load/1`, an `error function_clause`) — see the `src/native` handoff in
  the blocker chain below.
- **The kernel processes are ready when the stop is reached.** The three `{kernelProcess, …}` bursts
  drive until their directive's MFA returns, so `whereis(application_controller)` answers a live,
  registered server — the directive ran `application_controller:start/1` to its `{ok, AC}` return
  (`application_controller.erl:203-211`), i.e. the AC completed `init/2` (`:531-558`) and reached
  `gen_server:enter_loop`. `heart` and `logger` are not registered — both legitimately (heart returns
  `ignore`; logger never reaches registration). Instruction 21 is unreached.
- **The stop's fold is `apply_boot_call`'s ordinary dispatch, not its `RunOutcome::Wait` arm.**
  `c:erlangrc/0` runs to the typed `noproc` exit inside the AC's `gen_server:call`, so no
  `Wait { func, pc }` is produced and `apply_boot_call`'s `Wait` arm (and its `ran == 0` fold) is
  unreached on this path.

### A `-boot` value's shape changes the observed stop (owner: `beam_engine` — handoff)
The stop is a function of the whole command line, and the `-boot` **value's byte length** perturbs it on
a knife edge: the short canonical form (`<repo>/bootstrap/bin/start`, ~71 chars) carries the script
cleanly to instruction 20, while a slightly longer value — or one with a non-canonical segment (`.`, `..`,
a doubled `/`), which shifts the byte count just as a longer name does — makes the runtime exit **1** with
the raw `beam_rt: allocation failure: message (<garbage> bytes)` instead of the honest `{noproc,…}` (the
count is deterministic for a given argv but varies with it: `21096`, `42408`, `17123030415440`, …).
`$P/bootstrap/bin/start` (71) is fine, `$P/bootstrap/./bin/start` (74) fails, and a padded long canonical
path fails too — so it is heap layout, not path semantics. Pinned by inspection to `beam_engine`'s
structured-raise path: `Machine::throw_with_stacktrace` (`machine/catch.rs`) attaches a `LiveReason` over
a term whose source heap frame is already gone, and a later copy (`beam_process` → `beam_message`'s
`size_object`) reads the freed frame as a length. **`src/boot/**` cannot fix it.** `tests/boot.rs`
therefore builds its `-boot` value from `CARGO_MANIFEST_DIR`'s ancestors (`start_boot()`), whose canonical
short form is the safe one. Repro:
`ERL_CRASH_DUMP_SECONDS=0 beam_rt -noshell -boot "$PWD/bootstrap/./bin/start"` →
`beam_rt: allocation failure: message (…)`; `… -boot "$PWD/bootstrap/bin/start"` → the `file_server_2`
`noproc`.

### Remaining blocker chain (owner per link)
1. **`beam_rt` / `kernel` (owner: the parent node, not `src/boot/**`) — no `file_server_2`.** The stop
   is `c:erlangrc/0`'s file consult needing the `file_server_2` registered kernel server, which this
   runtime never registers (it is a child of `kernel`'s `file_server`, started by the full
   `application:start_boot` path this runtime does not complete).
2. **`beam_engine`.** `Instr::Apply`/`ApplyLast` (`families/funs.rs::apply_target`) dispatch through the
   synchronous `call_bif` instead of the `resolve_external_call`/`Step::Enter` seam, so
   `proc_lib:init_p/5`'s `try apply(M,F,A) catch … end` cannot suspend; and a `func_info`-driven raise
   carries an EMPTY `Raise.stacktrace.frames`, so a boot-path `function_clause` is unlocalizable.
3. **`beam_bifs`.** Whatever the started applications still need past the above.

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
- **The `DRIVE_DEADLINE` bounds the outer drive, not a nested blocking pump.** A
  `{kernelProcess, …}` burst and an `{apply, …}`'s `Wait` arm both go through
  `drive_scheduler`, which checks the deadline between slices. An execution that suspends
  *inside* a `RuntimeContext` blocking pump instead — `RunOutcome::Wait` handled by
  `run_export_blocking`/`run_label_blocking` (`../context/mod.rs`) — loops until the machine
  returns or a pump reports `0`, with no wall-clock cap, so a perpetually runnable process on
  the queue would keep that pump going. On the real `start.boot` every boot-spawned process is
  parked in a `receive`, so the queue drains and the pump returns; the gap is a latent one for
  a boot chain that would spawn a busy process. The cap belongs in `../context/mod.rs` (this
  node's, but not `src/boot/**`).
- `bootscript.rs` is ~1756 lines (production ~700, the rest its oracle test module) — the crate's
  documented long-file precedent; `drive.rs` is ~382 lines. `#![deny(unsafe_code)]`; no `unwrap()` on
  runtime data.

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
  `BeamError::Raise` on the `{apply,{c,erlangrc,[]}}` directive at **index 20**, and that the `primLoad`
  modules loaded); `the_real_start_boot_reports_its_stop_through_execute_reporting` (the `execute_reporting`
  API: `stop.total == instructions.len()`, `stop.index == Some(20)`, the named `Apply` is `c:erlangrc/0`,
  and `whereis(application_controller)` is a live pid — pinning the
  unlink); `a_synthetic_script_executes_end_to_end` (compiles a fixture with `erlc`, writes a synthetic
  `.boot` with `erl`, asserts the `apply` step really ran); `a_boot_spawned_process_answers_a_blocked_apply`
  (pins `drive.rs`: a `{kernelProcess, …}` burst and a later `{apply, …}` blocked on it, resumed by a
  scheduler drive, its dictionary side effect read back through `runtime.eval`); 
  `a_malformed_boot_script_is_a_typed_error`; the burst pins
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
- `../runtime.rs`: `boot_is_idempotent_and_reports_itself` pins `preloaded_loaded()` = the whole registry;
  `boot_from_script_reporting` is the boot-stop-position entry point.
- `../tests/boot.rs` (integration, owned by this node): `start_boot()` builds a canonical, short `-boot`
  value (avoiding the heap-fragility above) and asserts the real-script stop is `stop.index == Some(20)`
  on the `{apply,{c,erlangrc,[]}}` directive, reporting the real `{noproc,…}` term; the `-boot` CLI drive;
  the CLI's honest-stop diagnostic (`file_server_2` `noproc`, and **not** `application_controller`); a
  synthetic-probe boot (the real script's first 20 directives + a `probe:run/0` `apply`, built with the
  oracle) proving the `application_controller` kernel process survives its short-lived directive process
  (`whereis` = a live pid); and the preloaded-only default.

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
