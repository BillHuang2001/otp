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
### The dispatch (owned by `../native.rs`, not this node)
`RuntimeContext::try_native(module, function, arity, args) -> Option<Result<Eterm>>` — one match
arm per native call (and exactly **one** for the whole `erts_internal` namespace:
`("erts_internal", _, _) => self.erts_internal_call(function, arity, args)`, and one for
`prim_file`). The arm is the source of truth for what is native; each submodule below owns the
per-function bodies. Files:

| Module | Serves |
|---|---|
| `../native.rs` (parent) | module docs, `try_native`, the shared helpers (`atom`/`atom_name`/`charlist`/`render`/`string_list`/`write_output`/`chars_to_string`), the `erlang` core (`halt/0,1`, `display/1`, `io:put_chars/1,2`, `apply/2,3`, `system_info/1`, `send_after/3`, `cancel_timer/1`, `os:cmd/1`), the binary-surface and scheduler-model test modules |
| `init.rs` | `init:get_argument/1` — a command-line flag from `RuntimeConfig::init_args`, plus the three **automatic** flags the emulator defines itself (`root` = the code root dir, `home` = `$HOME`, `progname` = the basename this runtime was invoked as — a deliberate, invocation-dependent divergence from the oracle's `erl`) — `get_arguments/0`, `get_plain_arguments/0`, `get_status/0` |
| `code.rs` | `code:get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1`, `is_sticky/1,2`, `ensure_loaded/1`, `lib_dir/0,1`, `root_dir/0`, `get_mode/0` (the code server's `interactive`/`embedded` mode, from the `-mode` init argument per `code.erl:1741-1765`) |
| `process.rs` (+ `process/monitor.rs`, `process/exit.rs`) | `erlang:make_ref/0`, `spawn/1,2,3`, `spawn_link/1,2,3`, `spawn_opt/2,3,4` (incl. the heap-size options → `beam_gc::HeapPolicy`), `spawn_monitor/1,2,3,4`, `ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`; the monitor family (`monitor/2,3`, `demonitor/1,2`) in `process/monitor.rs`, `exit/2,3` in `process/exit.rs` |
| `node.rs` | `erlang:is_alive/0`, `get_cookie/0`, `set_cookie/2`, `nodes/0,1` |
| `memory/` | `erlang:process_info/1,2` — the full per-process introspection surface (see below) |
| `group_leader.rs` | `erlang:group_leader/2` + `erts_internal:group_leader/2,3` over `Runtime::group_leaders` (per-process, default the root pid; see below) |
| `port.rs` | `erlang:port_command/2,3` and the closed-port half of `erlang:port_info/1,2` (the `PortHook` seam cannot express those) |
| `application.rs` | `application:get_env/1,2,3`, `get_all_env/1`, `get_key/2`, `get_application/0` (always `undefined` — there is no application controller and no owning application), `loaded_applications/0`, `which_applications/0` over a minimal native `.app` view |
| `erts_internal.rs` | the `erts_internal` floor (`erts_internal_call`; the dispatch table is the source of truth) — see below |
| `erts_internal/trace.rs` | the `erts_internal:trace/3` + `:trace_pattern/3` handler bodies (`erlang:trace/2,3` and `erlang:trace_pattern/2,3` call them) + their private pattern/target helpers, over `crate::trace` — see below |
| `prim.rs` | the `prim_file` floor (27 entries) — see below |
| `prim_file.rs` | the whole-file-descriptor half of the `prim_file` floor + its unit tests — see below |
| `file_server.rs` | the native **`file_server_2` service** (contract A.9): the registered service process, its native io-devices, the `gen_server`/`file`/`io` wire and the scheduler-pass mailbox step (see below) |
| `file_server/get_until.rs` | the `{get_until, …}` io-device request (`get_until::serve` + the engine-backed `EngineContinuation`) + tests in `file_server/get_until/tests.rs` (see below) |
| `file_server/get_line.rs` | the `{get_line, Enc, _Prompt}` io-device request (`get_line::serve` + the `LineContinuation`) + tests in `file_server/get_line/tests.rs` (see below) |
| `test_support.rs` | the shared test fixtures (`runtime`/`system_runtime`/`call`/`atom`/`chars`/`string_of`) |

**`init:get_arguments/0` lists ONLY the command-line flags**, so it differs from the oracle by exactly its four automatic `root`/`bindir`/`progname`/`home` entries — which is why a `RuntimeConfig::default()` runtime (an empty `init_args`, so the listing is `[]`) stops the real boot script earlier, at instruction 16 (`application:load/1`, an `error function_clause`); the CLI-equivalent configuration runs the whole script to `{progress,started}` — the boot-chain consequence is in [`../boot/CONTEXT.md`](../boot/CONTEXT.md).

**The process-control families route through the sibling `beam_process` machinery.** `monitor/2,3` and `demonitor/1,2` use `ProcessTable::{monitor, monitor_named, demonitor, demonitor_flush}`: `[flush]` removes a queued `'DOWN'` from the caller's mailbox and `[info]` answers the **real** "was a monitor on record and got removed" boolean (a second `demonitor(Ref, [info])` is `false`; the reference stays on record so it is never `badarg`). `exit/2,3` signals another process through `ProcessTable::send_exit`, but a **self**-directed exit is raised directly (non-trapping caller, or the untrappable `kill`) or delivered as `{'EXIT', self(), R}` via `RuntimeContext::deliver_message` (trapping caller and `Reason != kill`) — because the sibling scheduler defers a self-directed signal that arrives while the pid is `Running`, which it is inside a slice. `exit/3` accepts ONLY the bare atom `priority` as an option (`[{priority, high}]` is `badarg`). `is_alive/0`/`get_cookie/0`/`set_cookie/2`/`nodes/0,1` read the runtime's `DistNode`/cookie/local-node state (`../runtime.rs`; `get_cookie/0` is the atom `nocookie` while the node is not alive, and `set_cookie/2` RAISES `badarg`/`distribution_not_started`, matching the oracle). `spawn_opt` parses and validates EVERY option **before** the child is allocated, then builds the child's `HeapPolicy` with `beam_gc::HeapPolicy::from_spawn_opts` and applies it via the policy-aware `Runtime::spawn_*_with_policy`.

**The spawn family works from *inside* a scheduler slice too.** `RuntimeContext::owns_process_table` picks the target: a context that owns the table (the `-eval` evaluator) spawns through `Runtime::spawn_process_with_policy`/`spawn_fun_process_with_policy` as before, while one built by `for_process` — a slice of a pass that has **moved** the runtime's table out — goes through `RuntimeContext::spawn_in_pass`: the child's pid is allocated in the **pass** table with its heap policy and priority (the same `ProcessTable::spawn` + `set_priority` the direct path uses), the entry point is resolved by `Runtime::resolve_export_entry` (the wording shared with the direct path, so module resolution/`undef` are identical), and the record is handed to the pass (`Runtime::note_pending_spawn`). `Runtime::absorb_pending_spawns` then registers the engine entry in the pass's runner state and enqueues the pid **at its own priority**, so the child is scheduled and really runs — `spawn_link`/`spawn_opt`'s `link`/`monitor` go through the same table via `link_to_child`/`apply_spawn_options`. Proven end to end by `tests/spawn_from_slice.rs` (all five forms incl. a nested spawn, oracle-cross-checked) and by the two `runtime::scheduler::tests` cases over `absorb_pending_spawns`.

**`erlang:spawn_monitor/1,2,3,4` is an `erlang.erl` wrapper, not a `bif.tab` BIF.** The four arities (`erlang.erl:10667`/`:10686`/`:10706`/`:10874`) are `erlang:spawn_opt(…, [monitor])` behind `erlang.erl`'s own guards — `is_function(F, 0)` for `/1` and `/2`, plus `is_atom(Node)`, `is_atom(M)`, `is_atom(F)`, `is_list(A)` — so no registry entry (and nothing in the known-unimplemented list) claims them and the floor owns the whole family. A guard violation is the oracle's `badarg`; the `Node` forms on a node **other** than this runtime's own are the typed capability gap, because the oracle routes that clause through a distributed `erlang:spawn_request/5` (contract A.5) and this runtime has no send path for a spawn — never a locally fabricated child. The `Node` test is `process/monitor.rs`'s `pub(super)` `is_local_node`, one definition shared with `monitor/2,3`'s `{Name, Node}` target. The monitor itself is installed by the private `attach_monitor` (shared with `spawn_opt`'s `monitor` option, so the `{Pid, Ref}` shape and the reference → monitor bookkeeping cannot drift), and it is on record **before the child's first schedulable moment**: the child is only a run-queue entry (or a pass `PendingSpawn`) when the BIF returns and the runtime is single-threaded, so a child that exits immediately still delivers its `{'DOWN', …, normal}`. Proven end to end by `tests/spawn_monitor.rs` (all four arities on this runtime's own node, every guard rejection, and the node gap; oracle-cross-checked).

### `erlang:process_info/1,2` (contract A.8) — `memory/mod.rs` (+ `memory/tests.rs`)
A **runtime** operation: the answer is read out of this runtime's own `beam_process` table
(`Process::state/mailbox/links/monitors/monitored_by/dict/registers/collector/off_heap`), served
before the registry.

#### The reply shapes (oracle-verified against OTP 29.1.1 / erts-17.1)
- `process_info(Pid, Item)` (item **atom** or `{dictionary, Key}`) → `{Item, Value}` — **except**
  `registered_name`, whose value is the **bare** name atom, and the bare `[]` when the process is
  **unregistered** (the C's `always_wrap = !is_atom(opt)`).
- `process_info(Pid, [Item, …])` → `[{Item, Value}, …]` in the requested order, duplicates repeated.
  Here `registered_name` **is wrapped**: `{registered_name, Name}` or `{registered_name, []}`.
- `process_info(Pid, {dictionary, Key})` → `{{dictionary, Key}, Value | undefined}`.
- `process_info(Pid)` → the fixed `PI_1_KEYS` set (C `pi_1_keys[]`, `erl_bif_info.c:1008-1026`), in
  order: `registered_name, current_function, initial_call, status, message_queue_len, links,
  dictionary, trap_exit, error_handler, priority, group_leader, total_heap_size, heap_size,
  stack_size, reductions, garbage_collection, suspending`. A **registered** process emits all 17
  (`{registered_name, Name}` first); an **unregistered** one **omits** `registered_name` (the C skips
  it because `always_wrap = 0` makes it the bare `[]`, `erl_bif_info.c:1175`).

#### Failure modes (oracle-verified)
- non-atom / non-list / non-`{dictionary,_}` second arg ⇒ `error:badarg`;
- unknown item name (also **inside** a list, or an improper list) ⇒ `error:badarg`, **even** for a
  non-live pid;
- non-internal-pid first arg ⇒ `error:badarg`;
- a **valid** item with a live-but-absent pid ⇒ the atom `undefined` (all three forms; the list form
  of a dead pid is `undefined`, *not* `[]`).

#### Implemented items (real value from this runtime's state)
| item | shape |
|---|---|
| `registered_name` | see the wrapping rules above (the one special item) |
| `current_function` / `initial_call` | `{M,F,A}` of the function the process is in / was entered at, from the engine (`Machine::current_function`/`initial_call`) — see the execution-position note below |
| `current_location` | `{M,F,A,[{file,File},{line,Line}]}` from the engine (`Machine::current_location`) |
| `current_stacktrace` | the cooked stacktrace `[{M,F,A,Location}, …]`, innermost frame first (`Machine::current_stacktrace`) |
| `status` | `running` for the calling process (single-threaded runtime), else `runnable`/`waiting`/`suspended`/`exiting` from `ProcessState` |
| `message_queue_len` | integer — the mailbox length |
| `messages` | list of the queued payload terms, FIFO (**wrapped** like every other item) |
| `links` | list of the linked pids |
| `monitors` | list of `{process \| port, Pid}` |
| `monitored_by` | list of the watching pids |
| `dictionary` | list of `{Key, Value}` pairs, insertion order |
| `{dictionary, Key}` | the stored value, else `undefined` |
| `trap_exit` | `true`/`false` |
| `error_handler` | the atom `error_handler` (an unchanging runtime default) |
| `priority` | the scheduling priority atom (`normal`) |
| `group_leader` | the process's own group leader (`Runtime::group_leader_of`, default the root pid; set by `erlang:group_leader/2`, inherited on spawn) |
| `heap_size` / `total_heap_size` | this process's real heap capacities, in words |
| `stack_size` | the Erlang stack's word count |
| `reductions` | this process's real reduction counter |
| `garbage_collection` | the 5-item proplist `[{max_heap_size,Map},{min_bin_vheap_size,N},{min_heap_size,N},{fullsweep_after,N},{minor_gcs,N}]` (`beam_gc::GARBAGE_COLLECTION_KEYS` order) |
| `min_heap_size` / `min_bin_vheap_size` / `fullsweep_after` | the emulator's heap policy (`+hms`/`+hmbs`, rounded through the growth table) |
| `max_heap_size` | the process flag **map** `#{error_logger,include_shared_binaries,kill,size}` (`size` is the raw word bound, 0 = unlimited) |
| `message_queue_data` | `on_heap` — this runtime's real current placement (`beam_message::Queue` is heap-resident); the one divergence is an explicit `process_flag(message_queue_data, off_heap)`, accepted-and-ignored (see below) |
| `trace` | the process's **live** ERTS tracee-flag **bitmask** as an integer (`0` while untraced), read from [`crate::trace::TraceState`] and translated via `crate::trace::flags_to_atoms` + the `ERTS_TRACE_FLAG_BITS` table (this runtime's internal trace bits are deliberately not ERTS's); the two flags with no canonical atom (`return_from`, `cpu_timestamp`) are unmapped on purpose |
| `suspending` | `[]` (no `suspend_process` in M1) |
| `last_calls` | **documented divergence**: always `false`, but the oracle answers `false` only while `save_calls` is unset and the saved-call list (`[{M,F,A},…]`) once `process_flag(self(), save_calls, N)` is set — `Process::save_calls()` holds the flag so the truthful answer is readable, just not wired (`memory/mod.rs:803`) |
| `sequential_trace_token` | the process's sequential-trace token 5-tuple, or the bare `[]` while sequential tracing is inactive for it (read from `crate::seq_trace::token`) |
| `memory` | this process's real bytes (`(total_heap_size + stack_size) * 8` + off-heap overhead) |
| `catchlevel` | the live exception catch-frame depth as an integer, from `Machine::catch_frame_depth()` (the C's `catchlevel(rp) = rp->catches`); answered for a process with a readable engine, the typed gap otherwise (same conditionality as the execution-position items — a process parked outside any `try`/`catch` reports `0`) |

#### The execution-position items (the engine's view of *where* a process is)
`current_function`/`initial_call`/`current_location`/`current_stacktrace` are read from the
process's saved instruction pointer through the engine's read-only accessors
(`beam_engine::Machine::current_function` / `initial_call` / `current_location` /
`current_stacktrace`, whose builders are shared with the exception family's `build_stacktrace`).
`RuntimeContext::position_value` reaches them via `RuntimeContext::engines()` →
`runner::RunnerState::engine_at(pid) -> Option<(&Module, &Machine)>` (the runner's slice) or the
runtime's own runner state outside a pass. Two things make this answerable only *conditionally*:
- the **process table** must hold the pid: `snapshot` reads it through
  `RuntimeContext::process_table` — the **pass** table when the context is a scheduler slice (a
  pass *moves* the process table out of the runtime, leaving an empty placeholder), else the
  runtime's own;
- the pid must have a **readable engine/entry**: a **fun-bodied** process has no module to name
  `M`/`F` from, the **caller's own** process has its `Machine` moved out of the runner state while
  it runs, and a never-scheduled pid has none at all. Those report `Ok(None)` — the honest
  "cannot answer": the item is **omitted** by `process_info/1` (like an unregistered
  `registered_name`) and is the typed `unsupported` capability gap by `process_info/2`.

The end-to-end proof is `tests/position.rs`: `recv:wait/0` parks in a `receive` (fixture
`tests/fixtures/recv.erl`, clause head on **line 12**), a **second** process (`recv:probe_b/1`)
probes the parked pid, and the returned term is compared line-for-line against the live system
`erl` oracle (`[{current_function,{recv,wait,0}}, {initial_call,{recv,wait,0}},
{current_location,{recv,wait,0,[{file,"recv.erl"},{line,12}]}},
{current_stacktrace,[{recv,wait,0,[{file,"recv.erl"},{line,12}]}]}]`).#### Honest typed `unsupported` items (valid oracle names this runtime cannot answer)
Each raises the capability gap `unsupported: process_info: <name> …` (exit 3) — never a fabricated
value and never a false `badarg`. The gap names the missing accessor/hook:
| item | missing accessor / hook |
|---|---|
| `backtrace` | no raw/binary stack-dump accessor (`Machine` exposes only the cooked `current_stacktrace`) |
| `magic_ref` | the NIF resource API (`enif_alloc_resource`/`enif_make_resource`) is unbound, so no resource is bound into a process's off-heap table (oracle `[]`) |
| `parent` | no parent field on `beam_process::Process`/`ProcessFlags` — the spawner is not recorded |
| `label` | no label field on the PCB (`spawn_opt(_, [{label,_}])` is `badarg` even on the oracle) |
| `async_dist`, `priority_messages` | no such flags in `beam_process::ProcessFlags` |
| `garbage_collection_info` | `beam_gc::HeapInfo` measures no per-generation block sizes (the oracle's 11-key proplist) |
| `binary` | `beam_alloc::RefcBinary` exposes `refcount()`/`len()`/`data_ptr()`, but no process registers off-heap refc binaries in `Process::off_heap` (the engine holds them privately on `Machine::refc_binaries`), so the oracle's `[{Ptr,Size,Refc},…]` roster cannot be built (answering `[]` would be wrong for a process holding a large binary) |

An item this runtime cannot answer is **omitted** by `process_info/1` (the same mechanism the C uses
for an unregistered `registered_name`), so the list is a genuine, shorter-by-construction answer.

#### Documented divergences (correct-but-different-by-construction — do NOT "fix" to a hardcoded figure)
- **Count:** `/1` emits 15 (registered) / 14 (unregistered) items vs the oracle's 17 / 16 for a
  process with no readable execution position (the `-eval` root making its own call has no engine,
  so `current_function`/`initial_call` are omitted — see the execution-position note); with a
  readable position the count matches the oracle's.
- **`heap_size`** agrees with the oracle's 233 (the 233-word nursery), but **`total_heap_size`** is
  **466** (this collector pre-allocates a 233-word mature generation). The real figure is reported
  deliberately — inventing 233 would be a lie.
- **`memory`** is this runtime's own byte total (smaller than the oracle's, because the heap is).
- **`status`**: the calling process is `running` (this runtime is single-threaded, so exactly one
  process executes at a time — the oracle's answer for the caller); every other process carries its
  own `ProcessState` (a process parked in `receive` is `waiting`, matching the oracle).
- **`group_leader`** is the process's own (`Runtime::group_leader_of(pid)`, default the root pid;
  settable by `erlang:group_leader/2`, inherited on spawn) — one source of truth shared with
  `erlang:group_leader/0`; this runtime's real pid, not the oracle's `<0.70.0>` io process.
- **`reductions`** is this process's real counter (the absolute value differs — the code differs).
- **`error_handler`** (the atom `error_handler`) and **`suspending`** (`[]`) are unchanging runtime
  defaults — the facts they report are true of this runtime. **`last_calls`** is the exception: it is
  hard-coded `false` while `Process::save_calls()` holds a real flag — see its table row.
- **`message_queue_data`** is `on_heap`, this runtime's real **current** placement
  (`beam_message::Queue` is heap-resident). The one divergence: an **explicit**
  `process_flag(message_queue_data, off_heap)` is accepted-and-ignored, so such a process reports
  `on_heap` where the oracle reports `off_heap` — honouring it needs per-process storage in
  `beam_process::Process` (a cross-crate handoff).
- **`trace`** is the **live** per-process ERTS tracee-flag bitmask as an **integer**, never the
  `trace_info(P, flags)` atom list (oracle `{trace,0}` untraced, `{trace,1024}` after `[call]`,
  `{trace,32763895}` after `[all]`). `trace_bitmask` maps `flags_to_atoms(flags)` through the
  `ERTS_TRACE_FLAG_BITS` table (`1 << (3 + n)`), whose 21 rows sum to the oracle's `[all]` exactly;
  untraced ⇒ `0`. Drift-guarded by `memory/tests.rs`.

### `erts_internal.rs` — the served BIFs (contract A.7)
`RuntimeContext::erts_internal_call(function, arity, args) -> Option<Result<Eterm>>`; `beam_bifs`
owns exactly **one** `erts_internal` BIF (`cmp_term/2`), which this module deliberately does not
claim so it falls through to the registry. `get_dflags/0`'s five flag words are
`beam_dist::flags`'s constants (the single source of truth, each defined from the individual
`DFLAG_*` bits) narrowed to `i64` at the emit site — this module keeps **no** local duplicate.

| BIF | Ground truth | Behaviour (oracle-verified, OTP 29.1.1 / erts-17.1) |
|---|---|---|
| `term_type/1` | `erl_map.c:3336-3417` | the C's family name: `fixnum`, `atom`, `tuple`, `flatmap`, `hashmap`, `pid`, `port`, `nil`, `catch`, `list`, `hfloat`, `fun`, `export`, `sub_binary`, `bignum`, `external_pid`, `external_port`, `external_reference` |
| `get_dflags/0` | `dist.c:5566-5592` | the fixed flag record `{erts_dflags,468283523004,17230663572,468283523004,8396866,8192}` — identical on a non-distributed node and on a `-sname` node; two slots exceed 2^32 (wide fixnums), and the term's `term_to_binary/1` image is byte-exact to the oracle's |
| `beamfile_chunk/2` | `beam_bif_load.c:165-201` | a named `.beam` chunk's payload as a **binary**; `undefined` for an absent or zero-length chunk or an unparseable container (a duplicate tag resolves to the **last** occurrence — `read_beam_chunks`, `beam_file.c:1556-1581`); `badarg` for a non-binary first arg or a name that is not a list of small ints. Chunk ids are the 4-byte ASCII tags, **case-sensitive**. Read through the public `beam_loader::container`, never hand-rolled parsing |
| `beamfile_module_md5/1` | `beam_bif_load.c:203-229` | the module checksum as a **bare 16-byte binary** — the C's `erts_new_binary_from_data(sizeof beam.checksum, beam.checksum)`, **not** `beam_lib:md5/1`'s `{ok,{Module,Md5}}` wrapper (that is the Erlang-side shape). `undefined` for an image `beamfile_read` cannot parse (a malformed container, or a `FunT` chunk that is not `4 + 24n` bytes ⇒ `BEAMFILE_READ_CORRUPT_LAMBDA_TABLE`); `badarg` for a non-binary argument. The digest is `crate::context::module_md5` — the same chunk-layout digest the `NEW_FUN_EXT` identity uses, **not** a stored value |
| `map_to_tuple_keys/1` | `erl_map.c:3318-3328` | a **flat** map's keys tuple (atom keys in atom-index order); a hashmap ⇒ `badarg`; a non-map ⇒ `error:{badmap,Term}` (structured, live reason term) |
| `binary_to_integer/2` | `big.c:4186-4232` | the C's *value* atoms: a small int, `big`, or `badarg` |
| `list_to_integer/2` | `big.c:4234-4355` | `{Value, Rest}`, or `no_integer`/`not_a_list`/`badarg`/`big` |
| `time_unit/0`, `perf_counter_unit/0` | `erl_time_sup.c:2537-2539` | `1000000000` |
| `get_creation/0` | `dist.c:5594-5604` | `undefined` on a non-distributed node, else the creation integer |
| `is_process_alive/1` | `erl_bif_info.c:4069` | `true`/`false` for an internal pid; `badarg` otherwise |
| `is_process_alive/2` | `erl_bif_info.c:4058-4067` | the reference-carrying variant Erlang's `/1` wrapper is built on: `(Pid, Ref)` ⇒ `ok` for a **live and a dead** pid (the C is an argument test plus a signal send and never consults the table); `badarg` for a non-pid / a non-runtime-reference. The signalling side is a typed gap this arm does not need |
| `process_flag/3` | `bif.c:2262-2297` (`process_flag_aux:1896-2022`) | the bare `process_flag_aux` surface for the **calling** process, implementing exactly the one flag the C helper does, `save_calls`: `(self(), save_calls, N)` ⇒ the previous length (`0`, then `7`, `4`, …); a non-small/out-of-range value or the `trap_exit` flag ⇒ `error:badarg`; a non-pid target ⇒ the **value** `badtype`; another process ⇒ the typed async-RPC gap. Deliberately **not** `erlang:process_flag/2` |
| `no_aux_work_threads/0` | `erl_process.c:9234-9237` (value set `:6219-6223`) | this runtime's scheduler count `+ 1` (no dirty-allocator instances are modelled). **Divergence:** the count is *this* runtime's own (`crate::info::schedulers_count` — `+S N` else the logical-processor count), so the answer is oracle-exact for every explicit `+S N` and differs from the oracle's default by construction, exactly like `system_info(schedulers)` |
| `processes_next/1` | `bif.c:4111-4120` | `{NextIndex, [Pid]}` / `{Limit, []}`; `badarg` past the limit. **Divergence:** index-addressed over *this* table, one pid per call (the C ptab is paginated) |
| `term_to_string/1` | `bif.c:4160-4189` | the `~w` rendering as a string, one line, no newline |
| `term_to_string/2` | `bif.c:4160-4189` | the `undefined` limit form is exact; a **numeric** limit is a typed `unsupported` (the `~P` depth directive is not exposed by `../format.rs`); a limit `<= 1` or a non-integer/non-`undefined` limit is `badarg` |
| `trace/3` | `erl_bif_trace.c` (`trace_3`) | body of `erlang:trace/2,3` (handler in `erts_internal/trace.rs`, over `crate::trace`): `PidSpec` selects the tracees (a pid the table holds, or the group atoms `all`/`existing`/`existing_processes`/`processes`), `How` sets (`true`)/clears (`false`) their flags, `FlagList` the flag set; the **caller** becomes the tracer. Returns the tracee count. The port/`new`-style PidSpecs and a non-`true`/`false` `How` are the typed/`badarg` paths |
| `trace_pattern/3` | `erl_bif_trace.c` (`trace_pattern_3`) | body of `erlang:trace_pattern/2,3`: installs/removes a per-function trace pattern over `MFA` (each of module/function/arity a name or the `'_'` wildcard); consults `crate::trace`'s `PatternTable` |

Documented divergences (deterministic, never a wrong value): `term_type/1` reports every
byte-aligned binary as `sub_binary` — the real `heap_binary`/`sub_binary`/`refc_binary` split is the
boxed header subtag (`erl_map.c:3368-3376`), readable only through the `unsafe` `beam_term::boxed`;
a **typed `unsupported`** stays for the boxed families the safe API cannot name (an internal
`reference`, a non-byte-aligned bitstring). `no_aux_work_threads/0` and `processes_next/1` derive
from *this* runtime's own state (see the table).
### `prim.rs` — the served `prim_file` BIFs (contract A.7, the `prim_*` preloaded modules)
`RuntimeContext::prim_file_call(function, arity, args) -> Option<Result<Eterm>>`. `prim_file` is a
**preloaded** module whose BIFs are the C `prim_file_nif.c` NIFs; the floor claims the subset whose
whole observable contract is pinned byte-for-byte against the oracle, backed by the host's own file
system (`std::fs`/`std::env`) — "primitive" is exactly what this layer is for.

| BIF | Ground truth | Behaviour (oracle-verified, OTP 29.1.1 / erts-17.1) |
|---|---|---|
| `get_cwd/0` | `prim_file.erl:787` → `get_cwd_nif` → `efile_get_cwd` (`unix_prim_file.c:1050`, `getcwd(3)`), decoded by `internal_native2name` | `{ok, Cwd}` with `Cwd` a **string** (char list); a native path that is not valid UTF-8 is the C's `{ok,{error,warning}}` (`erl_unicode.c:2774-2789`, warning type `warning`); an `errno` ⇒ `{error, Reason}` |
| `get_cwd/1` | `prim_file.erl:771-778` (pure Erlang clauses) | `[Letter,$:]` (`A`–`Z`/`a`–`z`) ⇒ `{error,enotsup}` (Unix `efile_get_device_cwd` is a constant `ENOTSUP`, `unix_prim_file.c:1042`); any other cons cell (incl. an **improper** list — the `[_|_]` clause) ⇒ `{error,einval}`; anything else ⇒ `{error,badarg}` |
| `read_file/1` | `prim_file.erl:564-570` → `read_file_nif` | `{ok, Binary}`; `enoent`/`eisdir`/`enotdir`/`eloop`/`eacces`/… via `errno`; a **bad path** ⇒ the **value** `{error,badarg}` |
| `delete/1` | `prim_file.erl:804-809` → `efile_del_file` = `unlink(2)` (`unix_prim_file.c:991-1002`) | `ok`; else `{error, Reason}`; the C remaps `EISDIR`→**`EPERM`** (`:993-996`); a bad path ⇒ `{error,badarg}` |
| `rename/2` | `prim_file.erl:811-816` → `efile_rename` = `rename(2)` (`unix_prim_file.c:945-961`) | `ok`; the C remaps `ENOTEMPTY`→**`EEXIST`** and an old path of exactly `/`→**`EINVAL`** (`:947-955`); `EXDEV` passes through; either arg bad ⇒ `{error,badarg}` |
| `make_dir/1` | `prim_file.erl:817-822` → `efile_make_dir` = `mkdir(2)`, mode `0777` (`unix_prim_file.c:979-989`) | `ok`; an existing file *or* directory ⇒ `{error,eexist}`; a missing parent ⇒ `{error,enoent}`; a non-directory parent ⇒ `{error,enotdir}` |
| `del_dir/1` | `prim_file.erl:823-828` → `efile_del_dir` = `rmdir(2)` (`unix_prim_file.c:1004-1032`) | `ok`; the C remaps `ENOTEMPTY`→**`EEXIST`** and then, when the target names the process's own directory by `st_ino`/`st_dev` (`stat(".")`), **`EINVAL`** — so deleting a non-empty **cwd** is `einval`, not `eexist` |
| `read_link/1` | `prim_file.erl:590-606` → `efile_read_link` = `readlink(2)` (`unix_prim_file.c:859-895`), decoded by `translate_raw_name/2` | a valid UTF-8 target ⇒ `{ok, Target}` (a **string**; a dangling link still answers its target); a non-UTF-8 raw target, or a path that is not a link ⇒ `{error,einval}`; `enoent` for a missing path |
| `make_link/2` | `prim_file.erl:829-834` → `efile_make_hard_link` = `link(2)` (`unix_prim_file.c:963-969`) | `ok`; an existing link name ⇒ `{error,eexist}`; a missing source ⇒ `{error,enoent}`; bad path ⇒ `{error,badarg}` |
| `make_symlink/2` | `prim_file.erl:835-840` → `efile_make_soft_link` = `symlink(2)` (`unix_prim_file.c:971-977`) | `ok` (the target need not exist); an existing link name ⇒ `{error,eexist}`; bad path ⇒ `{error,badarg}` |
| `altname/1` | `prim_file.erl:842-848`; on Unix `efile_altname` is a constant `ENOTSUP` (`unix_prim_file.c:1085-1090`) | `{error,enotsup}` for every valid path (it does **not** return its argument); a bad path ⇒ `{error,badarg}` |
| `open/2` | `prim_file.erl:132-141` (`open_nif(encode_path(Name), Modes)` → `make_fd/2` → `#file_descriptor{module = prim_file, data = build_fd_data(...)}`), `build_fd_data/5` (`:512-527`); C `efile_open` (`unix_prim_file.c:173-226`) + `efile_translate_modelist` (`common/prim_file_nif.c:500-539`) | `{ok, {file_descriptor, prim_file, Data}}` where `Data` is the oracle's map shape (`handle`, `owner`, plus `r_buffer`/`r_ahead_size` only for a **read-capable** descriptor) plus the private `fd_registry_id`. Modes follow the C's flags (`read`→`O_RDONLY`; `write` alone→`O_TRUNC\|O_WRONLY\|O_CREAT`; read+write→`O_RDWR\|O_CREAT`; `append`→`O_APPEND` and no `O_TRUNC`; `exclusive`, `sync`, `{read_ahead,S}`); **every** unmodelled option (`raw`, `binary`, `list`, `compress`, `retry_read`, …) is silently ignored, exactly as the C does. `{error,badarg}` for a bad path or a non-list mode list; `{error,enoent}`/`{error,eisdir}`/`{error,eexist}`/`{error,eacces}`/… from the errno; a `directory`-mode open is **served** (a real `O_DIRECTORY` plus the post-open type checks — `open(D,[directory,read])` ⇒ `{ok,Fd}`, `open(File,[directory])` ⇒ `{error,enotdir}`, `skip_type_check` skips them) |
| `read/2` | `prim_file.erl:162-171` + `read_1/5` (`:180-204`) → `read_nif`; C `read_nif_impl` (`common/prim_file_nif.c:632-663`) | `{ok, Binary}` (`N` bytes, fewer at EOF) \| `eof` (0 bytes read and `N` ≠ 0) \| `{error, Reason}`. One `read(2)` of at most `N`: **read-ahead is not reproduced** (`prim_buffer` is never called — its only export is `on_load/0`; divergence documented in `prim_file.rs`). `N` must be a non-negative integer (`-1` ⇒ `{error,badarg}`, `1 bsl 80` ⇒ `{error,einval}`); a write-only descriptor ⇒ `{error,ebadf}`; a malformed `Data` ⇒ `{error,badarg}`; a closed/unknown handle ⇒ `{error,einval}` |
| `write/2` | `prim_file.erl:247-263` → `write_nif`; C `write_nif_impl` (`common/prim_file_nif.c:664-682`, `enif_inspect_iovec`) | `ok` \| `{error, Reason}`; the argument is iodata (a byte-aligned binary, a char list of byte-sized integers, proper sub-lists, `[]`, `<<>>`); an atom, a float, a negative/overflowing integer, a non-byte-aligned binary or an improper tail ⇒ `{error,badarg}`; a read-only descriptor ⇒ `{error,ebadf}` |
| `close/1` | `prim_file.erl:154-160` → `close_nif`; C `close_nif` (`common/prim_file_nif.c:612-631`) | `ok`; releases the registry entry, so a second `close` and every later op on that handle ⇒ `{error,einval}` (the C `file_handle_wrapper` CLOSED branch); an unknown/non-resource handle ⇒ `{error,badarg}` |
| `position/2` | `prim_file.erl:306-334` → `position_nif`; C `seek_nif_impl` (`common/prim_file_nif.c:782-806`) + `efile_seek` (`unix_prim_file.c:502-533`) | `{ok, NewPos}` \| `{error, Reason}`; `Loc` = `bof`/`cur`/`eof` (offset 0), `{Mark, Offset}`, or a bare non-negative integer (`{N, bof}`); a seek past EOF is legal; a negative resulting offset ⇒ `{error,einval}`; a bad mark or non-integer offset (`{1,bof}`, `foo`, `{bof,1.0}`) ⇒ `{error,badarg}`; a closed handle ⇒ `{error,einval}` |
Every path-only entry is a direct NIF call wrapped by
`try … catch error:badarg -> {error,badarg}` in `prim_file.erl`, so a **bad path is always the
*value* `{error,badarg}`, never a raise**; the C's errno REMAPs (`EISDIR`→`EPERM`,
`ENOTEMPTY`→`EEXIST`, the old-`/` and non-empty-cwd `EINVAL` cases) are reproduced explicitly — an
omitted remap is a real divergence.

The path contract (reproduced from `encode_path/1`): a **binary** (its bytes verbatim; a NUL ⇒
badarg) or an **iolist** of code points — nested lists flattened, a nested **binary** ⇒ badarg
(`erts_native_filename_need` never flattens one) — encoded as UTF-8, the Unix native filename
encoding. An atom ⇒ badarg (`erl_unicode.c:2645`); a NUL, a code point that is not a Unicode scalar
value (`:2306-2354`), a non-integer element, or an improper tail ⇒ badarg. The walk is iterative
with an explicit stack (mirroring the C's `ESTACK`), so nesting depth cannot exhaust the Rust stack.
A path built from a **file's raw bytes** (`read_file`'s result, `read_link`'s target) goes through
`beam_codec::term::make_binary` on the pointer-stable `eval_heap` (no 64-byte limit, unlike
`beam_engine::terms::make_heap_binary`). The `errno`→atom mapping is the C's
(`erl_errno_str.c:1229` + `errno_name`: the POSIX name lower-cased), with the C's own `errno_<n>`
form (`:47`) for an errno outside the table.

### `file_server.rs` — the native **`file_server_2`** service (contract A.9)
The runtime-native **file service** that completes the real boot chain. `lib/kernel/src/file.erl`
routes every non-`raw` file operation through `gen_server:call(file_server_2, Request, infinity)`,
and the file service is first *reached* at the `{apply,{c,erlangrc,[]}}` directive of
`bootstrap/bin/start.boot` (`c:erlangrc/0` → `file:path_eval/2` → `file:open("$HOME/.erlang",[read])`).
`FileServices` supplies the missing registered service. It is **not** a BIF: it is a process plus a
scheduler-pass step, the topic of the rest of this subsection.

#### The process model (never enqueued)
`FileServices::start(&mut ProcessTable, &mut AtomTable, HeapPolicy)` is called from `Runtime::new`
next to the `init` registration and runs **one** process, born `ProcessState::Runnable`, `insert`ed
into the table and registered under the atom `file_server_2`, but **never enqueued** and given no
engine — the pattern the runtime's own `init`/`self_pid` process already uses. Because
`Process::wake` only wakes a `Waiting`/`Suspended` process, a message delivered into the service's
mailbox never puts it on a run queue, so the runner never sees it and
`error::unknown_process_entry` is unreachable. `whereis(file_server_2)`, `monitor(process, Name)`
and a send all resolve it (`server()`/`is_running()` accessors). A successful `{open,…}` spawns a
**second** `Runnable`, never-enqueued process — the **io-device** — under the same heap policy, and
keys it in `FileServices::devices` as a `Device` (the open `std::fs::File` plus the `binary`/`list`
read mode `file_io_server:parse_options/1` derived).

#### The scheduler-pass step
`Runtime::service_file_requests(&mut ProcessTable) -> Result<usize>` drains every service/io-device
mailbox (`Process::receive`) and delivers each reply through `ProcessTable::deliver_simple`,
returning the delivery count; a reply to a since-dead caller is dropped
(`BeamError::TermNotFound`), exactly as `gen:reply/2` swallows its own send failure.
`scheduler_pass` calls it as an **additive step right after the run queue drains** (the same place
the port half runs); a non-zero count is added to the pass's `delivered` total and followed by
`scheduler.resume_woken`, so a serviced-only pass never looks like "nothing happened" and a caller
blocked in `gen_server:call`'s `receive` makes progress. The requesting process is normally the
runtime's own `Runnable` `self_pid`, which the delivery therefore does **not** enqueue; it resumes
through the engine's blocking-aware entry (`RunOutcome::Wait` → `Machine::resume_outcome`), not a
scheduler slice.

#### The wire protocols (verbatim from `lib/`)
| Request | Reply |
|---|---|
| `{'$gen_call', {Pid, Tag}, Request}` (the local `infinity` form, `gen.erl:231-250`) | `{Tag, Reply}` to `Pid`; a `[alias\|Ref]` tag is the typed `unsupported` gap (the alias delivery this runtime cannot perform), never a fabricated delivery |
| `{file_request\|io_request, From, ReplyAs, Request}` (`file_io_server.erl:199-233`) | `{file_reply\|io_reply, ReplyAs, Reply}` to `From` |
An unknown `file_server` request is the oracle's `{noreply, State}`: nothing is delivered. A
`FileServices` process never runs and never collects, so a mailbox payload (already copied onto the
service's own heap by `deliver_simple`) is stable for as long as it is read.

#### The served request surface (`file_server:handle_call/3`, `file_server.erl:101-201`)
| Request | Behaviour |
|---|---|
| `{open, Name, ModeList}` | `{error,einval}` for a non-list mode; `{error,badarg}` for a path `prim_file` rejects; `{error,eisdir}` for a **directory** (an explicit `metadata().is_dir()` check is required, because Linux `open(2)` **succeeds** on a directory while the oracle's `efile_open` answers `EISDIR`); another OS failure → the oracle's errno atom; a readable file → `{ok, IoPid}` with a fresh native io-device |
| `{read_file,_}`, `{delete,_}`, `{make_dir,_}`, `{del_dir,_}`, `{read_link,_}`, `{altname,_}` | reuse the existing `prim_file` primitives (`prim.rs`), so the errno remaps are identical |
| `{rename,_,_}`, `{make_link,_,_}`, `{make_symlink,_,_}` | reuse the `prim_file` primitives |
| `{get_cwd}` (bare atom or 1-tuple) / `{get_cwd,_}` | reuse `prim_file:get_cwd` |
| `{list_dir,_}` | `{ok, Names}` from `std::fs::read_dir` in the **host directory order** (`file.erl:1044` documents "The names are not sorted") |
| anything else (`write_file`, `read_file_info`, `copy`, …) | the typed `unsupported` capability gap, never a wrong value |

#### The io-device request surface (`file_io_server.erl:199-233`)
A request is either a **bare command atom** or a tuple whose head is the command: `file.erl` sends
`close`/`sync`/`datasync`/`truncate` as the bare atom (`:1536`) and the positioned form as
`{position,At}` (`:1954`). Served for real: `close` (retires the device), `sync`, `datasync`,
`truncate`, `{position,At}` (`At` is `bof`/`cur`/`eof` or `{Mark,Offset}`, and a bare non-negative
integer is `file.erl`'s `{N,bof}` shorthand), `{get_chars,Enc,Prompt,N}` (shaped `binary`/`list`
from the open mode), `{put_chars,Enc,Chars}`, `{get_until,Enc,Prompt,Mod,Func,XtraArgs}` (plus
the legacy 4-tuple form, whose encoding defaults to `latin1`), and `{get_line,Enc,_Prompt}` (plus
the legacy 2-tuple form, encoding defaulted to `latin1` — `file:read_line/1`'s request). Two
requests run Erlang code and own their read loop
([`file_server/get_until.rs`](file_server/get_until.rs),
[`file_server/get_line.rs`](file_server/get_line.rs)) — see the capability-gaps subsection below.
Every other request is the typed `unsupported` gap.
The reply tag is **paired** with the request tag, never echoed: `{io_request, …}` is answered
`{io_reply, ReplyAs, Reply}` and `{file_request, …}` with `{file_reply, ReplyAs, Reply}`
(`file_io_server.erl:199-239`), because every caller matches the reply on its **own** tag
(`io.erl:1670`'s `execute_request` matches `io_reply`).

#### Honest capability gaps (never a fabricated term)
* **`{get_until,Enc,_Prompt,Mod,Func,XtraArgs}` is served** by
  [`file_server/get_until.rs`](file_server/get_until.rs): the read loop drives an engine-backed
  `Continuation` running `Mod:Func(Cont, Data, XtraArgs…)`, decodes `{more,Cont}`/`{done,Reply,Buf}`
  and stashes `Buf` as the device's `State#state.buf`. Typed gaps: a continuation suspending in a
  `receive`; a leftover that is a deep iolist / non-charlist / negative code point / non-byte-aligned
  bitstring; an encoding other than `latin1`/`unicode`; a pid with no registered device ("closed io
  device"). The `{get_chars,…}` arm ignores the stashed buffer and a `{utf16,_}`/`{utf32,_}`
  **stream** is read as `latin1` (both shared divergences).
* **`{get_line,Enc,_Prompt}` is served** by [`file_server/get_line.rs`](file_server/get_line.rs):
  the same loop drives the `LineContinuation` (the oracle's `io_lib:collect_line/4` + `cast/4`), so
  `file:read_line/1`/`io:get_line/2` complete. Typed gaps: a request `Enc` that is neither `latin1`
  nor `unicode` (incl. a non-atom); invalid UTF-8 on a `unicode` device surfaces `beam_io`'s
  `invalid_unicode` stop where the oracle answers `{error,{no_translation,unicode,latin1}}`, after
  which a further request hits the "closed io device" gap rather than `{error,terminated}`.
* **Other io-device requests stay the typed `unsupported` gap** — `setopts`/`getopts`/
  `pread`/`pwrite`/`advise`/`allocate`/`read_handle_info`, the legacy `put_chars` arities, a
  non-`latin1` encoding and `read_file_info`/`write_file`.
* **A scheduled process's `gen_server:call` to the service cannot complete** — the engine's
  non-blocking `fold_outcome` folds the callee's suspend into `error::blocked()`. Owner:
  **`beam_engine`**.
* **The io-device read path completes from library code, `{get_line,…}` included.**
  `io:request/2` loads the `io` module body, `net_kernel:dflag_unicode_io/1` is served (the
  `RuntimeHooks` bridge) and the service answers with the tag **paired** to the request, so
  `file:read/2`, `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4` and `file:read_line/1` all complete
  and agree with the oracle (`../tests/io_scan.rs`; `file:read_line/1` additionally through the
  `{get_line,…}` arm above).

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
- `file_server.rs`'s colocated units pin the wire decoders (`improper_list_head`'s `[alias|Ref]`
  detection, `seek_from`'s `bof`/`cur`/`eof`/bare-integer forms, `iodata_bytes`, `tuple_fields`),
  the errno→atom table and the `binary`/`list` open-mode parse; the service is proven end to end over
  real temporary files/directories by `../tests/file_server.rs` (every rendering cross-checked
  against the live `erl`).
- `file_server/get_until/tests.rs` pins the `{get_until, …}` request: the protocol decoders
  (`interpret`, `leftover_from`), `Runtime::open_encoding`, the rejections, and the request end to
  end through `Runtime::serve_device_request` over a real temporary file with the real
  `erl_scan:tokens/4` on the engine (the stashed leftover is fed to the second request).
- **Oracle values** are quoted in each test's doc comment and were re-verified against the system
  `erl` (`erl -noshell -eval '…'`, OTP 29.1.1 / erts-17.1). Where the runtime answers a typed
  capability gap the oracle answers a value, the test pins the `unsupported` outcome with a
  `// documented divergence:` comment. `get_dflags/0` is pinned by byte-comparing the built term's
  `term_to_binary/1` image against the oracle's; `prim.rs`'s file-system entries run against a real
  per-test scratch dir (the file's `scratch_dir` helper, which clears any leftover first).
- Test fixtures that need a real `.beam` or the system Erlang skip **loudly**
  (`system_runtime()` / the file-local `lists_beam()`); the arithmetic/`term_type` suites need none.
- Cross-crate integration (CLI/exit codes, the preloaded `boot/*` corpus, `-eval`) is pinned by the
  `../tests/` suites, not here.

## Routing Table
- `../native.rs` → the dispatch table + the `erlang`-core items and shared helpers (read for the
  served set; edit there to add an arm)
- `memory/` → `erlang:process_info/1,2` (this file's detailed surface) + `memory/tests.rs`
- `erts_internal.rs` → the `erts_internal` floor (`erts_internal_call`): `term_type/1`,
  `get_dflags/0`, `beamfile_chunk/2`, `beamfile_module_md5/1`, `map_to_tuple_keys/1`,
  `binary_to_integer/2`, `list_to_integer/2`, `time_unit/0`, `perf_counter_unit/0`,
  `get_creation/0`, `is_process_alive/1,2`, `process_flag/3`, `group_leader/2,3`,
  `no_aux_work_threads/0`, `processes_next/1`, `term_to_string/1,2`, and the `trace/3` +
  `trace_pattern/3` dispatch arms (whose bodies live in `erts_internal/trace.rs`) + its unit tests
- `erts_internal/trace.rs` → the `erts_internal:trace/3` + `:trace_pattern/3` handler bodies (the
  functions `erlang:trace/2,3` and `erlang:trace_pattern/2,3` call) and their private
  pattern/target helpers, over `crate::trace` (no colocated tests — the tracing behaviour is pinned
  by `../tests/trace*.rs`)
- `prim.rs` → the path half of the `prim_file` floor (`prim_file_call`) + its unit tests: the path
  contract (`encode_path`/`collect_path`), the `errno`→atom table, and the served entries
  `get_cwd/0,1`, `read_file/1`, `delete/1`, `rename/2`, `make_dir/1`, `del_dir/1`, `read_link/1`,
  `make_link/2`, `make_symlink/2`, `altname/1`
- `prim_file.rs` → the file-descriptor half of the `prim_file` floor + its unit tests: `open/2`
  (incl. a `directory`-mode `O_DIRECTORY` open), `read/2`, `write/2`, `close/1`, `position/2`,
  `pread/2,3`, `pwrite/2,3`, `truncate/1`, `sync/1`, `datasync/1`, `write_file/2`, `set_cwd/1`,
  `ipread_s32bu_p32bu/3`, `get_handle/1`
- `file_server.rs` → the native `file_server_2` service (`FileServices`, its `Device` io-devices and
  the request/reply wire) + its unit tests; the pass step is `Runtime::service_file_requests`
  (owned by `../runtime/scheduler.rs`)
- `file_server/get_until.rs` → the `{get_until, …}` io-device request (`get_until::serve` + the
  engine-backed `EngineContinuation` implementing `beam_io::io_device::Continuation`) + its unit
  tests in `file_server/get_until/tests.rs`
- `init.rs` / `code.rs` / `application.rs` → the preloaded-module surfaces (contract A.7) — the `init` automatic flags (`root`/`home`/`progname`), the code server's `get_mode/0` and `application:get_application/0` included
- `process.rs` (+ `process/monitor.rs`, `process/exit.rs`) / `port.rs` → the process/spawn (incl. `spawn_monitor`) and registry/reference surfaces, the monitor/demonitor and exit-signalling surfaces, and the port-write surface (contracts A.4/A.7/A.8/A.9)
- `node.rs` → the node/cookie surface (`is_alive/0`, `get_cookie/0`, `set_cookie/2`, `nodes/0,1`) + its unit tests
- `test_support.rs` → the shared fixtures
- `../context/mod.rs` → `RuntimeContext::call_bif` / `try_native`'s caller and the host seams
- `../context/bridges.rs` → `module_md5` (the `.beam` MD5 digest `beamfile_module_md5/1` serves, re-exported `pub(crate)` by `../context/mod.rs` so the floor reuses it instead of re-deriving the chunk parse)
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
  The **process-tracing core IS served**: `erts_internal:trace/3` + `:trace_pattern/3`
  (`erts_internal/trace.rs`) and `erlang:trace/2,3`, `trace_pattern/2,3`, `trace_info/2`,
  `trace_delivered/1` plus the `seq_trace*` family (`../native.rs`) over `crate::trace`. The
  residual gaps need the debugger/breakpoint machinery and the trace-session bookkeeping, and the
  `/4` + `trace_info/3` surfaces add a flag/match-spec dimension `crate::trace` does not expose.
- **Microstate accounting / scheduler wall time:** `await_microstate_accounting_modifications/3`,
  `gather_microstate_accounting_result/2`, `await_sched_wall_time_modifications/2`,
  `gather_sched_wall_time_result/1`, `mc_iterator/1`, `mc_refill/1`, `scheduler_wall_time/1`,
  `system_flag_scheduler_wall_time/1`. Needs per-scheduler run-time accounting (the M1 scheduler
  is single-threaded and does not account).
- **System tasks / system monitors:** `request_system_task/3,4`, `spawn_system_process/3`,
  `system_check/1`, `gather_system_check_result/1`, `is_system_process/1`, `system_monitor/1,3`.
  Needs the system-task protocol between `init`/the application controller and side-effect-free
  system processes, plus the monitor subscription set. (`is_system_process/1` additionally needs
  `beam_rt` to flag its root/`init` process as a system process — `beam_rt` never calls
  `beam_process::ProcessFlags::set_system`, so answering it now would be wrong for `init`.)
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
  `ets_info_binary/1` is **not a BIF** (absent from `erts/emulator/beam/bif.tab`) and is removed
  from this list.
- **Process & scheduler control/introspection:** `garbage_collect/1` needs GC-on-demand driven from
  the heap-admission point (the root set is the *executing* machine's `x`+stack, so a BIF must not
  collect directly) — the missing API is a `Runtime::request_gc(pid)` latch consumed by
  `context/gc.rs::ensure_heap`; only the C's `minor`/`major` vs `badarg` argument check is servable
  in the floor today. `process_display/2` needs a process **stack dump**, which the engine exposes
  no accessor for. `suspend_process/2` needs suspend/resume signalling; `await_result/1`,
  `flush_monitor_messages/3` and `'@flush_monitor_messages_refopt'/0` are **pure Erlang** receive
  loops, not BIFs at all. `process_flag/3` and `is_process_alive/2` are **now served by the floor**
  (see the table above). `is_system_process/1` is **already served** by the `RuntimeHooks` bridge
  (`src/context/runtime_hooks.rs::is_process_predicate`) — the floor must not shadow it; its one
  divergence is that the oracle's `true` for `init` is unreachable because nothing calls
  `beam_process::ProcessFlags::set_system`.
- **Map internals:** `map_hashmap_children/1` and `map_next/3` are **already served** by the
  `beam_bifs` registry (`beam_bifs/src/erts_internal.rs`) — the floor must not claim them.
  (`beam_term::map`'s public API really is `map_pairs`/`map_pairs_storage_order`/`map_size`/
  `map_get`/`map_put`/`map_remove`/`map_words_capacity`, with no interior/path accessor; the
  registry's own implementation is the answer.)
- **Allocation/IO introspection & misc:** `gather_alloc_histograms/1`, `gather_carrier_info/1`,
  `gather_io_bytes/2`, `crasher/6` need the `erts_alloc` carrier/histogram readers, the
  async-thread pool, and the crash-dump/internals-dump path. `get_internal_state_blocked/1` is
  **pure Erlang** (it calls the unshipped `erts_debug:get_internal_state/1`) — not a floor
  candidate. (`erts_internal:beamfile_module_md5/1` is covered above; `module_info/0,1` is the
  ordinary compiler-generated surface.)
- **The port-BIF family** (`open_port/2`, `port_call/3`, `port_close/1`, `port_command/3`,
  `port_connect/2`, `port_control/3`, `port_info/1,2`) is **not** claimed *here*: the `erlang:*`
  counterparts are served by `../port.rs` + `beam_bifs`' `PortHook` seam. `erts_internal`'s own
  entries stay unclaimed (the preloaded wrapper is irrelevant to M1 callers).
- **`prim_*` preloaded driver modules:** the `prim_file` subset `prim.rs` + `prim_file.rs` serve (the
  **27** entries in their tables) is on the floor; **every other** `prim_file` entry and all of
  `erl_prim_loader`, `prim_inet`, `prim_socket`, `prim_net`, `prim_zip`, `prim_buffer`, `prim_eval`
  are **Not implemented** and still fall through to a real preloaded-`.beam` load (they ship in the
  `erts` `ebin`, which is on the code path) whose `-nifs()`/`erlang:load_nif` entry then needs the
  matching host.
  * **Served beyond the table above:** `pread/2,3`, `pwrite/2,3` (cursor untouched, via safe
    `FileExt::read_at`/`write_at`), `truncate/1`, `sync/1`, `datasync/1`, `write_file/2`, `set_cwd/1`,
    `ipread_s32bu_p32bu/3`, `get_handle/1` (a bare native-endian fd binary — divergence: the *number*
    is this runtime's own fd). `copy/3` works (pure Erlang); `open(D,[directory,read])` ⇒ `{ok,Fd}`
    (`std::fs::File` opens with `O_DIRECTORY`; `open(D,[read])` ⇒ `{error,eisdir}`).
  * **Typed gaps (explicit `unsupported`, never a silent wrong value):** `read_file_info/1,2`,
    `read_link_info/1,2`, `read_handle_info/1,2` — every `#file_info{}` field is fillable from
    `std::fs::Metadata`/`MetadataExt` **except** `access` (`access(2)` / `geteuid`+`getegid`, absent
    from safe `std`; the crate is `deny(unsafe_code)`); the default `local` time type is **not** a
    blocker (`posixtime_to_universaltime/1` + `universaltime_to_localtime/1` are served). Also
    `write_file_info/2,3` (`chmod`/`chown`/`utimes`), `advise/4`, `allocate/3`, `file_desc_to_ref/2`
    (raw fd adoption), `read_line/1` (drives the `prim_buffer:*` resource NIFs, all `undef`).
  * `list_dir/1`/`list_dir_all/1` **not** claimed: `readdir(3)` order, unpinnable (`file_server.rs`
    *does* serve the `{list_dir,Name}` request over `std::fs::read_dir`). `prim_zip:splitter/3`
    unclaimed (pure Erlang, error paths not reproduced).
  * Deliberately unclaimed (an arm would *create* a divergence — the oracle answers the same):
    `copy/2`, `write_file/3`, `delayed_close/1` (unexported ⇒ `error:undef`), `sendfile/8`
    (`{error,enotsup}`/`badarg`), `internal_get_nif_resource/1` (no NIF-resource model).
  * Remaining machinery: `prim_socket`/`prim_net`/`prim_inet`, `prim_zip` (zlib), `prim_buffer`,
    `erl_prim_loader` (boot loader protocol), `prim_eval`. **Missing `beam_io` facilities:** a safe
    `path_access` + `process_credentials()` (closes the whole `#file_info{}` family) and a raw-syscall
    floor `fadvise`/`fallocate`/`chmod`/`chown`/`utimes`.

## Known Issues

### Curated-gap closability (`../error.rs:415-453`; oracle-checked, OTP 29.1.1 / erts-17.1)
**Closable via an `erlang.erl` wrapper arm** — the public `erlang:` name wraps an `erts_internal:*` primitive this floor already serves: `erlang:garbage_collect/0` (`erlang.erl:3046`) + `/1` (`:3055`) → **served** `erts_internal:garbage_collect/1` (`erts_internal.rs:266`); `erlang:process_flag/3` (`:5522-5531`) → **served** `erts_internal:process_flag/3` (`erts_internal.rs:252`); `erlang:localtime_to_universaltime/1` (`:11875-11881`) → the **registered** `/2` (`beam_bifs/src/calendar.rs:418-421`). A non-self target lowers to `erlang:F(Node,...)`, whose `erlang.erl` clause calls `erts_internal:F(...)` (`call_bif`'s `module == "erlang"` fallback, `context/mod.rs:1810-1819`).
**Mislabeled — the oracle does not export the name either** (`{error,undef}`), so the runtime answers `unsupported`/exit 3 where the oracle exits 1: `register/3`, `system_profile/1`, `apply/1`, `function_exported/2`, `load_module/3`, `statistics/0`, `map_get/3`, `concatenate/2`, `system_info/2`. `set_cookie/2` is **already served** (`native.rs:232`, `node.rs:125-140`).
**Needs a new facility (shape-closable only):** `port_connect/2`/`port_call/3`, `spawn_request/2..5` (omits `/1`), `suspend_process/1` (omits `/2`), `system_monitor/*` (`/0` ⇒ `undefined`; `erts_internal:system_monitor/1,3` exported but unserved), `process_display/2`, `load_module/2`, `disconnect_node/1` (needs `"net_kernel"` in `FLOOR_MODULES`, `native.rs:116-125`). Edit gates: `error.rs:844-950`, `native.rs:837-869`.

### Recorded gaps and fabricated values
- **`last_calls` ⇒ `false` is a real divergence** — `Process::save_calls()` holds a real flag (`memory/mod.rs:803`).
- `application.rs:303-409` `app_env_props` omits kernel's `logger`/`connect_all` injections ⇒ `application:get_env(kernel,connect_all)` is `undefined` here vs `{ok,true}` on the oracle.
- `init:get_argument(bindir)` (`init.rs:89`) real value unused (falls through to `error`).
- `file_server.rs` does not serve `{write_file,Name,Data}` (`:337-357`) though `prim_file:write_file/2` IS implemented (`prim.rs:188`).
- `context/process_bridge.rs:323-331` `process_flag(error_handler, A)` accepts an atom where the oracle raises `badarg` (owner `context/`).
All other constants are truthful defaults or derive from real state (`application.rs:98`/`:194-196`, `code.rs:49`/`:183-185`, `init.rs:127-175`, `port.rs:148-154`/`:282-301`).

## Handoffs (owned by OTHER nodes)
The per-call dispatch arm for every native BIF is a registration in `../native.rs` (one level up, outside *this* node's scope); its `("code","get_mode",0)` and `("application","get_application",0)` arms are reachable and agree with the oracle (`interactive` / `undefined`), though its module-doc table still omits both names.
`init:get_argument(root)` and `init:get_argument(home)` agree with the oracle through the same `init:get_argument/1` arm.
`init:get_argument(progname)` is a deliberate, invocation-dependent gap: both runtimes report the basename **they** were invoked as (the oracle's `"erl"` comes from the `erl` shell script's `$0`), so the value is a property of the harness's invocation, not one this runtime may fabricate — never a wrong result.
The `file_server_2` service's remaining cross-node gap — a scheduled `gen_server:call` to the service (**`beam_engine`**) — is itemised in the `file_server.rs` subsection above.

## See Also
- `../../CONTEXT.md` → the crate façade, the CLI/`system_info` surfaces, and the A.8 contract.
- `../beam_gc`, `../beam_process`, `../beam_message` → the state this floor reads.
- `../../../../erts/emulator/beam/erl_bif_info.c` → the C ground truth (`pi_1_keys[]`,
  `process_info_2`'s forms, the `always_wrap`/`nil`-skip rules).
- `../../../../erts/emulator/beam/erl_map.c` → `term_type/1`'s family table; `dist.c` →
  `get_dflags/0`/`get_creation/0`; `beam_bif_load.c` → `beamfile_chunk/2`.
- `../../../../erts/emulator/nifs/` → `prim_file`'s C NIFs (`{common,unix}/prim_file_nif.c`,
  `unix/unix_prim_file.c`) and `../../../../erts/preloaded/src/prim_file.erl` → the wrappers.
