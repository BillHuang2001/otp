# `beam_rt/src/native/` — the runtime-native BIF floor

## Intent
The BIFs that are really **runtime** operations: they read/write this runtime's own state
(process table, port registry, code server, atom table, application view, parsed command line,
timer heap, the module container loader) and are therefore served *before* the `beam_bifs`
registry is consulted. They exist because `beam_bifs` owns the static `bif.tab` surface
(arithmetic, comparison, term ops, `lists`/`maps`/`binary`, `io:format`, `ets`, `records`, …)
while a handful of names need the host — and because
`init:*`/`code:*`/`application:*`/`erts_internal:*` are **preloaded-module surface** (contract
A.7) whose real `.beam` implementations are written against process/message machinery
Milestone-1 does not run.

The floor is a *fall-through* layer: every entry returns `Option<Result<Eterm>>`; `None` means
"not a native BIF", and the caller (`RuntimeContext::call_bif`, `../context/mod.rs`) then tries
the registry (or loads the module). Nothing here duplicates a registry BIF, and a call the floor
does not claim is never answered with a fabricated value.

## API Surface
### The dispatch (source of truth: `../native.rs`)
`RuntimeContext::try_native(module, function, arity, args) -> Option<Result<Eterm>>` — **one match
arm per native call**, plus exactly one arm for the whole `erts_internal` namespace
(`("erts_internal", _, _) => self.erts_internal_call(…)`) and one for `prim_file`
(`prim_file_call`), whose per-function dispatch lives in the child modules. The arms are the
source of truth for what is native; each submodule owns its per-function bodies.

`call_bif`'s dispatch order: **loaded NIF → floor (`try_native`) → `beam_bifs` registry →
on-demand external-module load**. A key in the `erlang` namespace that none of those claims is
classified as the capability gap (`unsupported: …`, CLI exit 3) or the bare `error:undef`
(`../error.rs`).

`FLOOR_MODULES: [&str; 8]` (`native.rs:119`) = `application`, `code`, `erlang`, `erts_internal`,
`init`, `io`, `os`, `prim_file`. Every one is a module the code server can **also** load as a
`.beam`, so it gates `RuntimeContext::resolve_external_call`, which must answer `Ok(None)` for all
of them — entering the body would run it *instead of* the host implementation. It does **not**
gate `call_bif`'s dispatch: a `beam_bifs` registry key in a floor namespace the floor has no arm
for still reaches the registry (`os:env/0` is `beam_bifs`'s; the floor owns only `os:cmd/1`).
Drift-guarded by `native.rs::tests::every_floor_module_is_a_real_try_native_arm` (⇔ the
`try_native` arms) and by `../error.rs`'s
`known_unimplemented_bifs_are_capability_gaps_and_never_shadow_the_registry` /
`the_effective_gap_set_never_shadows_the_registry`.

### Files (one responsibility each; per-entry ground truth/oracle probes are in each module's rustdoc)
| Module | Serves |
|---|---|
| `../native.rs` (parent) | module docs, `try_native`, the shared helpers (`atom`/`atom_name`/`charlist`/`render`/`string_list`/`write_output`/`chars_to_string`), and the arms whose bodies live here: the `erlang` core (`halt/0,1,2`, `display/1`, `apply/2,3`, `system_info/1`, `send_after/3`, `cancel_timer/1`, `make_ref/0`, `pre_loaded/0`, `module_loaded/1`), `io:put_chars/1,2`, `os:cmd/1`, the `seq_trace`/`seq_trace_info`/`seq_trace_print`/`system_flag(reset_seq_trace)` family over `crate::seq_trace`, and the `trace`/`trace_info`/`trace_delivered`/`trace_pattern` family over `crate::trace` |
| `init.rs` | `init:get_argument/1` (a command-line flag from `RuntimeConfig::init_args`, plus the four **automatic** flags `root`/`home`/`progname`/`bindir`), `get_arguments/0`, `get_plain_arguments/0`, `get_status/0`, `script_id/0`, `stop/0,1`, `restart/0`, `boot/1` |
| `code.rs` | `code:get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1`, `is_sticky/1,2`, `ensure_loaded/1`, `load_file/1`, `lib_dir/0,1,2`, `priv_dir/1`, `root_dir/0`, `get_mode/0` |
| `process.rs` (+ `process/monitor.rs`, `process/exit.rs`) | `erlang:make_ref/0`, `spawn/1,2,3`, `spawn_link/1,2,3`, `spawn_opt/2,3,4`, `spawn_monitor/1,2,3,4`, `ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`; the reference-alias family (`alias/0,1`, `unalias/1`); the monitor family (`monitor/2,3`, `demonitor/1,2`) in `process/monitor.rs`; `exit/2,3` in `process/exit.rs` |
| `node.rs` | `erlang:is_alive/0`, `get_cookie/0`, `set_cookie/2`, `nodes/0,1` |
| `memory/` | `erlang:process_info/1,2` — the per-process introspection surface (+ `memory/tests.rs`) |
| `group_leader.rs` | `erlang:group_leader/2` + `erts_internal:group_leader/2,3` over `Runtime::group_leaders` |
| `port.rs` | `erlang:port_command/2,3`, `erlang:port_control/3` (the driver's `control` callback, with the oracle's list-vs-binary reply rule) and the closed-port half of `erlang:port_info/1,2` (the `PortHook` seam cannot express those) |
| `application.rs` | `application:get_env/1,2,3`, `get_all_env/1`, `get_all_key/1`, `get_key/1,2`, `get_application/0,1`, `loaded_applications/0`, `which_applications/0` over a minimal native `.app` view |
| `erts_internal.rs` | the `erts_internal` floor (`erts_internal_call`) **plus the `erlang:garbage_collect/0,1` bodies** it hosts |
| `erts_internal/trace.rs` | the `erts_internal:trace/3` + `:trace_pattern/3` handler bodies (`erlang:trace/2,3`/`trace_pattern/2,3` call them) + their pattern/target helpers, over `crate::trace` (no colocated tests — `../tests/trace*.rs`) |
| `prim.rs` | the **path** half of the `prim_file` floor (`prim_file_call`) + the path contract, the `errno`→atom table and their unit tests |
| `prim_file.rs` | the **whole-file-descriptor** half of the `prim_file` floor + its unit tests |
| `file_server.rs` (+ `file_server/`) | the native **`file_server_2` service** (contract A.9) — see [`file_server/CONTEXT.md`](file_server/CONTEXT.md) |
| `test_support.rs` | the shared test fixtures (`runtime`/`system_runtime`/`call`/`atom`/`chars`/`string_of`) |

### The preloaded-module surfaces (contract A.7)
**`init`** (`init.rs`). `init:get_argument/1` serves the four **automatic** flags — `root` (the
code root dir), `home` (`$HOME`), `progname` (`$PROGNAME` else the compile-time `"erl"`, the C rule
`erlexec.c:1702-1705`) and `bindir` (`<root>/erts-<vsn>/bin`) — so it is *not* the boot determinant
it once was: the real boot script runs to `{progress,started}` under the CLI-equivalent
configuration and under a `RuntimeConfig`-default runtime alike. `init:get_arguments/0` **prepends**
those four entries (`root`, `bindir`, `progname`, `home`, each `{Flag,[[Value]]}`, an unavailable
value omitting its entry — the oracle's own shape) before the command-line flags (boot-chain
consequence: [`../boot/CONTEXT.md`](../boot/CONTEXT.md)). `init:script_id/0` is the **loaded** boot
script's `{Name,Vsn}`, or the typed gap when none is loaded. `init:stop/0` records halt status 0 and
answers `ok` (the running expression is not interrupted; `cli::run` applies the recorded status
after the action completes); `stop/1` answers `ok` for a non-negative integer status (recorded),
raises the `erlang:halt/1` crash-dump slogan for a byte list (incl. `[]`) and `badarg` for anything
else (incl. the atom `normal`). `init:restart/0` is a real **in-process reboot** —
`Runtime::restart` resets the code path to its boot-time default (`CodeServer::default_paths`: the
configured `-pa`/`-pz` dirs plus the system ebin dirs), re-runs the boot sequence with the `booted`
flag cleared and re-executes the recorded boot script — then answers `ok`. `init:boot/1` parses the
`[binary()]` BootArgs, takes the `-boot <Script>` value (default `$ROOT/start`,
`init.erl:1257-1258`), resolves it through the CLI's own rule (`cli::resolve_boot_file_for`) and
executes it, then answers `ok`. Fidelity limits: no live `init` process, so the oracle's
clear-system teardown is approximated by the re-boot; the real `boot/1` never returns (it enters
init's receive loop) whereas this one returns after booting.

**`code`** (`code.rs`). All entries read the code server's own path/load state.
`code:load_file/1` really loads (`{module,M}` from `CodeServer::load`) with the oracle's
`{error,sticky_directory|nofile|badfile}` vocabulary. `code:get_mode/0` reads the code server's
`interactive`/`embedded` mode from the `-mode` init argument (`code.erl:1741-1765`).
`code:priv_dir/1` is `lib_dir(App, priv)` — the application directory **joined** with `priv`, never
`stat`ed (the code server rewrites `{priv_dir,App}` into `{lib_dir,App,priv}`,
`code_server.erl:1058-1059`), with the guard's atom/string forms accepted and `{error,bad_name}`
for a name the code server does not know (`code.erl:978-980`).
**`code:*` is the native floor, not a live `code_server`** — OTP's real `code_server.beam` needs
facilities outside this crate, so `code.rs`'s behaviour must not change (see `../CONTEXT.md`).
`code:is_loaded/1` and `code:which/1` report **path resolvability**, not the code server's load
state — the deliberate divergence that keeps the `boot/code_is_loaded_*` corpus cases agreeing;
never "fix" it.

**`application`** (`application.rs`). A minimal native `.app` view: `get_env/1,2,3`,
`get_all_env/1`, `get_all_key/1`, `get_key/1,2`, `get_application/0,1`, `loaded_applications/0`,
`which_applications/0`. `get_application/0` is always `undefined`; `/1` maps a module to its
loaded application. `app_env_props` reads the `.app` resource's `env` list **verbatim**, so
kernel's start-time `logger`/`connect_all` injections are absent ⇒
`application:get_env(kernel,connect_all)` is `undefined` here vs `{ok,true}` on the oracle.
`get_application/1` on a non-atom that is not a pid leaves the function with no clause
(`function_clause`, the oracle's own shape); the oracle's extra `is_pid/1` clause answers
`undefined` for a pid — a documented divergence, since this runtime runs no application-owned
process.

### The process-control families (route through `beam_process`) — `process.rs` + `process/`
`monitor/2,3` and `demonitor/1,2` use `ProcessTable::{monitor, monitor_named, demonitor,
demonitor_flush}`: `[flush]` removes a queued `'DOWN'` from the caller's mailbox and `[info]`
answers the **real** "was a monitor on record and got removed" boolean (a second
`demonitor(Ref, [info])` is `false`). Only a term that is **not a reference at all** is the
oracle's `badarg` (the C's structural `is_not_internal_ref`, `bif.c:443-450`); a reference that
backs no recorded monitor — a bare `make_ref/0`, or an `erlang:send_after/3` timer reference the
timer heap owns — is the "nothing was removed" answer (`demonitor/1` ⇒ `true`, `demonitor/2`'s
`[info]` ⇒ `false`), leaving the timer pending.
The **reference-alias** family (`alias/0,1`, `unalias/1`) rides the same table: `alias(Opts)` mints
a fresh standalone reference alias owned by the caller and `unalias/1` clears one the caller holds,
answering whether it did. `alias/0` is the `erlang.erl` wrapper over `alias([])`
(`erlang.erl:2130-2133`) and, because `erlang` is a floor namespace, the wrapper is un-enterable —
the floor owns the name and hands the shared `/1` implementation the empty option list, so both
arities cannot drift. `Opts` is `[explicit_unalias | reply | priority]` (the C's `alias_1`,
`bif.c:5903-5930`); anything else (a tuple included, or a non-list) is `badarg`. An alias is **not**
a monitor, so `demonitor/1` of one answers `true` and leaves it in place.
`exit/2,3` signals another process through `ProcessTable::send_exit`, but a **self**-directed exit
is raised directly (non-trapping caller, or the untrappable `kill`) or delivered as `{'EXIT',
self(), R}` via `RuntimeContext::deliver_message` (trapping caller and `Reason != kill`) — because
the sibling scheduler defers a self-directed signal that arrives while the pid is `Running`, which
it is inside a slice. `exit/3` accepts ONLY the bare atom `priority` as an option (`[{priority,
high}]` is `badarg`). The node surface (`node.rs`) reads the runtime's `DistNode`/cookie/local-node
state: `get_cookie/0` is the atom `nocookie` while the node is not alive, `set_cookie/2` raises
`badarg` for a non-atom node/cookie (and for this runtime's own node name on a non-alive VM) and
`distribution_not_started` for a well-typed remote pair, `nodes/0,1` answers this runtime's own
node set.

`spawn_opt` parses and validates EVERY option **before** the child is allocated, then builds the
child's `HeapPolicy` with `beam_gc::HeapPolicy::from_spawn_opts` and applies it via the
policy-aware `Runtime::spawn_*_with_policy`.

**The spawn family works from *inside* a scheduler slice too.** `RuntimeContext::owns_process_table`
picks the target: a context that owns the table (the `-eval` evaluator) spawns through
`Runtime::spawn_process_with_policy`/`spawn_fun_process_with_policy`, while one built by
`for_process` — a slice of a pass that has **moved** the runtime's table out — goes through
`RuntimeContext::spawn_in_pass`: the child's pid is allocated in the **pass** table with its heap
policy and priority (the same `ProcessTable::spawn` + `set_priority` the direct path uses), the
entry point is resolved by `Runtime::resolve_export_entry` (shared wording, so module
resolution/`undef` are identical), and the record is handed to the pass
(`Runtime::note_pending_spawn`). `Runtime::absorb_pending_spawns` then registers the engine entry in
the pass's runner state and enqueues the pid **at its own priority**, so the child really runs —
`spawn_link`/`spawn_opt`'s `link`/`monitor` go through the same table via
`link_to_child`/`apply_spawn_options`. Proven by `../tests/spawn_from_slice.rs` (all five forms incl.
a nested spawn, oracle-cross-checked) and by the two `runtime::scheduler::tests` cases over
`absorb_pending_spawns`.

### `erlang:garbage_collect/0,1` (contract A.7) — the on-demand collection
Bodies in `erts_internal.rs`, arms in `../native.rs::try_native`. `erlang` is a floor namespace, so
its `.beam` wrappers are un-enterable and the floor owns both names: `/0` (`erlang.erl:3051-3053`,
whose body is `erts_internal:garbage_collect(major)`) and `/1` (`erlang.erl:3055-3066`, the wrapper
`erlang:garbage_collect(Pid, [])`, whose `self()` clause reaches that same preloaded BIF).

**No BIF may collect.** The root set a collection relocates is the **executing machine's**
`x[0..live)` plus its Erlang stack, and a BIF runs *inside* the engine, owning neither — so a
servable call records a **request** through the one latch (`Runtime::request_gc`) and answers the
oracle's `true`. The request is served at the two sites that *do* own that root set: the engine's
heap-admission point (`../../src/context/gc.rs::ensure_heap`, where a pending request bypasses only
the occupancy test, never the safety valves) and the runner's park arms (`../../src/runner/run.rs` →
`Runtime::serve_gc_request`, after each `publish_parked_roots`); a request still pending when its
process terminates is pruned at the pass boundary
(`../../src/runtime/scheduler.rs::scheduler_pass` → `prune_gc_requests`). A request is recorded
**only** for a **scheduled slice** — a context that does not own the process table
(`RuntimeContext::owns_process_table`); every other context (`-eval`, a single-pass `run_*`)
allocates on the pointer-stable runtime arena, which no collection ever moves, so it answers the
oracle's `true` and records nothing. `Runtime::gc_stats().collections` therefore moves only when a
collection really ran; a request the valves refuse (a pinned or halted pid, a **relocation-exempt**
process, the global escape-store valve) is dropped honestly with the counters untouched, never
re-armed.

`/1`'s argument contract is the oracle's (`erl` probes, and the C's own test in
`erl_process.c:11309-11324`): a **live local pid** ⇒ `true` and a request recorded for *that* pid; a
well-formed internal pid the process table does not hold ⇒ `false`, with nothing recorded; anything
that is not an internal pid — including an **external**/wire pid, which this runtime has no
distribution link to and so can never honour ⇒ `badarg`. `/2` (`{async, ReqId}` system tasks)
remains a **typed capability gap** and stays in
[`../../src/error.rs::KNOWN_UNIMPLEMENTED_ERLANG_BIFS`](../../src/error.rs). Observable evidence:
[`../../tests/garbage_collect.rs`](../../tests/garbage_collect.rs) — the return values are
oracle-cross-checked, and a scheduled `ask/1` really moves the `collections` counter where the
identical `plain/1` workload moves it by zero.

### `erlang:spawn_monitor/1,2,3,4` — an `erlang.erl` wrapper, not a `bif.tab` BIF
The four arities (`erlang.erl:10667`/`:10686`/`:10706`/`:10874`) are `erlang:spawn_opt(…,
[monitor])` behind `erlang.erl`'s own guards — `is_function(F, 0)` for `/1` and `/2`, plus
`is_atom(Node)`, `is_atom(M)`, `is_atom(F)`, `is_list(A)` — so no registry entry (and nothing in the
known-unimplemented list) claims them and the floor owns the whole family. A guard violation is the
oracle's `badarg`; the `Node` forms on a node **other** than this runtime's own are the typed
capability gap, because the oracle routes that clause through a distributed
`erlang:spawn_request/5` (contract A.5) and this runtime has no send path for a spawn — never a
locally fabricated child. The `Node` test is `process/monitor.rs`'s `pub(super)` `is_local_node`, one
definition shared with `monitor/2,3`'s `{Name, Node}` target. The monitor is installed by the private
`attach_monitor` (shared with `spawn_opt`'s `monitor` option, so the `{Pid, Ref}` shape and the
reference → monitor bookkeeping cannot drift), and it is on record **before the child's first
schedulable moment**: the child is only a run-queue entry (or a pass `PendingSpawn`) when the BIF
returns and the runtime is single-threaded, so a child that exits immediately still delivers its
`{'DOWN', …, normal}`. Proven by `../tests/spawn_monitor.rs`.

### `erlang:process_info/1,2` (contract A.8) — `memory/mod.rs` (+ `memory/tests.rs`)
A **runtime** operation: the answer is read out of this runtime's own `beam_process` table
(`Process::state/mailbox/links/monitors/monitored_by/dict/registers/collector/off_heap`), served
before the registry. `memory/mod.rs`'s rustdoc carries the full per-item table and oracle probes;
the essentials:

* **Reply shapes** (oracle-verified, OTP 29.1.1 / erts-17.1). `/2` with an item **atom** or
  `{dictionary, Key}` → `{Item, Value}` — **except** `registered_name`, whose value is the **bare**
  name atom, and the bare `[]` when the process is unregistered (the C's `always_wrap =
  !is_atom(opt)`). `/2` with a **list** → `[{Item, Value}, …]` in the requested order, duplicates
  repeated, `registered_name` **wrapped** (`{registered_name, Name}` / `{registered_name, []}`). `/2`
  with `{dictionary, Key}` → `{{dictionary, Key}, Value | undefined}`. `/1` → the fixed
  `PI_1_KEYS` set (C `pi_1_keys[]`, `erl_bif_info.c:1008-1026`), in order: `registered_name,
  current_function, initial_call, status, message_queue_len, links, dictionary, trap_exit,
  error_handler, priority, group_leader, total_heap_size, heap_size, stack_size, reductions,
  garbage_collection, suspending`. A **registered** process emits all 17 (`{registered_name, Name}`
  first); an **unregistered** one **omits** `registered_name` (and emits the remaining 16).
* **Failure modes.** A non-atom / non-list / non-`{dictionary,_}` second argument ⇒ `error:badarg`;
  an unknown item name (also **inside** a list, or an improper list) ⇒ `error:badarg`, **even** for a
  non-live pid; a non-internal-pid first argument ⇒ `error:badarg`; a **valid** item with a
  live-but-absent pid ⇒ the atom `undefined` (all three forms; the list form of a dead pid is
  `undefined`, *not* `[]`).
* **Implemented items** (~28, each a real value from this runtime's state):
  `registered_name` (the one special item), the engine-derived `current_function`/`initial_call`/
  `current_location`/`current_stacktrace`/`catchlevel`, `status`, `message_queue_len`/`messages`,
  `links`/`monitors`/`monitored_by`, `dictionary`/`{dictionary, Key}`,
  `trap_exit`/`error_handler`/`priority`/`group_leader`,
  `heap_size`/`total_heap_size`/`stack_size`, `reductions`, `garbage_collection` (the 5-item
  proplist `[{max_heap_size,Map},{min_bin_vheap_size,N},{min_heap_size,N},{fullsweep_after,N},
  {minor_gcs,N}]`, `beam_gc::GARBAGE_COLLECTION_KEYS` order) and its three
  `min_heap_size`/`min_bin_vheap_size`/`fullsweep_after` components, `max_heap_size` (the flag
  **map**), `message_queue_data`, `trace`, `sequential_trace_token`, `suspending`, `last_calls`,
  `memory`.
* **The execution-position items** (`current_function`/`initial_call`/`current_location`/
  `current_stacktrace`) and `catchlevel` are read from the process's saved instruction pointer
  through the engine's read-only accessors (`beam_engine::Machine::current_function`/`initial_call`/
  `current_location`/`current_stacktrace`/`catch_frame_depth`, builders shared with the exception
  family's `build_stacktrace`), reached by `RuntimeContext::position_value`/`catchlevel_value` via
  `RunnerState::engine_at(pid)`. They are answerable only **conditionally**: the pid must be in the
  process table (`RuntimeContext::process_table` — the **pass** table inside a scheduler slice,
  which has *moved* it out of the runtime, else the runtime's own) **and** have a readable
  engine/entry — a **fun-bodied** process has no module to name `M`/`F` from, the **caller's own**
  process has its `Machine` moved out while it runs, and a never-scheduled pid has none. Those
  report `Ok(None)` — the item is **omitted** by `/1` and is the typed `unsupported` gap by `/2`.
  End-to-end proof: `../tests/position.rs` — `recv:wait/0` parks in a `receive` (fixture
  `../tests/fixtures/recv.erl`, clause head on line 12), a second process (`recv:probe_b/1`) probes
  it, and the term is compared line-for-line with the live system `erl` oracle.
* **Honest typed `unsupported` items** (valid oracle names, gap reason names the missing
  accessor/hook): `backtrace` (no raw/binary stack-dump accessor — only the cooked stacktrace),
  `magic_ref` (the NIF resource API is unbound, so no resource is bound into a process),
  `parent`/`label` (no such PCB field), `async_dist`/`priority_messages` (no such
  `beam_process::ProcessFlags`), `garbage_collection_info` (`beam_gc::HeapInfo` measures no
  per-generation block sizes), `binary` (`beam_alloc::RefcBinary` has `refcount()`/`len()`/
  `data_ptr()`, but no process registers off-heap refc binaries in `Process::off_heap` — the engine
  holds them privately on `Machine::refc_binaries` — so the `[{Ptr,Size,Refc},…]` roster cannot be
  built). An item this runtime cannot answer is **omitted** by `/1` (the C's own mechanism for an
  unregistered `registered_name`).
* **Documented divergences** (correct-but-different-by-construction — do NOT "fix" to a hardcoded
  figure): `/1` emits 15 (registered) / 14 (unregistered) items where the oracle emits 17/16 for a
  process with no readable execution position (the two engine keys); `heap_size` agrees with the
  oracle's 233 (the 233-word nursery) but `total_heap_size` is the real **466** (a 233-word mature
  generation is pre-allocated) — reporting the real figure is deliberate; `memory` is this
  runtime's own byte total (smaller, because the heap is); `status` is `running` for the calling
  process (single-threaded runtime) and the process's own `ProcessState`
  (`runnable`/`waiting`/`suspended`/`exiting`) otherwise; `group_leader` is the process's own
  (`Runtime::group_leader_of`, default the root pid, set by `erlang:group_leader/2`, inherited on
  spawn) — this runtime's real pid, not the oracle's `<0.70.0>` io process; `reductions` is this
  process's real counter; `error_handler` (the atom `error_handler`) and `suspending` (`[]`) are
  unchanging runtime defaults whose reported facts are true of this runtime;
  `message_queue_data` is `on_heap` — this runtime's real current placement
  (`beam_message::Queue` is heap-resident) — and the one divergence is an **explicit**
  `process_flag(message_queue_data, off_heap)`, accepted-and-ignored (honouring it needs per-process
  storage in `beam_process::Process`); `trace` is the **live** per-process ERTS tracee-flag
  **bitmask** as an integer (`0` while untraced), never `trace_info(P, flags)`'s atom list —
  `crate::trace::flags_to_atoms` mapped through `ERTS_TRACE_FLAG_BITS` (`1 << (3 + n)`; the 21 rows
  sum to the oracle's `[all]` exactly), the two flags with no canonical atom (`return_from`,
  `cpu_timestamp`) deliberately unmapped. **`last_calls` is the one real divergence**: hard-coded
  `false` while `Process::save_calls()` holds a real flag (`memory/mod.rs:803`).

### `erts_internal:*` (contract A.7) — `erts_internal.rs` (+ `erts_internal/trace.rs`)
`RuntimeContext::erts_internal_call(function, arity, args) -> Option<Result<Eterm>>`. `beam_bifs`
owns exactly **one** `erts_internal` BIF (`cmp_term/2`), which this module deliberately does not
claim so it falls through to the registry. `erts_internal.rs`'s rustdoc carries the per-BIF oracle
probes; the **served set** is: `term_type/1`, `get_dflags/0`, `beamfile_chunk/2`,
`beamfile_module_md5/1`, `map_to_tuple_keys/1`, `binary_to_integer/2`, `list_to_integer/2`,
`time_unit/0`, `perf_counter_unit/0`, `get_creation/0`, `is_process_alive/1,2`, `process_flag/3`,
`group_leader/2,3`, `no_aux_work_threads/0`, `processes_next/1`, `term_to_string/1,2`, `trace/3`,
`trace_pattern/3`, `garbage_collect/1`.

Key contracts / invariants:
* `get_dflags/0` emits the fixed record `{erts_dflags,468283523004,17230663572,468283523004,8396866,
  8192}` — identical on a non-distributed and a `-sname` node; its five flag words are
  `beam_dist::flags`'s constants (the single source of truth) narrowed to `i64` at the emit site
  (no local duplicate), and the term's `term_to_binary/1` image is byte-exact to the oracle's.
* `beamfile_chunk/2` (`beam_bif_load.c:165-201`) returns a named `.beam` chunk's payload as a
  **binary**; `undefined` for an absent or zero-length chunk or an unparseable container (a
  duplicate tag resolves to the **last** occurrence); `badarg` for a non-binary first arg or a name
  that is not a list of small ints. Chunk ids are the 4-byte ASCII tags, **case-sensitive**; the
  read goes through the public `beam_loader::container`, never hand-rolled parsing.
* `beamfile_module_md5/1` (`beam_bif_load.c:203-229`) returns the module checksum as a **bare
  16-byte binary** — the C's `erts_new_binary_from_data(sizeof beam.checksum, beam.checksum)`,
  **not** `beam_lib:md5/1`'s `{ok,{Module,Md5}}` wrapper (the Erlang-side shape). `undefined` for an
  image `beamfile_read` cannot parse (a malformed container, or a `FunT` chunk that is not
  `4 + 24n` bytes); `badarg` for a non-binary argument. The digest is
  `crate::context::module_md5` — the same chunk-layout digest the `NEW_FUN_EXT` identity uses, read
  from the argument binary, never a stored value.
* `map_to_tuple_keys/1` (`erl_map.c:3318-3328`): a **flat** map's keys tuple (atom keys in
  atom-index order); a hashmap ⇒ `badarg`; a non-map ⇒ `error:{badmap,Term}` (structured, live
  reason term). `binary_to_integer/2` (`big.c:4186-4232`) returns the C's *value* atoms; a small
  int, `big`, or `badarg`. `list_to_integer/2` returns `{Value, Rest}`, or
  `no_integer`/`not_a_list`/`badarg`/`big`. `time_unit/0` and `perf_counter_unit/0` are both
  `1000000000`; `get_creation/0` is `undefined` on a non-distributed node, else the creation
  integer.
* `is_process_alive/1` (`erl_bif_info.c:4069`): `true`/`false` for an internal pid, `badarg`
  otherwise. `is_process_alive/2` (`erl_bif_info.c:4058-4067`) is the reference-carrying variant
  Erlang's `/1` wrapper builds on: `(Pid, Ref)` ⇒ `ok` for a **live and a dead** pid (an argument
  test plus a signal send, never a table lookup); `badarg` for a non-pid / a non-runtime-reference.
  The signalling side is a typed gap this arm does not need.
* `process_flag/3` (`bif.c:2262-2297`, `process_flag_aux:1896-2022`) is the bare `process_flag_aux`
  surface for the **calling** process, implementing exactly the one flag the C helper does,
  `save_calls`: `(self(), save_calls, N)` ⇒ the previous length (`0`, then `7`, `4`, …); a
  non-small/out-of-range value or the `trap_exit` flag ⇒ `error:badarg`; a non-pid target ⇒ the
  **value** `badtype`; another process ⇒ the typed async-RPC gap. Deliberately **not**
  `erlang:process_flag/2`.
* `no_aux_work_threads/0` (`erl_process.c:9234-9237`) is this runtime's scheduler count `+ 1` (no
  dirty-allocator instances are modelled). **Divergence:** the count is *this* runtime's own
  (`crate::info::schedulers_count` — `+S N` else the logical-processor count), oracle-exact for
  every explicit `+S N` and different by construction otherwise, exactly like
  `system_info(schedulers)`.
* `processes_next/1` (`bif.c:4111-4120`) answers `{NextIndex, [Pid]}` / `{Limit, []}`, `badarg`
  past the limit. **Divergence:** index-addressed over *this* table, one pid per call (the C ptab
  is paginated).
* `term_to_string/1` (`bif.c:4160-4189`) is the `~w` rendering as a string, one line, no newline;
  `term_to_string/2`'s `undefined` limit form is exact, a **numeric** limit is a typed
  `unsupported` (the `~P` depth directive is not exposed by `../format.rs`), and a limit `<= 1` or a
  non-integer/non-`undefined` limit is `badarg`.
* `trace/3` (`erl_bif_trace.c`) is the body of `erlang:trace/2,3`: `PidSpec` selects the tracees (a
  pid the table holds, or the group atoms `all`/`existing`/`existing_processes`/`processes`), `How`
  sets/clears their flags, `FlagList` is the flag set, and the **caller** becomes the tracer; it
  returns the tracee count. `trace_pattern/3` installs/removes a per-function pattern over `MFA`
  (each of module/function/arity a name or the `'_'` wildcard) via `crate::trace`'s `PatternTable`.
  The port/`new`-style `PidSpec`s and a non-`true`/`false` `How` are the typed/`badarg` paths.
* **Divergences:** `term_type/1` reports every byte-aligned binary as `sub_binary` — the real
  `heap_binary`/`sub_binary`/`refc_binary` split is the boxed header subtag (`erl_map.c:3368-3376`),
  readable only through the `unsafe` `beam_term::boxed` this crate's `deny(unsafe_code)` forbids; a
  typed `unsupported` stays for the boxed families the safe API cannot name (an internal
  `reference`, a non-byte-aligned bitstring). `no_aux_work_threads/0` and `processes_next/1` derive
  from *this* runtime's own state (see above).

### `prim_file:*` (contract A.7) — `prim.rs` (path half) + `prim_file.rs` (fd half)
`RuntimeContext::prim_file_call(function, arity, args) -> Option<Result<Eterm>>`. `prim_file` is a
**preloaded** module whose BIFs are the C `prim_file_nif.c` NIFs; the floor claims the subset whose
whole observable contract is pinned byte-for-byte against the oracle, backed by the host's own file
system (`std::fs`/`std::env`) — "primitive" is exactly what this layer is for. Everything else
returns `None`, so the call falls through to the preloaded `.beam` (an honest
`erlang:nif_error(undef)`).

**Served (33 entries; per-entry ground truth/probes in each module's own rustdoc table):**
* **path half** (`prim.rs`): `get_cwd/0,1`, `read_file/1`, `delete/1`, `rename/2`, `make_dir/1`,
  `del_dir/1`, `read_link/1`, `make_link/2`, `make_symlink/2`, `altname/1`, `read_file_info/1,2`,
  `read_link_info/1,2`.
* **fd half** (`prim_file.rs`): `open/2`, `read/2`, `write/2`, `close/1`, `position/2`,
  `pread/2,3`, `pwrite/2,3`, `truncate/1`, `sync/1`, `datasync/1`, `write_file/2`, `set_cwd/1`,
  `ipread_s32bu_p32bu/3`, `get_handle/1`, `read_handle_info/1,2`.

**Invariants:** every path-only entry is a direct NIF call wrapped by `try … catch error:badarg ->
{error,badarg}` in `prim_file.erl`, so a **bad path is always the value `{error,badarg}`, never a
raise**; the C's errno REMAPs are reproduced explicitly (`unlink` `EISDIR`→`EPERM`; `rename`
`ENOTEMPTY`→`EEXIST` and an old path of exactly `/`→`EINVAL`; `rmdir` `ENOTEMPTY`→`EEXIST` and,
when that names the process's own non-empty cwd by ino/dev, `EINVAL`) — an omitted remap is a real
divergence. The path contract (from `encode_path/1`): a **binary** (its bytes verbatim; a NUL ⇒
badarg) or an **iolist** of code points (nested lists flattened; a nested **binary** ⇒ badarg; a
NUL, a non-scalar code point, a non-integer element or an improper tail ⇒ badarg) encoded as UTF-8;
the walk is iterative with an explicit stack, so nesting depth cannot exhaust the Rust stack. The
`errno`→atom map is the C's (the POSIX name lower-cased, with the C's own `errno_<n>` form outside
the table). `open/2` hands out the oracle's `{file_descriptor, prim_file, Data}` map shape and
follows the C's flag derivation (`read`→`O_RDONLY`; `write` alone→`O_TRUNC|O_WRONLY|O_CREAT`;
read+write→`O_RDWR|O_CREAT`; `append`→`O_APPEND` and no `O_TRUNC`; plus `exclusive`, `sync`,
`{read_ahead,S}`); every unmodelled option is silently ignored, exactly as the C does; a
`directory`-mode open is **served** (a real `O_DIRECTORY` plus the post-open type checks, with
`skip_type_check` skipping them).

**Divergences:** `read/2` does **not** reproduce read-ahead (`prim_buffer` is never called — its
only export is `on_load/0`); `get_handle/1`'s fd *number* is this runtime's own; a path built from a
file's raw bytes goes through `beam_codec::term::make_binary` on the pointer-stable `eval_heap` (no
64-byte limit, unlike `beam_engine::terms::make_heap_binary`).

**Typed gaps** (explicit `unsupported`, never a silent wrong value): `write_file_info/2,3`,
`advise/4`, `allocate/3`,
`list_dir/1`/`list_dir_all/1` (raw `readdir(3)` order, unpinnable), `read_line/1` (its Erlang body
drives the `prim_buffer:*` resource NIFs, all `undef`), `file_desc_to_ref/2`. **Deliberately
unclaimed** because the oracle answers the same: `copy/2`, `write_file/3`, `delayed_close/1`
(unexported ⇒ `error:undef`), `sendfile/8` (`{error,enotsup}`/`badarg`), and
`internal_get_nif_resource/1` (no NIF-resource model). `copy/3` works (pure Erlang).

### `file_server_2` — the native file service (contract A.9)
`file_server.rs` + `file_server/` own the runtime-native **file service**: the registered
`file_server_2` process (never enqueued, drained by the scheduler-pass step), its per-`open` native
io-devices, the `gen_server`/`file`/`io` request/reply wire, and the `{get_until,…}`/`{get_line,…}`
readers. The served `file_server` requests are `open/2` (any mode, incl. write/append),
`read_file/1`, `write_file/2`, `delete/1`, `make_dir/1`, `del_dir/1`, `read_link/1`, `altname/1`,
`rename/2`, `make_link/2`, `make_symlink/2`, `get_cwd/0,1`, `list_dir/1`, and
`read_file_info/1,2`/`read_link_info/1,2` (the whole 14-tuple `#file_info{}`, its `access` slot from
a real `access(2)` probe); the **io-device** protocol serves `close`, `position/1`, `sync`,
`datasync`, `truncate`, `get_chars/3`, `put_chars/2`, `get_until/4,5`, `get_line/1,2` and the
mutable read-mode/encoding surface `getopts`/`setopts(Opts)`, so `file:write_file/2,3` (over the
write-mode `{open,…}` that delegates to `prim_file:open/2` and really creates the path),
`file:read_line/1` and `io:getopts/1`/`io:setopts/2` round-trip to disk. **Full detail:
[`file_server/CONTEXT.md`](file_server/CONTEXT.md).**

## Constraints
- **`#![deny(unsafe_code)]`** — the crate-wide rule covers this whole floor. Terms are built and
  read only through `beam_engine::terms` (heap-seam builders/readers), `beam_codec::term`
  (`&mut beam_alloc::Heap` materializers), `beam_loader::container` (read-only container parsing)
  and `beam_term`; the parent's `render` drives the registered `io:format/2` printer rather than a
  private one.
- **Return-based errors** (workspace `../CONTEXT.md` §3): `Option<Result<Eterm>>`, never a
  crate-local error enum, never `panic!`/`unwrap()` on runtime data, never a silent swallow. A
  "value atom" answer the C *returns* (`badarg`, `big`, `no_integer`, `not_a_list`) is `Ok(atom)`.
- **Consulted before the registry.** A native entry must never shadow or duplicate a `beam_bifs`
  BIF; the `erlang`-namespace fallback classifies a known-unimplemented name as the capability gap
  (`unsupported: …`, CLI exit 3) and anything else as the bare `error:undef`.
- **`code:*` is the native floor, not a live `code_server`** — `code.rs`'s behaviour must not
  change (see `../CONTEXT.md`; OTP's real `code_server.beam` needs facilities outside this crate).
- **Files ~1000 lines of production code.** Pre-existing oversize (single-responsibility, kept
  whole because the routing table names them): `application.rs`, `process.rs`; `prim.rs` (~3237
  total / ~805 production) and `prim_file.rs` (~1536 total) are mostly comprehensive test modules.
  `erts_internal.rs`'s tracing half is split out into `erts_internal/trace.rs` (~310 production),
  leaving `erts_internal.rs` at ~968 production / 2265 total. Split by responsibility; every public
  item documented.

## Test Strategy
- **Unit tests** colocated per module (`#[cfg(test)] mod tests`), driving the floor through the
  crate's own dispatch (`test_support::call` ⇒ `RuntimeContext::call_bif` ⇒ `try_native`) so the
  *served surface* is pinned, not an internal function. Fixtures live once in `test_support.rs`.
- `memory/tests.rs` pins the `process_info` reply shapes, the failure modes, the `unsupported` set,
  and the oracle `registered_name` wrapping rules (the root pid is registered as `init`, so a test
  can `unregister` it to reach the live-unregistered case).
- **Oracle values** are quoted in each test's doc comment and were re-verified against the system
  `erl` (`erl -noshell -eval '…'`, OTP 29.1.1 / erts-17.1). Where the runtime answers a typed
  capability gap the oracle answers a value, the test pins the `unsupported` outcome with a
  `// documented divergence:` comment. `get_dflags/0` is pinned by byte-comparing the built term's
  `term_to_binary/1` image against the oracle's; `prim.rs`'s file-system entries run against a real
  per-test scratch dir (the file's `scratch_dir` helper, which clears any leftover first).
  `file_server/`'s suites are described in [`file_server/CONTEXT.md`](file_server/CONTEXT.md).
- Test fixtures that need a real `.beam` or the system Erlang skip **loudly**
  (`system_runtime()` / the file-local `lists_beam()`); the arithmetic/`term_type` suites need none.
- Cross-crate integration (CLI/exit codes, the preloaded `boot/*` corpus, `-eval`) is pinned by the
  `../tests/` suites, not here. This node's A.9 native-floor surface — `application:get_all_key/1`,
  `get_key/1,2`, `get_application/0,1`; `init:get_argument/1`'s automatic flags; `init:script_id/0`;
  `code:lib_dir/2`; the `filelib` existence probes — is pinned end to end, against the live oracle,
  by `../tests/a9_native_floor.rs`.
- The `init:*` termination/reboot family (`init:get_arguments/0`'s automatic prefix, `init:stop/0,1`
  exit codes, `init:restart/0`, `init:boot/1`) and `code:load_file/1`'s real load are pinned by
  `../tests/native_init.rs` and `../tests/code_load_file.rs`, driving the built binary and
  cross-checking the live oracle (skipping **loudly** without the toolchain).

## Routing Table
- `../native.rs` → the dispatch table (`try_native`, `FLOOR_MODULES`) + the `erlang`-core,
  `io:put_chars`, `seq_trace*` and `trace*` arms and the shared helpers (read for the served set;
  edit there to add an arm)
- `memory/` → `erlang:process_info/1,2` + `memory/tests.rs`
- `erts_internal.rs` → the `erts_internal` floor (`erts_internal_call`): `term_type/1`,
  `get_dflags/0`, `beamfile_chunk/2`, `beamfile_module_md5/1`, `map_to_tuple_keys/1`,
  `binary_to_integer/2`, `list_to_integer/2`, `time_unit/0`, `perf_counter_unit/0`,
  `get_creation/0`, `is_process_alive/1,2`, `process_flag/3`, `group_leader/2,3`,
  `no_aux_work_threads/0`, `processes_next/1`, `term_to_string/1,2`, `garbage_collect/1`, and the
  `trace/3` + `trace_pattern/3` dispatch arms + its unit tests
- `erts_internal/trace.rs` → the `erts_internal:trace/3` + `:trace_pattern/3` handler bodies and
  their private pattern/target helpers, over `crate::trace`
- `prim.rs` → the path half of the `prim_file` floor (`prim_file_call`) + its unit tests: the path
  contract (`encode_path`/`collect_path`), the `errno`→atom table, and the served entries
  `get_cwd/0,1`, `read_file/1`, `delete/1`, `rename/2`, `make_dir/1`, `del_dir/1`, `read_link/1`,
  `make_link/2`, `make_symlink/2`, `altname/1`, `read_file_info/1,2`, `read_link_info/1,2`
- `prim_file.rs` → the file-descriptor half of the `prim_file` floor + its unit tests: `open/2`
  (incl. a `directory`-mode `O_DIRECTORY` open), `read/2`, `write/2`, `close/1`, `position/2`,
  `pread/2,3`, `pwrite/2,3`, `truncate/1`, `sync/1`, `datasync/1`, `write_file/2`, `set_cwd/1`,
  `ipread_s32bu_p32bu/3`, `get_handle/1`, `read_handle_info/1,2`
- `file_server.rs` (+ `file_server/`) → the native `file_server_2` service, its io-devices, the
  request/reply wire, the write path (`{write_file,…}` + the delegating write-mode `{open,…}`), the
  `{get_until,…}`/`{get_line,…}` readers and the `getopts`/`setopts` read-mode surface
  ([`file_server/CONTEXT.md`](file_server/CONTEXT.md)); the pass step is
  `Runtime::service_file_requests` (owned by `../runtime/scheduler.rs`)
- `init.rs` / `code.rs` / `application.rs` → the preloaded-module surfaces (contract A.7) — the
  `init` automatic flags (`root`/`home`/`progname`/`bindir`) + `init:script_id/0` + the
  termination/reboot family, the code server's `get_mode/0`, `lib_dir/2` and `priv_dir/1`, and the
  `application`
  config view (`get_key/1,2`, `get_all_key/1`, `get_application/0,1`)
- `process.rs` (+ `process/monitor.rs`, `process/exit.rs`) / `port.rs` → the process/spawn (incl.
  `spawn_monitor`) and registry/reference surfaces, the monitor/demonitor and exit-signalling
  surfaces, and the port surface (`port_command/2,3`, `port_control/3` and the closed-port/
  `registered_name` halves of `port_info/1,2`) (contracts A.4/A.7/A.8/A.9)
- `node.rs` → the node/cookie surface (`is_alive/0`, `get_cookie/0`, `set_cookie/2`, `nodes/0,1`)
- `group_leader.rs` → `erlang:group_leader/2` + `erts_internal:group_leader/2,3` over
  `Runtime::group_leaders`
- `test_support.rs` → the shared fixtures
- `../context/mod.rs` → `RuntimeContext::call_bif` / `try_native`'s caller and the host seams
- `../context/bridges.rs` → `module_md5` (the `.beam` MD5 digest `beamfile_module_md5/1` serves,
  re-exported `pub(crate)` by `../context/mod.rs` so the floor reuses it instead of re-deriving the
  chunk parse)
- `../runtime.rs`, `../codeserver.rs`, `../boot.rs`, `../port.rs`, `../timers.rs`, `../dist.rs` →
  the runtime state these BIFs read (owned elsewhere)
- `../error.rs` → `unsupported`/`undef`/`halt`/`live_reason_term` and the known-unimplemented list
- `../../../../erts/emulator/beam/` → legacy C ground truth (read-only)

## Not implemented — the `erts_internal` / `prim_*` BIFs left falling through
Every name not in a table above answers `None` from `erts_internal_call`/`prim_file_call`, so the
call falls through to the `beam_bifs` registry and then to a real module load (the `erts` `ebin` is
on the code path) — an honest failure, never a fabricated value. Grouped by the machinery each
needs:

- **Distribution / `net_kernel` (A.5 phase 2):** `abort_pending_connection/2`,
  `await_port_send_result/3`, `create_dist_channel/3`, `dist_ctrl_put_data/2`,
  `dist_spawn_init/1`, `dist_spawn_request/4`, `dynamic_node_name/0,1`, `new_connection/1`,
  `spawn_request/4`, `spawn_init/1`. Needs the distribution controller protocol, a live node
  identity feeding `erlang:node/0`, connection/channel multiplexing, remote spawn, and the
  `-connect_all`/`+zdbbl` configuration surface.
- **Tracing / breakpoints (`dbg`) — the debugger half only:** `erts_internal:trace_session_create/3`,
  `:trace_session_destroy/1`, `:trace/4`, `:trace_pattern/4`, `:trace_info/3`,
  `:notify_breakpoint_hit/3`, `erts_debug:breakpoint/2`, `erl_debugger:breakpoint/3`,
  `:breakpoints/1,3` and the `erts_trace_cleaner:*` pair (`bif.tab:799-805`, `:822-825`) stay gaps.
  The **process-tracing core IS served** (see above). The residual gaps need the
  debugger/breakpoint machinery and the trace-session bookkeeping, and the `/4` + `trace_info/3`
  surfaces add a flag/match-spec dimension `crate::trace` does not expose.
- **Microstate accounting / scheduler wall time:** `await_microstate_accounting_modifications/3`,
  `gather_microstate_accounting_result/2`, `await_sched_wall_time_modifications/2`,
  `gather_sched_wall_time_result/1`, `mc_iterator/1`, `mc_refill/1`, `scheduler_wall_time/1`,
  `system_flag_scheduler_wall_time/1`. Needs per-scheduler run-time accounting (the M1 scheduler
  is single-threaded and does not account).
- **System tasks / system monitors:** `request_system_task/3,4`, `spawn_system_process/3`,
  `system_check/1`, `gather_system_check_result/1`, `is_system_process/1`, `system_monitor/1,3`.
  Needs the system-task protocol between `init`/the application controller and side-effect-free
  system processes, plus the monitor subscription set. (`is_system_process/1` is **already served**
  by the `RuntimeHooks` bridge, `src/context/runtime_hooks.rs::is_process_predicate` — the floor
  must not shadow it; its one divergence is that the oracle's `true` for `init` is unreachable
  because nothing calls `beam_process::ProcessFlags::set_system`.)
- **Code purge / literal areas:** `check_dirty_process_code/2`, `check_process_code/1,3`,
  `is_process_executing_dirty/1`, `purge_module/2`, `prepare_loading/2`,
  `set_code_and_literal_cleaner_prio/1`, `wait_release_literal_area_switch/1`,
  `dirty_process_handle_signals/1`. Needs the code server's purge protocol (scanning every
  process's stack for references to a module's code) and the literal-area release handshake.
- **Atomics / counters / persistent terms:** `atomics_new/2`, `counters_new/1`, `counters_add/3`,
  `counters_get/2`, `counters_info/1`, `counters_put/3` and `erase_persistent_terms/0` are **already
  served** — the `counters_*`/`atomics_*` entries by the `beam_bifs` registry
  (`beam_bifs/src/runtime/atomics.rs`) and `erase_persistent_terms/0` by the `RuntimeHooks` bridge —
  so the floor must NOT claim them (an arm would duplicate the registry). They are listed only so a
  later reader does not re-add them.
- **ETS internals:** `ets_lookup_binary_info/2`, `ets_raw_first/1`, `ets_raw_next/2` need the raw
  `term_to_binary` **block** writer / `db_raw_first|next` key iterator, which `beam_ets`' public API
  does not expose. `ets_super_user/1` needs an ETS-protection-bypass bit on
  `beam_process::ProcessFlags` (answering `ok` without it would fabricate a grant).
  `ets_info_binary/1` is **not a BIF** (absent from `erts/emulator/beam/bif.tab`).
- **Process & scheduler control/introspection:** `erlang:garbage_collect/2` stays a typed
  capability gap. `process_display/2` needs a process **stack dump**, which the engine exposes no
  accessor for. `suspend_process/2` needs suspend/resume signalling; `await_result/1`,
  `flush_monitor_messages/3` and `'@flush_monitor_messages_refopt'/0` are **pure Erlang** receive
  loops, not BIFs at all.
- **Map internals:** `map_hashmap_children/1` and `map_next/3` are **already served** by the
  `beam_bifs` registry (`beam_bifs/src/erts_internal.rs`) — the floor must not claim them.
  (`beam_term::map`'s public API really is `map_pairs`/`map_pairs_storage_order`/`map_size`/
  `map_get`/`map_put`/`map_remove`/`map_words_capacity`, with no interior/path accessor; the
  registry's own implementation is the answer.)
- **Allocation/IO introspection & misc:** `gather_alloc_histograms/1`, `gather_carrier_info/1`,
  `gather_io_bytes/2`, `crasher/6` need the `erts_alloc` carrier/histogram readers, the
  async-thread pool, and the crash-dump/internals-dump path. `get_internal_state_blocked/1` is
  **pure Erlang** (it calls the unshipped `erts_debug:get_internal_state/1`) — not a floor
  candidate.
- **The port-BIF family** (`open_port/2`, `port_call/3`, `port_close/1`, `port_command/3`,
  `port_connect/2`, `port_control/3`, `port_info/1,2`) is **not** claimed *here*: the `erlang:*`
  counterparts are served by `../port.rs` (the `PortHook` seam: `open_port/2`, `port_close/1`, the
  live-port `port_info/1,2`), by the sibling [`port.rs`](port.rs) (`port_command/2,3`,
  **`port_control/3`** and the closed-port / `registered_name` halves of `port_info/1,2` — the
  forms the seam cannot express) and, for the `erts_internal:port_*` spellings this floor does not
  claim, by the `beam_bifs` registry over its `RuntimeHooks` seam.
- **`prim_*` preloaded driver modules:** the `prim_file` subset `prim.rs` + `prim_file.rs` serve (the
  **33** entries above) is on the floor; **every other** `prim_file` entry and all of
  `erl_prim_loader`, `prim_inet`, `prim_socket`, `prim_net`, `prim_zip`, `prim_buffer`, `prim_eval`
  are **Not implemented** and still fall through to a real preloaded-`.beam` load (they ship in the
  `erts` `ebin`, which is on the code path) whose `-nifs()`/`erlang:load_nif` entry then needs the
  matching host.
  * **Served beyond the table above:** `pread/2,3`, `pwrite/2,3` (cursor untouched, via safe
    `FileExt::read_at`/`write_at`), `truncate/1`, `sync/1`, `datasync/1`, `write_file/2`, `set_cwd/1`,
    `ipread_s32bu_p32bu/3`, `get_handle/1`; `copy/3` works (pure Erlang); `open(D,[directory,read])`
    ⇒ `{ok,Fd}` (`std::fs::File` opens with `O_DIRECTORY`; `open(D,[read])` ⇒ `{error,eisdir}`).
  * **Typed gaps:** `write_file_info/2,3`, `advise/4`,
    `allocate/3`, `file_desc_to_ref/2`, and `prim_file:read_line/1` (its Erlang body drives the
    `prim_buffer:*` resource NIFs — the library-level `file:read_line/1` is served by the
    `file_server_2` `{get_line,…}` arm,
    [`file_server/get_line.rs`](file_server/get_line.rs)).
  * `list_dir/1`/`list_dir_all/1` **not** claimed (raw `readdir(3)` order, unpinnable;
    `file_server.rs` *does* serve the `{list_dir,Name}` request over `std::fs::read_dir`).
    `prim_zip:splitter/3` unclaimed (pure Erlang, error paths not reproduced).
  * Deliberately unclaimed (an arm would *create* a divergence — the oracle answers the same):
    `copy/2`, `write_file/3`, `delayed_close/1` (unexported ⇒ `error:undef`), `sendfile/8`
    (`{error,enotsup}`/`badarg`), `internal_get_nif_resource/1` (no NIF-resource model).
  * Remaining machinery: `prim_socket`/`prim_net`/`prim_inet`, `prim_zip` (zlib), `prim_buffer`,
    `erl_prim_loader` (boot loader protocol), `prim_eval`. **Missing `beam_io` facilities:** a
    raw-syscall floor
    `fadvise`/`fallocate`/`chmod`/`chown`/`utimes`.

## Known Issues

### Curated-gap closability (`../error.rs`; oracle-checked, OTP 29.1.1 / erts-17.1)
The membership rule is single and strict: a `(function, arity)` belongs in
`KNOWN_UNIMPLEMENTED_ERLANG_BIFS` **iff** it is an OTP `erlang` BIF the oracle **exports and serves**
but this runtime does **not** serve. A name the oracle itself rejects as `undef` must never be
listed — curating one would answer the oracle's exit-1 "no such function" with an exit-3 capability
gap, a divergence dressed up as a gap. The rule is drift-guarded by
`a_name_the_otp_release_does_not_export_is_not_a_curated_gap`, which asserts the nine names the
release does not export (`apply/1`, `function_exported/2`, `load_module/3`, `system_info/2`,
`statistics/0`, `map_get/3`, `concatenate/2`, `register/3`, `system_profile/1`) are **not** listed
and classify as the bare `undef`.

The listed set is 19 `(function, arity)` pairs over 12 names: `set_cookie/2` (already served by the
floor, so an inert entry), `disconnect_node/1`, `port_connect/2`, `port_call/3`,
`spawn_request/1,2,3,4,5`, `process_flag/3`, `process_display/2`, `suspend_process/1,2`,
`system_monitor/0,1,2`, `garbage_collect/2`, `load_module/2`, `localtime_to_universaltime/1`.
**Closable via an `erlang.erl` wrapper arm** — the public `erlang:` name wraps an `erts_internal:*`
primitive this floor already serves: `erlang:process_flag/3` (`erlang.erl:5522-5531`) → the
**served** `erts_internal:process_flag/3`; `erlang:localtime_to_universaltime/1`
(`erlang.erl:11875-11881`) → the **registered** `/2` (`beam_bifs/src/calendar.rs`). A non-self
target lowers to `erlang:F(Node,...)`, whose `erlang.erl` clause calls `erts_internal:F(...)`
(`call_bif`'s `module == "erlang"` fallback, `context/mod.rs`). `erlang:garbage_collect/0,1` are
closable the same way and the floor already serves both, so only `/2` stays listed.
**Needs a new facility (shape-closable only):** `port_connect/2`/`port_call/3`,
`spawn_request/1..5`, `suspend_process/1,2`, `system_monitor/0,1,2` (`/0` ⇒ `undefined`;
`erts_internal:system_monitor/1,3` exported but unserved), `process_display/2`, `load_module/2`,
`disconnect_node/1` (needs `"net_kernel"` in `FLOOR_MODULES`). Edit gates: `../error.rs`'s
`known_unimplemented_bifs_are_capability_gaps_and_never_shadow_the_registry` +
`the_effective_gap_set_never_shadows_the_registry`, and `native.rs`'s
`every_floor_module_is_a_real_try_native_arm`.

### Recorded gaps and fabricated values
- **`last_calls` ⇒ `false` is a real divergence** — `Process::save_calls()` holds a real flag
  (`memory/mod.rs:803`).
- `application.rs`'s `app_env_props` reads the `.app` resource's `env` list verbatim, so kernel's
  start-time `logger`/`connect_all` injections are absent ⇒ `application:get_env(kernel,connect_all)`
  is `undefined` here vs `{ok,true}` on the oracle.
- `init:get_argument(progname)` follows the C rule — `$PROGNAME` when set, else the compile-time
  `"erl"` (`erlexec.c:1702-1705`); the oracle's `"erl"` comes from the same compile-time default (its
  launcher sets `PROGNAME` only for a renamed binary), so the two agree. `init:get_argument(root)`
  and `(home)` agree through the same arm; `bindir` is `<root>/erts-<vsn>/bin` of the installation
  the runtime resolved (`$ERTS_STDLIB_DIR`, else the filesystem probe), which is the oracle's own
  emulator directory whenever both run against the same installation.
- `init:script_id/0` is the **loaded** boot script's `{Name,Vsn}`; on the default `-eval`/`run` path
  no script is loaded, so it is the precise typed gap
  `unsupported: init:script_id/0 (no boot script has been loaded)` — never a fabricated version.
- `erlang:module_loaded/1` answers this runtime's **own** loaded set: the boot set is smaller than
  the oracle's (an app outside the preloaded `primLoad` batches is not yet loaded here), a
  documented divergence pinned by `../tests/module_loaded.rs`.
- `code:is_loaded/1`/`code:which/1` report **path resolvability**, not the code server's load state
  — the deliberate divergence that keeps the `boot/code_is_loaded_*` corpus cases agreeing; never
  "fix" it.
- `code:load_file/1` really loads with the oracle's `{error,sticky_directory|nofile|badfile}`
  vocabulary. Divergences: a **non-atom** argument is `badarg` where the oracle raises
  `function_clause`; a non-sticky module of an application outside `stdlib`/`kernel`/`erts` is
  `{error,nofile}` unless a `-pa` directory supplies it (this runtime's code path holds only the
  three system apps, the oracle's holds every application); the loader's typed error is folded into
  `badfile`; and the oracle's sticky/`badfile` logger report is not reproduced. Pinned by the
  colocated `code_load_file_*` units and `../tests/code_load_file.rs`.
- `init:restart/0` is **synchronous** and approximates the oracle's system teardown by re-running the
  boot sequence (no application-owned process or port exists to take down, and modules are never
  unloaded); `init:boot/1` returns `ok` where the real `boot/1` never returns (it enters init's
  receive loop). Pinned by `../tests/native_init.rs`.
- `application:get_application/1` on a non-atom that is not a pid leaves the function with no clause
  (`function_clause`, the oracle's own shape); the oracle's extra `is_pid/1` clause answers
  `undefined` for a pid — a documented divergence (no application-owned process runs here).
- `context/process_bridge.rs`'s `process_flag(error_handler, A)` accepts an atom where the oracle
  raises `badarg` (owner `context/`).
All other constants are truthful defaults or derive from real state (`application.rs`, `code.rs`,
`init.rs`, `port.rs`).

## Handoffs (owned by OTHER nodes)
The per-call dispatch arm for every native BIF is a registration in `../native.rs` (one level up,
outside *this* node's scope). The `("code","get_mode",0)` arm is reachable and agrees with the
oracle (`interactive`), though the parent's module-doc table still omits it (the table now also lists
`code:load_file/1` and the `init:stop/0,1`/`restart/0`/`boot/1` arms). The `file_server_2` service
answers a **scheduled** process's `gen_server:call` — `file:read/2`/`file:read_line/1`/`io:*` from a
`run` process complete through the ordinary `RunOutcome::Wait` suspend/resume
(`../../tests/scheduled_io_device.rs`), so no cross-node gap remains (detail in
[`file_server/CONTEXT.md`](file_server/CONTEXT.md)).

## See Also
- `../CONTEXT.md` → the crate façade, the CLI/`system_info` surfaces, and the A.8 contract.
- [`file_server/CONTEXT.md`](file_server/CONTEXT.md) → the `file_server_2` service (A.9).
- `../beam_gc`, `../beam_process`, `../beam_message` → the state this floor reads.
- `../../../../erts/emulator/beam/erl_bif_info.c` → the C ground truth (`pi_1_keys[]`,
  `process_info_2`'s forms, the `always_wrap`/`nil`-skip rules).
- `../../../../erts/emulator/beam/erl_map.c` → `term_type/1`'s family table; `dist.c` →
  `get_dflags/0`/`get_creation/0`; `beam_bif_load.c` → `beamfile_chunk/2`.
- `../../../../erts/emulator/nifs/` → `prim_file`'s C NIFs (`{common,unix}/prim_file_nif.c`,
  `unix/unix_prim_file.c`) and `../../../../erts/preloaded/src/prim_file.erl` → the wrappers.
