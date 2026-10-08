# `beam_rt` — boot subsystem (`src/boot.rs` + `src/boot/`)

## Intent
The **boot sequence** of the runtime (contract **A.7**, OTP-app contracts). `src/boot.rs` owns the
preloaded-module *name registry* (`PRELOADED_MODULE_NAMES` / `PreloadedModules`), the core-atom seed
(`CORE_ATOMS` / `seed_core_atoms`) and the version banner; `src/boot/preloaded.rs` (module
`crate::boot::preloaded`) is what actually **loads** the preloaded `.beam`s into the shared module
cache. `src/boot/bootscript.rs` (module `crate::boot::bootscript`) is the **OTP boot-script reader +
executor**: it parses a `.boot` file into a pure-Rust `BootScript` and executes its directives against
a `Runtime`. `src/boot.rs` is the module root — this directory holds the loader + boot-script
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
- Consumed by `Runtime::boot` (`../runtime.rs`), which is driven by `Runtime::boot_if_needed` on every
  public entry point. `Runtime::preloaded_loaded()` / `is_preloaded_loaded(name)` report the loaded set.
- `code:which/1` / `code:all_loaded/0` / `code:is_loaded/1` render `preloaded` from the **name registry**
  (`../native/code.rs`), not from this cache; `code:all_loaded/0` explicitly filters preloaded names out
  of the cache so it does not list them twice.

## The boot script (contract A.7, part 2)
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
| `{kernelProcess,Name,{M,F,A}}` (3) | `KernelProcess` | materialize args, spawn, register `Name` |
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
- **The two process directives drive the scheduler — bounded, and it never hangs.** A
  `{kernelProcess, …}` spawns+registers and then runs a **bounded burst** (see below) so a long-lived
  server reaches its `receive`; an `{apply, …}` runs the export through the engine's blocking-aware
  entries and drives the scheduler whenever the call suspends, resuming it once a boot-spawned process
  answers. Both drives go through `drive_scheduler` over the crate-internal `Runtime::pump_scheduler`,
  with small per-call slice budgets (`KERNEL_BURST_SLICES` 64, `APPLY_BLOCK_SLICES` 256) between
  deadline checks and **one** wall-clock `DRIVE_DEADLINE` (10 s) shared by the whole `execute` pass,
  plus a 1 s `KERNEL_BURST_DEADLINE` cap per burst. An exhausted deadline is a typed `error::message`;
  a blocked apply whose drive ran nothing is the engine's typed `error::blocked()`; a drive stops as
  soon as a `pump_scheduler` call reports `0`. A capability gap stays an `error::unsupported` raise.
  A `{kernelProcess, …}` burst deliberately does **not** surface the spawned process's own crash —
  the directive's contract is spawn+register, so the failure surfaces at the next dependent directive.
- **Honest M1 stop.** The full real `start.boot` loads every `primLoad` batch (instructions 0–15 all
  succeed, incl. the three `kernelProcess` spawns and their bursts) and stops at **instruction 16**,
  `{apply,{application,load,[{application,stdlib,Props}]}}`, with a `BeamError::Raise` rendered
  **`exit noproc`**: its `application_controller:load_application/1` → `gen_server:call/2` finds the
  controller's registration gone because the controller **died during its burst**, its `proc_lib`
  start hitting the scheduler-boundary refusal `unsupported: erlang:spawn from inside a scheduler
  slice` (`../native/process.rs::check_spawnable`). Not a wrong value and not a hang. The chain that
  would advance it: (1) the `check_spawnable` refusal → so `application_controller` can `proc_lib`-spawn
  while scheduled; (2) a `gen_server` round trip that survives the process-dictionary divergence —
  `beam_process::Dict` keys by raw `Eterm` word, so a structurally-equal `$ancestors` key misses; (3)
  a live `application_controller`/`application:load/*` capable of the remaining `application` startup;
  (4) `application:start_boot/2` and `c:erlangrc/0`, the script's two later `apply` directives.

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
  keep their existing semantics/return types; `boot()` stays idempotent and returns `Result<()>`.
- `bootscript.rs` is ~540 production lines + its oracle test module (the crate's documented long-file
  precedent); `#![deny(unsafe_code)]`; no `unwrap()` on runtime data.

## Test Strategy
- `boot/preloaded.rs`: `boot_loads_every_preloaded_module_into_the_cache` (all 22 cached; `erlang`/`init`/
  `erts_internal` present), `a_second_load_is_a_no_op` (idempotent), and
  `a_missing_preloaded_module_is_a_typed_error_not_a_silent_skip` (empty code path ⇒ `error::undef`, nothing
  cached). System-install tests **skip loudly** (clear `eprintln!` + early `return`) when no Erlang is
  installed — never silently pass.
- `boot/bootscript.rs` (5 tests): `reads_the_real_start_boot_into_the_oracle_instruction_sequence` (parses
  the committed `bootstrap/bin/start.boot`; pins `{"Erlang/OTP","30"}`, 22 instructions, and the
  instruction list — the huge kernel/stdlib `primLoad` batches by length + first/last name, the
  `application_controller` and `application:load/1` args by shape); the bounded prefix test
  `the_real_start_boot_prefix_loads_its_prim_load_batches` (executes 0..first-`apply` and asserts the
  named modules loaded); `the_real_start_boot_stops_honestly_at_the_first_unbootable_directive` (executes
  instruction-by-instruction, asserts a typed `BeamError::Raise` at index ≥ 12 — it is 16 — and that the
  `primLoad` batches loaded); `a_synthetic_script_executes_end_to_end` (compiles a fixture with `erlc`,
  writes a synthetic `.boot` with `erl`, and asserts the `apply` step really ran — observable through the
  process dictionary); `a_malformed_boot_script_is_a_typed_error` (empty / random bytes / a valid-ETF
  wrong-shape term / a truncated real script). Every test reads the committed script via
  `concat!(env!("CARGO_MANIFEST_DIR"), "/../../../bootstrap/bin/start.boot")`; system-install tests **skip
  loudly** when `erl`/`erlc` or the file are absent.
- `../runtime.rs`: `boot_is_idempotent_and_reports_itself` pins `preloaded_loaded()` = the whole registry.

## Routing Table
- `../boot.rs` → registry + core-atom seed + banner (module root; also `mod preloaded` + `mod bootscript`)
- `./preloaded.rs` → the real preloaded `.beam` loading (`load_preloaded`/`load_names`)
- `./bootscript.rs` → the OTP boot-script reader (`read`/`BootScript`/`Instruction`/`Mfa`/`Value`) + executor (`execute`)
- `../runtime.rs` → `Runtime::boot` / `boot_if_needed` / `preloaded_loaded` / `is_preloaded_loaded`, and the boot-script glue `boot_from_script` / `load_code_module` / `add_code_path` / `register_boot_process`
- `../codeserver.rs` → `CodeServer::{add_system_paths, load, is_loaded, loaded}` (the cache this feeds)
- `../native/code.rs` → the `code:*` surface that reports `preloaded` from the name registry (not owned here)
