# `beam_rt` — Rust ERTS Runtime Binary (boot · CLI · `system_info` · runtime glue)

## Intent
Top of the workspace: the `beam_rt` executable.
Owns the CLI flag surface (A.6 — detail in `src/cli/CONTEXT.md`), the boot sequence + preloaded-module registry skeleton (A.7), `system_info/1` + `erlang:process_info/2` (A.8), the concrete `ExecutionContext` glue, the A.4 runtime side of ports (`open_port/2` decoding + `port_info` over `beam_io`'s registry/poller, plus the driver C ABI host `src/port/driver.rs`), the deadlines behind `erlang:send_after/3` and a finite `receive … after T` (`src/timers.rs`), and the distributed node (A.5 phase 1, `src/dist.rs`).
Vertical slice: `../beam_engine/tests/slice.rs` runs on a test-only `MinimalContext`; this crate runs the same through the real glue and the executable.
Depends on all crates (wave 6) but calls down, never duplicates (BIFs → `beam_bifs`, terms/heaps → `beam_term`/`beam_alloc`, processes → `beam_process`, modules → `beam_loader`, execution → `beam_engine`); legacy C ground truth (read-only) under `../../erts/`.

## API Surface (FROZEN — re-exported at the crate root and `beam_rt::prelude::*`)

### `runtime.rs` — the façade
`Runtime` owns every table (config, atoms, processes + mailboxes, BIF heaps, scheduler/runner state, ETS/port/NIF registries, refs, timers, dist) and the scratch `info_heap`.
boot/config: `new(RuntimeConfig)`, `with_defaults()`, `boot()` (idempotent), `is_booted()`, `config()`, `load_module(path)`, `module(name)`, `preloaded()`.
slice: `run_function(path, function, arity, args) -> Result<Eterm>` (x(0)), `run_module(module, function, arity, args, out: &mut dyn OutputSink)`.
scheduling: `spawn_process`, `spawn_fun_process`, `enqueue_process(pid)` (idempotent), `run_one_slice()`, `run_slices(max)`, `run_scheduler()` (send→wake→resume in one call), `process_result(pid)`, `process_exit(pid)` (the reason term), `run_queue_len()`.
refs/introspection: `refs_mut()` (behind `make_ref`/monitors/timers), `system_info(key)`, `system_info_value(key)`, `eval(expr)`, `format_term(term, module)`, `timers_mut()`, `fire_due_timers(table)`, `note_fragment_wake(pid)`, `dist_node()`, `node_name()`, `pump_dist()` (the scheduler pass's distribution half).

### `runner.rs` — the `beam_process::ProcessRunner`
`RuntimeRunner<'a>::run(pid, table, budget) -> Result<RunOutcome>`; `RunnerState` holds an engine per pid plus `results`/`exits`.
`Returned(v)` records `v` and exits `normal`; `Wait{func,pc}` parks as `Waiting`/`Wait`; `Err(e)` records `exit_reason(e)` plus the reason term, never a panic.
A `Wait` arms a receive deadline when the parked `wait_timeout`'s delay is statically readable (`parked_receive_timeout`/`timeout_delay`: `Operand::Integer`/`Literal` or a live X-register; handle in `ProcessEngine::parked_timeout`); a fired deadline resumes at `timeout` with no message delivered, and a message-wake cancels it.

### `context/` — the concrete `ExecutionContext` (`mod.rs` + `bridges.rs` + `tests.rs`)
`RuntimeContext<'a>` carries the runtime borrow, the optional `output` sink (`SharedSink` = a cloneable `Rc<RefCell<..>>`; stdout when `None`), `mailbox_pid`/`mailbox_table`, `may_pump_scheduler` and `current_module`; `new`, `with_output`, `heap_available()`, `for_process(runtime, table, pid)` (running as that pid), `pid()`, `eval_heap()`, `run_export(...)`.
`call_bif` → `try_call_nif` → `try_native` (the floor) → `BifRegistry::contains` → `call` (a `BifContext` over shared atoms / pooled heap / self_pid / output / `PendingInterner` + `RuntimePorts` + `NodeIdentity` + exactly one of the five bridges); else `module=="erlang"` → `is_known_unimplemented_bif` ? `unsupported:` : bare `error::undef`; else load the module → `code_server.load` → `run_export`.
`apply_fun` → external `fun M:F/A` via `call_bif`; an `-eval` closure (`EVAL_FUN_UNIQ`) via `apply_eval_closure`; else the module's `FunDescriptor{uniq,index}` then `Machine::run_label`.
`ensure_heap` → `heap_available() >= heap_need` else `BeamError::SystemLimit` (no GC); `send_message` copies into the receiver's heap (`ProcessTable::deliver_simple`), an oversized payload retrying through the heap-fragment model.

### The five host-seam bridges (`context/bridges.rs`)
`call_registered` installs exactly one of five bridges, chosen by its own `uses_*` predicate; `NodeIdentity` (no borrow) rides alongside.
`NifBridge` (`beam_bifs::NifLoader`; `("erlang","load_nif",2)`) — before the filesystem; a second `load_nif` for a library-backed module is `NifLoadError::Reload`; a failure is returned, never raised.
`RecordBridge` (`beam_bifs::RecordSource`; `("records","get_definition",2)`) — answers from the loaded module's `Recs` chunk (`Module::record_by_name`), field atoms resolved through the module's own table; an unloaded module or undeclared record ⇒ `Ok(None)` ⇒ the oracle's plain `badarg` (`erl_record.c:1342-1403`), an unresolvable field atom a typed `TermNotFound`.
`DictBridge` (`beam_bifs::DictHook`; the five process-dictionary BIFs) — over the running process's `beam_process::Dict` (the scheduler pass's table, else the runtime's); a missing pid is `error::no_mailbox`.
`FunBridge` (`beam_bifs::FunCaller`; every other call) — delegates to `RuntimeContext::apply_fun`, cloning the caller's `SharedSink`, mailbox identity, scheduler-pump permission and `current_module`; carries no `TableRegistry`.
`SendBridge` (`beam_bifs::SendHook`; `("erlang","send",2) | ("erlang","!",2)`) — delegates to `send_message`; the return value is the message term, a send to a pid with no process succeeds silently, a non-pid-like destination is `badarg`, a registered name is delivered.
`NodeIdentity` (`beam_bifs::LocalNode`; in every arm) — the fixed `nonode@nohost`, creation `0`, feeding `Encoder::with_local_node`, so `term_to_binary/1` of an internal pid emits `NEW_PID_EXT` (`88`) and of an internal port `V4_PORT_EXT` (`120`).
`bridges.rs` also holds the dead-code `local_fun_identity`/`module_md5` helpers.

### `PendingInterner` · NIF dispatch
`PendingInterner` (`Clone` over `Rc<RefCell<{base, names}>>`): `new(base)` roots it at the shared table's length and `commit(&AtomTable)` interns the recorded names at their predicted indices; `load_nif/2`'s two writers (the BIF, the C `load` callback through `enif_make_atom`) share the cell.
`run_export` sets `current_module` (restored after), seen by the BIF through `BifContext::with_current_module` (`None` ⇒ `unsupported_bif`, never a guess).
`try_call_nif` is the first statement of `call_bif`, so a NIF replaces the Erlang stub of the same `{module,function,arity}`, its term living in the pointer-stable pooled BIF heap; `NifCallError::Raised(reason)` → `error::nif_raised`, `UnknownFunction` → bare `error::undef`, a miss → a no-op. Interception is at external dispatch only — a local `call` inside the NIF's own module never reaches `call_bif`.

### `port.rs` — the `PortHook` provider (A.4)
`PORT_INFO_ATOMS` (incl. `registered_name`), `PortName{Spawn,SpawnExecutable,Fd,SpawnDriver}`, `StreamType{List,Binary}` (list default) and `RuntimePorts` (`open_port/2`, `port_close/1`, `port_info/1,2`) — the `PortHook` impl, over `Rc<RefCell<PortRegistry>>` + the shared `Rc<AtomTable>` + the owning `ProcessId`.
`PortRegistry`: `new()` (anchors the `driver_*` codegen unit), `is_live`/`contains`, `write_command`, `len()`/`is_empty()` (= `system_info(port_count)`), `ids()` (port-number order = `erlang:ports/0`), `close(id)`, the `{spawn_driver,…}` routing `driver_lookup(name)`, `pump(timeout) -> Vec<PortEvent>`; consumers are `Runtime::deliver_port_messages` and the `-eval` trio `poll_port_messages`/`port_data_message`/`port_exit_message`.
`SpawnDriver` is the single OS-child implementation; this crate owns only the term decoding of `open_port/2`'s `Name`/`Options` (`SpawnSpec`/`SpawnOptions`) and the `PortMeta`.
`pump` polls in `PORT_POLL_SLICE` (25 ms) slices clamped by the earliest driver deadline (`beam_io::PortRegistry::next_deadline`), runs the scheduled port tasks and reports `PortEvent::{Data,Exited}`; `close`/`PortHook::close_port` call `beam_io`'s `notify_port_exit(…, Closed)` before `registry.close(...)`.

### `port/driver.rs` — the driver C ABI host (A.4)
`Driver{library, host: Box<DriverPort>}` pairs a `dlopen`ed driver with its host (field order is load-bearing: the library's `Drop` runs the entry's `finish` before the host drops).
`DriverPort` is the concrete `beam_abi::DriverHost` behind the opaque `ErlDrvPort_`, owning a driver-private term arena, the shared `AtomTable` + a `PendingInterner`, the owning `ProcessId`, the output/term-data/pid services and every captured `driver_output`/`send_term` payload; `load_driver(candidates, name)` reports the last candidate's typed error.
`DriverPortAdapter` is the port lifecycle the host lacks — a `beam_io::Driver` over a loaded C driver: `start`/`stop` run the entry's callbacks, `output` hands the port's queued bytes to the driver's `output` callback and re-queues the `driver_output` reply as the port's input, and the `Driver` drop runs `finish` + `dlclose`.
`PortRegistry::new` anchors `beam_abi`'s driver codegen unit so `build.rs`'s `-Wl,--export-dynamic` has the `driver_*`/`erl_drv_*` symbols to publish; `open_port({spawn_driver, Name}, Options)` routes here, and an unknown or empty driver name is the oracle's `error:badarg`.

### `dist.rs` — the distributed node (A.5, phase 1)
`DistNode` is the runtime's listening half: `start(configured, cookie)` binds a wildcard listener (`DIST_LISTEN_ADDR`, OS-assigned port; unbindable ⇒ a start-up error) and the first `pump()` publishes the node to `epmd` (`ALIVE2_REQ`); accessors `node_name()`/`advertised_name()`/`epmd_name()`/`is_short_name()`, `port()`, `creation()`, `is_registered()`/`is_alive()`/`registration_error()`, `connections()`, `is_idle()`, `last_link_error()`.
`NodeName::parse` splits a `-name`/`-sname` value into `full`/`advertised`/`epmd` (`-name` full, `-sname` short); a bare value's host is `local_hostname()` (`/proc/sys/kernel/hostname`, else `$HOSTNAME`), and `resolve_cookie(init_args)` = the `-setcookie` init argument, else `$HOME/.erlang.cookie`, else `DEFAULT_COOKIE` (`nocookie`).
`DistConnection` wraps one accepted socket in a server-side `beam_dist::ConnectionDriver` (`ConnectionDriver::server(advertised, cookie, creation)`, driven `'N'` → `'s'`/`'S'` → `'r'` → `'a'`); `pump()` registers once, accepts up to `MAX_ACCEPTS_PER_PUMP` (8) connections into links, then steps every link under a per-link read timeout of `DIST_READ_SLICE` (25 ms); a link's protocol/write failure retires it and records why (`last_link_error`).

### `nif.rs` — the live NIF libraries (A.3), and `build.rs`
`NifEntry{lib, host: Box<NifHostImpl>}` (field order is load-bearing: `lib`'s `Drop` runs the C `unload` against the erased host pointer before the boxed host drops) and `NifRegistry{libs}` (owned by `Runtime` as `Rc<RefCell<..>>`) are `!Send`; `NifRegistry`: `new()` (anchors the `enif_*` codegen unit), `is_loaded(module)` (the oracle's `reload` condition), `insert`, `lookup(module,function,arity) -> bool` (the dispatch predicate), `call(..) -> Result<Eterm, NifCallError>`.
`NifHostImpl{heap, atoms, interner, self_pid}` implements `beam_abi::NifHost`; `map_load_error` maps `NifLoadError` → `beam_bifs`'s variant-for-variant; a NIF call runs on a fresh per-call `NifHostImpl` sharing the runtime's one `Rc<AtomTable>` + the in-flight `PendingInterner`.
`build.rs` emits `cargo::rustc-link-arg-bins=-Wl,--export-dynamic` and `…-tests=…`, which with the `beam_abi` `enif_*` anchor publish the symbols a real NIF's `dlopen(RTLD_NOW)` needs (proof: `tests/nif.rs::the_binary_exports_the_enif_symbols_a_nif_imports`).

### `config.rs`, `cli.rs`, `cli/`, `boot.rs`, `info.rs`, `format.rs`
`config.rs`: `RuntimeConfig{emulator, code_paths, heap_words, heap, action, init_args}`, `Action{Boot,Version,Eval,Apply,Run,RunFile}`, `ArgTerm{Int,Atom,Str,List}`, `InitArgs`, `HeapOptions`, `MaxHeapFlags{kill,error_logger,include_shared_binaries}`.
`cli.rs`: `USAGE`, the four `EXIT_*` codes, `parse`/`parse_with_env` and the `run(Vec<String>) -> i32` driver.
`boot.rs` (A.7): `PreloadedModules`; `info.rs` (A.8): `SUPPORTED_KEYS`, `INFO_ATOMS`, `InfoValue`, `system_info_value`, `allocator_report`/`effective_policy`; `format.rs`: `term_to_string`/`term_to_string_w` (drives `io:format/2`).

## The runtime-native BIF floor (`src/native/`)

BIFs that are really runtime operations (reading/writing the runtime's own state), served before the `beam_bifs` registry from `RuntimeContext::try_native`, whose dispatch table is the source of truth:
`native.rs` — `try_native`, the shared helpers (`string_list`/`atom_name`/`atom`/`charlist`/`render`/`write_output`/`chars_to_string`), `halt/0,1`, `display/1`, `io:put_chars/1,2`, `erlang:apply/2,3`, `erlang:system_info/1` (and its `{allocator, A}` 2-tuple → `info::allocator_report`), `erlang:send_after/3` + `cancel_timer/1`, `erlang:process_info/2`, `os:cmd/1`.
`native/init.rs` — `init:get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`, `get_status/0` (→ `{starting, started}`).
`native/code.rs` — `code:get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1` (→ `{file,Path}` / `preloaded` / `false`), `is_sticky/1` (bool; `/2` is `error:undef`), `ensure_loaded/1` (→ `{module,Mod}` / `{error,nofile}`), `lib_dir/0,1`, `root_dir/0`.
`native/process.rs` — `self/0`, `make_ref/0`, `monitor/2`, `demonitor/1,2`, `spawn/1,2,3`, `spawn_link/1,2,3`, `spawn_opt/2,3,4`, `ports/0`, `processes/0`, `pre_loaded/0` (the oracle's exact list and order), `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`; `whereis(init)` is the runtime's own root process (oracle `<0.0.0>`).
`native/port.rs` — `port_command/2,3` (the port's write stream: a child's `stdin`, a `{fd,N}` port's adopted descriptor, or a C driver port's `output`; `Data` must be Erlang iodata at the top level — a bare integer is `badarg`, an integer element of an iolist is a byte, so `port_command(P,[42])` → `true`; `[force]` → `error:notsup`) and the `port_info/1,2` mapping, which owns the two answers a `PortHook` pair list cannot express: `undefined` for a closed/unknown port and the bare `[]` for a live, unregistered port's `registered_name`.
`native/memory.rs` — `erlang:process_info/2` for `garbage_collection` (the oracle's five-item proplist, `max_heap_size` its flag map), `heap_size` and `total_heap_size`, off the live process's `beam_gc` collector; `undefined` for a pid not in the table, `badarg` for a bad item/argument.
`native/application.rs` — `application:get_env/1,2,3`, `get_all_env/1`, `get_key/2`, `loaded_applications/0`, `which_applications/0`, over a minimal native app view (`.app` files through `CodeServer` + a recursive-descent term-text parser); `get_all_env(kernel)` reports the `.app`'s declared env only — seven pairs (oracle: nine; `logger`/`connect_all` are injected by kernel's app start), agreeing key-for-key but not in order.
`native/test_support.rs` — the floor's shared test fixtures.
`beam_bifs` owns the registry surface (`bif.tab`: arithmetic, comparison, type tests, term ops, `lists`/`maps`/`binary`, `io:format`, `ets`, `records`, …); a native entry must never duplicate one, and a call the floor does not claim returns `None` and falls through.

### `code:` is the native floor, not a live `code_server`

`code:*`, `init:*` and `application:*` are preloaded-module surface (A.7) that the oracle serves from `init.beam`/`code.beam`/`application.beam`, written against machinery M1 does not run; OTP's real `code_server.beam` needs `monitor`/`register`/`spawn_link`/`send`, a host timer, `make_ref/0`, named-ETS resolution and `persistent_term`/`erl_prim_loader`, so `beam_rt` ships no live one and the state lives in `CodeServer`. The `boot/*` differential cases touching `code:*` are shape-only; a future live server must reproduce the `{code_call, From, Req}` → `{code_server, Reply}` wire. `src/native/code.rs`'s `code:*` behaviour must not change.

### `erlang`-namespace fallback and the known-unimplemented list

`call_bif` consults the floor, then the `beam_bifs` registry, then falls back for the `erlang` namespace: a known unimplemented OTP `erlang` BIF (`error::is_known_unimplemented_bif`) raises `unsupported: erlang:X/Y …` (exit 3) and any other unclaimed name the bare `error::undef`. The effective gap set is the curated `error::KNOWN_UNIMPLEMENTED_ERLANG_BIFS` intersected with what the live registry does not own, so registering a listed name cannot shadow the registry (drift guard: `error::tests::the_effective_gap_set_never_shadows_the_registry`).

## CLI subset (contract A.6)

The exact switch surface, the `ERL_*FLAGS` merge, the init-argument rule, the exit-code contract and the divergences live in the child node `src/cli/CONTEXT.md`.
In brief: `cli::parse` is pure and `cli::parse_with_env` merges the environment (the driver uses `parse_with_env(&args, &ProcessEnv)`); a `-`-switch (recognized or not) is recorded as an init argument, a `+`-flag is an emulator flag and is not, and the `-eval`/`-s`/`-run` Start instructions are not; `-eval` is a general-expression `erl_eval`-style subset; the `run` argument grammar is `int`/`atom`/`"string"`/`[list]`; `run <file.beam> <mod>:<func>/<arity> [arg…]` loads, runs and prints.

## Exit codes & the `unsupported` signal

Four exit codes, keyed off by the differential harness (the table is in `src/cli/CONTEXT.md`): `0` (`EXIT_OK`) success · `1` (`EXIT_RUNTIME`) a genuine runtime error, printed `beam_rt: <error>` · `2` (`EXIT_USAGE`) a malformed invocation, printed `beam_rt: <error>` + `USAGE` · `3` (`EXIT_UNSUPPORTED`) a capability gap, printed as exactly one line `beam_rt: unsupported: <reason>` (no `USAGE`).
Unsupported is emitted for a `-eval` expression outside the grammar, a BEAM-engine gap (`unsupported_opcode…`) or a `beam_bifs` gap (`unsupported_bif`); the reason is a raise whose atom starts with `unsupported`, interpreted only by `src/error.rs::unsupported_reason` (via `ExitReason::as_atom`, not `BeamError::as_atom`), and it is uncatchable — `flow_from_error` turns it into `Flow::Fatal` (like `halt`), so the harness's `try … catch` probe cannot swallow it.
Structured raise reasons render as the real term: `src/error.rs::live_reason_term` recovers the opaque live-reason `Eterm` a `beam_common::Raise` may carry and renders it through `format_term_w` (used by `runner.rs::exit_reason`, `cli.rs::run_file` and `context/mod.rs::run_export`), so `beam_rt run <m>.beam m:badkey/0` → `beam_rt: error {badkey,nokey}` (exit 1).

## `system_info/1` keys (contract A.8)

`InfoValue` is the heap-free source of truth and `Runtime::system_info` materializes it on the scratch heap. Strings are Erlang strings (char small-ints), exactly as the oracle returns them; an unknown key — atom or tuple — is `Err(BeamError::BadArg)` (the oracle's `error:badarg`).
| Key | Value |
|---|---|
| `version` | `Str` `"17.1"` (`beam_common::version::ERTS_VERSION`) |
| `otp_release` | `Str` `"30"` (the major of `OTP_VERSION`) |
| `system_architecture` | `Str` the target triple from `std::env::consts` |
| `system_version` | `Str` `"Erlang/OTP {major} [erts-{erts}] [source] [{bits}-bit]"` |
| `nif_version` | `Str` `"2.18"` from `beam_abi::{ERL_NIF_MAJOR_VERSION,ERL_NIF_MINOR_VERSION}` |
| `wordsize` | `Int` `8` |
| `logical_processors` | `Int` `available_parallelism()` |
| `process_count` | `Int` the process table's length |
| `port_count` | `Int` the live ports (`PortRegistry::len`) |
| `os_type` | `Tuple` `{unix,linux}` (or `{win32,nt}`) |
| `threads` | `Bool` atom `true`/`false` |
| `emu_flavor` | `Atom` the `-emu_flavor` value (`jit` default, else `emu`/`smp`) |
| `schedulers` | `Int` an explicit `+S N` wins, else `available_parallelism()`; always `> 0` |
| `alloc_util_allocators` | `List` the ten alloc-util allocator names from `beam_alloc::registry::alloc_util_allocators()`, in the oracle's order |
| `allocated_areas` | `List` the oracle's 19 items in order, every value derived from this runtime's own state (`atom_space` is `{atom_space,B,B}`, the table items count live entries, items with no counterpart are `0`) |
| `garbage_collection` | `List` the 4 emulator-wide knobs `[{max_heap_size,0},{min_bin_vheap_size,46422},{min_heap_size,233},{fullsweep_after,65535}]`; `max_heap_size` the bare integer here, unlike the `process_info/2` flag map |
| `{allocator, A}` (a tuple key) | `List` of `{Key,Value}` rows via `beam_alloc::stats::AllocatorInfo::from_stats` over a live `registry::all_stats()` snapshot; `A` outside the ten names ⇒ `badarg` |

`INFO_ATOMS` (`src/info.rs`) is the surface's complete atom vocabulary (the value atoms above, the `{allocator,…}` row names, the `process_info/2` item names + `undefined`), interned by `seed_core_atoms` because `InfoValue::to_eterm` resolves through the atom table and reports `TermNotFound` rather than inventing a term; add any new atom-valued key's atom to `INFO_ATOMS`.

## Constraints

* Errors are return-based, workspace-wide (`../CONTEXT.md` §3): every fallible API returns `beam_common::Result`; no crate-local error enum, no `panic!`/`unwrap()` on runtime data, no silent swallowing. CLI diagnostics are `BeamError::Raise(Raise::error(ExitReason::atom(msg)))`.
* `unsafe`-free: the whole crate is `#![deny(unsafe_code)]`, and the glue uses only the safe peripheries of the lower crates.
* Call down, never duplicate: the `~p` printer is not re-implemented — `format.rs` drives the registered `io:format/2` BIF.
* Frozen term layout (`../CONTEXT.md` §2): terms are built/inspected only through `beam_term`.
* Files ~1000 lines, split by responsibility; `#![warn(missing_docs)]`, every public item documented. Pre-existing oversize (not restructured): `src/port.rs` (~2453 lines; production half ~1470) and `src/info.rs` (~1261; production half ~610).

## Design Decisions

* One shared `Rc<AtomTable>` (the `CodeServer`'s): every loaded module is interned into it and `call_bif` hands `BifContext` that table, so a BIF-built atom is a valid index for every module.
* `Runtime` owns the process heap so a returned `Eterm` outlives `run_*`; `system_info`/`format_term` use a per-call `info_heap`.
* `ensure_heap` returns `BeamError::SystemLimit` on a shortfall (default 64 Ki words); collection-based growth is Stage 4.
* Unmodeled emulator flags are preserved verbatim; `ERL_*FLAGS` is confined to `parse_with_env` + the `env` module.
* A scheduler pass needs `&mut Runtime` and `&mut ProcessTable` at once, so it moves both out (`take_processes`/`put_processes`) along with the pid's `Machine` (out of its `HashMap`).
* A returning process's `x(0)` is recorded in `RunnerState::results` (after `stabilize_term` copies it onto the pointer-stable `eval_heap`) and exits `normal`.
* Every resource handed to `BifContext` is a non-borrowing clone, so a BIF call or send takes the same path as the instruction.
* `nif_error` raises the live reason term (`Flow::Raise { class: Error, reason: <the argument term> }`), so the exact term survives and stays catchable.
* Timers are a runtime-local deadline min-heap (`src/timers.rs`) drained by the scheduler loop, because `beam_process::TimerService` needs a `Send + Sync` sink while `Runtime` is `!Send`.
* Oversized messages are composed in-crate through the heap-fragment model, since `Process::heap_mut`/`mailbox_mut` cannot be borrowed together.
* The driver port handle IS its per-port `DriverEnv` — boxed so a callback's `ErlDrvPort` pointer stays valid across callbacks.

## Known Issues / Intentional M1 gaps

* No interactive shell; distribution is a listening node only: `-noshell` is the only mode, `-name`/`-sname` bind a listener and register with `epmd` (failure recorded in `DistNode::registration_error`, never fatal), yet no link reaches `Established`, inbound `DOP_*` is never delivered, there is no `net_kernel` protocol, and `node/0`/`system_info` do not reflect the live node.
* Preloaded modules are a name-set skeleton: boot does not load preloaded `.beam`s and the atom table seeds `CORE_ATOMS` + `INFO_ATOMS` rather than the full `atom.names` ordering, so preloaded positional atom indices are not reproduced; the erts `ebin` IS on the code path, so they load from disk on demand.
* Own-state answers differ from the oracle by construction, not by bug: `erlang:processes/0`, `erlang:ports/0`, `system_info(port_count)`, `erlang:module_loaded/1`; `erlang:whereis(init)` agrees (the root process, oracle `<0.0.0>`), while `erlang:registered/0` is known-unimplemented.
* Scheduler is single-threaded: a blocked receiver IS resumed by a sender (send→wake→resume), a due `send_after/3` timer or a fired `receive … after T` deadline; the gaps are no reduction pre-emption (`budget` unenforced, `Yield` never returned), a `receive … after T` whose delay is not statically readable (Y-register or `after infinity`) still parking, an uncatchable capability gap on the runner path, and no GC (a runaway allocation returns `SystemLimit`).
* `-eval`/`-s`/`-run` are documented subsets (typed errors in `src/eval/mod.rs`): supported are the expression subset (`-eval` prints nothing), the send operator, a bare `receive` driving a spawned sender, the spawn probes and a bounded scheduler pump; gaps are a `receive` that never receives, one inside a spawned process, `after infinity`, and absent real `erl` start-script processing / `-init_debug`. A non-byte-aligned `-eval` result cannot be printed — the registered `io:format` `~w`/`~p` rejects a non-byte-aligned bitstring, or a container past ~80 columns, with `badarg` — so corpus probes read such values through `bit_size/1`/`is_list`; a construction/pattern segment wider than 64 bits is unsupported and a bignum is `system_limit`.
* NIF loading is live: `erlang:load_nif/2` really `dlopen`s the shared object and runs the C NIF through `try_call_nif`, a load failure is the oracle's returned `{error, {Reason, Text}}` (never a raise), and a second `load_nif` for a library-backed module is `reload`. Typed gaps (A.3): the host leaves `term_to_binary`/`binary_to_term`/`make_ref` at the `NifHost` defaults.
* `NEW_FUN_EXT`: `fun M:F/A` builds a real `FunKind::External` term (`term_to_binary` → `EXPORT_EXT`); a compiled-module `make_fun3` closure's five identity fields are derived in `bridges.rs` (byte-exact to the oracle) but the encoder has no seam to feed them in, so it stays the typed `unsupported: local_fun` (the `wire/new_fun_ext_encode` corpus case); an `erl_eval` lambda has no image.
* Ports match the oracle's observable shape; the gaps: only stdout is polled (stderr discarded, one `Read::read` per readiness), `connected` flips to `false` only at `port_close/1`, `eof` is inert, a `binary`-stream read wider than 64 bytes is typed unsupported, and `os:cmd/1` stays the runtime-native host spawn. The send family serves `erlang:send/2`/`erlang:'!'/2` (the `SendBridge`) with two typed gaps: `erlang:send(Port, Data)` (the oracle drops a send to an unknown/closed port silently, where `port_command/2` raises `badarg`) and the distributed `{Name, Node}` send (A.5).
* Driver ports (A.4) load and drive a real C driver end to end: `driver_output` IS delivered to the owner as `{Port, {data, Data}}` (`tests/driver.rs`), `driver_select` registers one descriptor per direction with the runtime's poller (several ⇒ typed unsupported), and `driver_set_timer` mirrors its deadline into the port's `TIMEOUT` task and clamps the poll round (cleared before the callback so a re-arming one wins). Typed gaps: `driver_async` keeps the `-1` default, `port_control/3` misses `control`, a decoded `send_term` stays in `DriverPort::sent`, `ERL_DRV_USE`'s `stop_select` never fires, and no OS child (`os_pid` undefined); with no `erl_ddll:load_driver/*`, the `Name` doubles as the search (a `/`-containing name is the object path, driver name = its file stem; else `<name>.so`, `./<name>`).
* The timer family is mostly gaps: `erlang:send_after/3` answers a `#Ref<>`; `cancel_timer(Pending)` the remaining ms, `cancel_timer(Fired|Cancelled|unknown-ref)` `false`, `cancel_timer(non-ref)` `badarg`, a bad time/destination `badarg`; `start_timer/3,4`, `send_after/4`, `cancel_timer/2` and `erlang:system_info/2` are unimplemented.
* GC and memory observables: `process_info/2` answers `garbage_collection` (the oracle's five-item proplist, non-hardcoded), `heap_size` (agreeing, 233) and `total_heap_size` (466 vs the oracle's 233 — a 233-word mature generation is pre-allocated); a pid not in the table is `undefined`, an unknown/non-atom item or a non-pid `badarg`, and `erlang:process_info/1` is unimplemented. `erlang:memory/0,1` and `erlang:statistics/1` are not registered BIFs (exit 1); only those heap items, `system_info(allocated_areas)`/`{allocator, A}` and `ets:info(T, memory)` answer memory numbers.
* The heap switches are modelled; some rejection details and the CLI divergences are not: `+hms`/`+hmbs` set a `beam_gc::HeapPolicy` rounded through the growth table (`+hms 1000` → 1598) and `+hmax*` the `max_heap_size` map, but a malformed value is exit 2 where the oracle exits 1. The remaining divergences (that exit code, the `ErtsStrToSint` laxness, `-emu_flavor`, `+K`, `-M*`/`+M*`, and `-smp disable` — oracle-rejected with `Use "+S 1" instead.` while `beam_rt` accepts it) are recorded in `src/cli/CONTEXT.md`.

## Notes for Agents

* Read `../CONTEXT.md` and the two `../../ERTS_RUST_REWRITE_*.md` design docs before changing the glue; this crate is where the Stage-1 engine meets the Stage-5 core.
* Run the crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt` (bare `cargo` from the repo root fails — the workspace manifest is `erts_rust/Cargo.toml`; the sandbox `$HOME` is read-only, hence the wrapper).
* The compatibility gate: `erts_rust/tools/differential/gate.sh`, or manually `erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -q -p differential -- suite`.
* `tests/fixtures/` must not be passed to the oracle as a single `-pa` — it contains a `lists.beam` that shadows stdlib.

## Test Strategy

* Unit tests (colocated `#[cfg(test)]` in each module), `src/context/tests.rs` and the floor tests (`src/native/*.rs`, `src/info.rs`, fixtures in `test_support.rs`): the A.6 parser / `ERL_*FLAGS` merge / emulator-flag validators; `system_info_value` per key + its unknown-key `badarg`; `InfoValue`→`Eterm`; `PreloadedModules`; the evaluator + binary surface; `FunCaller`; `nif_error`'s live reason; the dictionary BIFs via `DictBridge`; the error helpers, the unsupported classifier and its drift guard; the `uses_*` predicates (pairwise-disjoint across the five bridges); `RecordBridge` over a loaded module's `Recs`; the `NodeIdentity` encoding; the nested-BIF seams; `PendingInterner`; the nine mailbox methods; the heap-fragment path; the local-fun-identity oracle; every `code:*`/`init:*`/`application:*`/process/reference/monitor entry; `process_info/2`; the timer BIFs; the A.8 keys.
* Ports / bit operators / timers (`src/port.rs`, `src/port/driver.rs`, `src/eval/mod.rs`, `src/runtime.rs`): every `Name` form + options (`{fd,N}` over a socket pair); empty name → `BadArg` with no port number consumed; close/reap; the `port_info` vocabulary; `pump`'s once-only `{exit_status,…}`, the `next_deadline` clamp and `take_driver_outputs`; the driver select/timer tests; `ids()`; the six bit operators; the timer heap and the receive-deadline tests over `recv_after.beam` (`after20` ⇒ `timeout`, `afterzero` never suspends, `self_send` takes the queued message, a message wakes `after2000` and cancels it, `wait_forever` parks).
* Integration suites (`tests/`), each pinning a contract: `slice`/`cli` (`add/2` → 5, `fib/1 10` → 55; the `run`/`--version`/`-eval` boundary, incl. grammar-external `-eval` → 3); `cli_flags` (the A.6 switch/`ERL_*FLAGS` surface); `eval` (the differential `Eval` probes + the control-flow, port, `semantics/*`, `boot/*` and `wire/*` extras); `records`/`nif`/`driver` (A.7/A.3/A.4, fixtures compiled at test time); `dist` (A.5 phase 1); `timers` (the timer capability at the executable boundary); `multimodule` (the `undef` producers); `dict` (the process dictionary); `function_clause` (the raise via `CORE_ATOMS`); `structured_errors` (structured raise reasons); `mailbox` (the nine methods); `scheduler` (resume-on-send); `port_wake`/`port_stream` (the port → mailbox wake); `send` (the `SendHook`); `port_parity` (iodata bare-integer, `registered_name`, empty name, closed/unknown `undefined`). Oracle values verified against system `erl`.

## Routing Table

* `src/lib.rs` → crate docs, `#![deny(unsafe_code)]`, module wiring, re-exports, `prelude`.
* `src/main.rs` → the executable shim over `cli::run`.
* `src/cli.rs` → the A.6 switch parser, `USAGE`, the `run` driver, the `EXIT_*` codes.
* `src/cli/` → `env.rs` (the `ERL_*FLAGS` merge/tokenizer) + `flags.rs` (the emulator-flag validators) — child node `src/cli/CONTEXT.md`.
* `src/config.rs` → the runtime config and CLI action/argument/heap-flag types.
* `src/error.rs` → the error constructors, halt helpers, the `unsupported` classifier and the known-unimplemented gap set.
* `src/boot.rs` → the preloaded-module registry (A.7) and `CORE_ATOMS`/`PRELOADED_MODULE_NAMES`.
* `src/codeserver.rs` → `CodeServer`: the shared atom table, the module cache, the code path.
* `src/info.rs` → the A.8 `system_info` surface.
* `src/runtime.rs` → `Runtime`, the crate façade.
* `src/runner.rs` → the `beam_process::ProcessRunner` + `exit_reason`.
* `src/context/` → the concrete `ExecutionContext` (`mod.rs`, `bridges.rs`, `tests.rs`).
* `src/mailbox.rs` → the nine `beam_engine::Mailbox` methods over `beam_message::Queue`.
* `src/timers.rs` → the runtime-local deadline min-heap.
* `src/native.rs` → the runtime-native BIF floor + `src/native/{init,code,process,port,memory,application,test_support}.rs`.
* `src/eval/mod.rs` → the `-eval` evaluator; `src/eval/{ast,parse}.rs` → the AST + lexer/Pratt parser.
* `src/format.rs` → `term_to_string`/`_w` over the registered `io:format/2` printer.
* `src/port.rs` / `src/port/driver.rs` → the A.4 `PortHook` provider + the driver C ABI host.
* `src/dist.rs` → the distributed node (A.5 phase 1).
* `src/nif.rs` / `build.rs` → the live NIF libraries (A.3) + the `--export-dynamic` link arg.
* `tests/` → the integration suites above and `tests/fixtures/` (incl. the never-committed `event_driver.{c,erl}`; recipe in `fixtures/README.md`).
* Read-only inputs: `../beam_engine/src/context.rs`, `../beam_bifs/`, `../beam_abi/{nif,driver}`, `../beam_loader/tests/fixtures/slice.beam`, and the C under `../../erts/`.

## Handoffs (owned by OTHER nodes)

`differential suite` last measured `total=541 agree=540 mismatch=0 unsupported=1 skipped=0`; the one `unsupported` is `wire/new_fun_ext_encode` (A.5). The `records/*` cases agree; remaining `wire/*` encode/decode asks live in `beam_codec`/`beam_bifs`.
* `beam_bifs` — a fun-identity seam on the ETF encode path: `binconv.rs::encode_body` is the only `beam_codec::Encoder` construction point and `BifContext` carries no identity, so a compiled-module `FunKind::Local` fun stays `unsupported: local_fun`.
* `beam_process` — a `!Send`-friendly `SchedulerPool` for SMP adoption, since `Runtime` is `!Send` and this crate is `#![deny(unsafe_code)]`.
* `beam_process` / `beam_message` — a split borrow over a process's heap + mailbox plus an owning drain for the wake list, so `beam_message::send_fragmented` can be driven from here.
* `beam_engine` — a deadline on the block outcome or a `Machine` Y-register accessor: `RunOutcome::Wait { func, pc }` carries no deadline, so the runner IR-scans for the parked `wait_timeout` (statically readable operands only).
* `beam_dist` — the server-side `'N'` handshake rejects a real OTP 29 peer with a typed `badarg`, the single blocker for a real handshake.
* `beam_bifs` / `src/context` — `node/0` and the `NodeIdentity` wiring: feed the identity from `Runtime::dist_node()` so a live `-sname`/`-name` node is visible to `erlang:node/0` (not registered) and the node-valued `system_info` items.
* Distribution, phase 2 — inbound `DOP_*` into a mailbox: resolve a link's queued `DOP_REG_SEND`/`DOP_SEND` to a local process and `deliver` the payload through `beam_process`.
* `beam_io` — the driver-port remainder (`async`, a decoded `send_term`, `port_control/3` reaching `control`, `ERL_DRV_USE`'s `stop_select`, a per-registration `ErlDrvEvent` identity, a self-exiting child), plus the `open_port({spawn_executable, <a DIRECTORY path e.g. /tmp>}, [])` divergence: the oracle returns a port where `beam_rt` raises `error:eacces`.
* `beam_engine` — name the rejected opcode: kernel `code.beam`'s `code_server:call/1` executes `recv_marker_reserve/1` (also `recv_marker_bind`, `send`, `loop_rec`, `remove_message`), which no family claims, so it falls to the generic `error::unsupported("widened opcode not yet executed")` that does not name the instruction.

## See Also

* `../CONTEXT.md` → crate-layer contracts, the seam types, the wave-6 position, the M1 scope table; `../../CONTEXT.md` §1 A.6/A.7/A.8 → CLI/configuration, preloaded/boot, introspection contracts, §3 error strategy, §7 staging.
* `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.6–A.8 → the CLI/config, OTP-app and introspection surfaces.
* `../beam_engine/CONTEXT.md` (the seam), `../beam_bifs/CONTEXT.md` (the BIF registry), `../beam_loader/CONTEXT.md` (the IR + fixture).
* `src/cli/CONTEXT.md` → the A.6 command-line surface: the switch table, the `ERL_*FLAGS` merge, the init-argument rule and the documented divergences.
