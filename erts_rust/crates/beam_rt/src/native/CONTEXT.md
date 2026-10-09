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
| `erts_internal.rs` | the `erts_internal` floor (14 BIFs) — see below |
| `prim.rs` | the `prim_file` floor (16 entries) — see below |
| `prim_file.rs` | the whole-file-descriptor half of the `prim_file` floor (`open/2` `read/2` `write/2` `close/1` `position/2`) + its unit tests |
| `file_server.rs` | the native **`file_server_2` service** (contract A.9) — the registered service process + its native io-devices, the `gen_server`/`file`/`io` wire, and the scheduler-pass step that services their mailboxes (see below) |
| `test_support.rs` | the shared test fixtures (`runtime`/`system_runtime`/`call`/`atom`/`chars`/`string_of`) |

**`init:get_arguments/0` lists ONLY the command-line flags**, so it differs from the oracle by exactly its four automatic `root`/`bindir`/`progname`/`home` entries — which is why a `RuntimeConfig::default()` runtime (an empty `init_args`, so the listing is `[]`) stops the real boot script three instructions earlier, at instruction 16 (`application:load/1`, an `error function_clause`); the boot-chain consequence is in [`../boot/CONTEXT.md`](../boot/CONTEXT.md).

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
| `message_queue_data` | `on_heap` (this runtime's `beam_message::Queue` is heap-resident — a true placement) |
| `trace` | `0` (no tracing subsystem in M1) |
| `suspending` | `[]` (no `suspend_process` in M1) |
| `last_calls` | `false` (no tracing subsystem in M1) |
| `memory` | this process's real bytes (`(total_heap_size + stack_size) * 8` + off-heap overhead) |

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
| `catchlevel` | the runtime has no catch-frame store reachable from here |
| `sequential_trace_token` | no `seq_trace` subsystem |
| `backtrace` | no binary backtrace (see `current_stacktrace`) |
| `magic_ref`, `parent`, `label` | no such PCB fields in M1 |
| `async_dist`, `priority_messages` | no such process flags in M1 |
| `garbage_collection_info` | per-generation block detail beyond `beam_gc::HeapInfo` |
| `binary` | no process registers off-heap refc binaries yet, and `beam_alloc::RefcBinary` exposes no refcount accessor for the `{Ptr,Size,Refc}` third field |

An item this runtime cannot answer is **omitted** by `process_info/1` (the same mechanism the C uses
for an unregistered `registered_name`), so the list is a genuine, shorter-by-construction answer.

#### Documented divergences (correct-but-different-by-construction — do NOT "fix" to a hardcoded figure)
- **Count:** `/1` emits 15 (registered) / 14 (unregistered) items vs the oracle's 17 / 16 **for a
  process with no readable execution position** — the `-eval` root process making its own call has
  no engine, so `current_function`/`initial_call` are omitted (see the execution-position note). For
  a process with a readable position the two are emitted and the count matches the oracle's.
- **`heap_size`** agrees with the oracle's 233 (the 233-word nursery), but **`total_heap_size`** is
  **466** (this collector pre-allocates a 233-word mature generation). The real figure is reported
  deliberately — inventing 233 would be a lie.
- **`memory`** is this runtime's own byte total (smaller than the oracle's, because the heap is).
- **`status`**: the calling process is `running` (this runtime is single-threaded, so exactly one
  process executes at a time — the oracle's answer for the caller); every other process carries its
  own `ProcessState` (a process parked in `receive` is `waiting`, matching the oracle).
- **`group_leader`** is the process's own group leader (`Runtime::group_leader_of(pid)`), defaulting to
  this runtime's root pid and settable by `erlang:group_leader/2` (and inherited on spawn) — one source
  of truth shared by `erlang:group_leader/0`, the `/2` BIFs and this item. It is this runtime's real
  pid, not the oracle's `<0.70.0>` io process.
- **`reductions`** is this process's real counter (the absolute value differs — the code differs).
- **`error_handler`/`trace`/`suspending`/`last_calls`/`message_queue_data`** are unchanging M1
  defaults (the facts they report are true of this runtime).

### `erts_internal.rs` — the served BIFs (contract A.7)
`RuntimeContext::erts_internal_call(function, arity, args) -> Option<Result<Eterm>>`; `beam_bifs`
owns exactly **one** `erts_internal` BIF (`cmp_term/2`), which this module deliberately does not
claim so it falls through to the registry.

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
| `no_aux_work_threads/0` | `erl_process.c:9234-9237` (value set `:6219-6223`) | this runtime's scheduler count `+ 1` (no dirty-allocator instances are modelled). **Divergence:** the count is *this* runtime's own (`crate::info::schedulers_count` — `+S N` else the logical-processor count), so the answer is oracle-exact for every explicit `+S N` and differs from the oracle's default by construction, exactly like `system_info(schedulers)` |
| `processes_next/1` | `bif.c:4111-4120` | `{NextIndex, [Pid]}` / `{Limit, []}`; `badarg` past the limit. **Divergence:** index-addressed over *this* table, one pid per call (the C ptab is paginated) |
| `term_to_string/1` | `bif.c:4160-4189` | the `~w` rendering as a string, one line, no newline |
| `term_to_string/2` | `bif.c:4160-4189` | the `undefined` limit form is exact; a **numeric** limit is a typed `unsupported` (the `~P` depth directive is not exposed by `../format.rs`); a limit `<= 1` or a non-integer/non-`undefined` limit is `badarg` |

Documented divergences (deterministic, never a wrong value):
- `term_type/1` reports every byte-aligned binary as `sub_binary`: the real
  `heap_binary`/`sub_binary`/`refc_binary` split is the boxed **header subtag**
  (`erl_map.c:3368-3376`), and the only predicates that read it are `unsafe`
  (`beam_term::boxed`), which this crate's `#![deny(unsafe_code)]` forbids. A **typed
  `unsupported`** stays for the boxed families the *safe* API cannot name at all — an internal
  `reference` and a non-byte-aligned bitstring.
- `no_aux_work_threads/0` derives from this runtime's own scheduler count (see the table), and
  `processes_next/1` is index-addressed over *this* table.
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
| `open/2` | `prim_file.erl:132-141` (`open_nif(encode_path(Name), Modes)` → `make_fd/2` → `#file_descriptor{module = prim_file, data = build_fd_data(...)}`), `build_fd_data/5` (`:512-527`); C `efile_open` (`unix_prim_file.c:173-226`) + `efile_translate_modelist` (`common/prim_file_nif.c:500-539`) | `{ok, {file_descriptor, prim_file, Data}}` where `Data` is the oracle's map shape (`handle`, `owner`, plus `r_buffer`/`r_ahead_size` only for a **read-capable** descriptor) plus the private `fd_registry_id`. Modes follow the C's flags (`read`→`O_RDONLY`; `write` alone→`O_TRUNC\|O_WRONLY\|O_CREAT`; read+write→`O_RDWR\|O_CREAT`; `append`→`O_APPEND` and no `O_TRUNC`; `exclusive`, `sync`, `{read_ahead,S}`); **every** unmodelled option (`raw`, `binary`, `list`, `compress`, `retry_read`, …) is silently ignored, exactly as the C does. `{error,badarg}` for a bad path or a non-list mode list; `{error,enoent}`/`{error,eisdir}`/`{error,eexist}`/`{error,eacces}`/… from the errno; a `directory`-mode open is the honest `unsupported` gap (`std::fs::File` cannot `OPENDIR`) |
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
and the last working directive of `bootstrap/bin/start.boot` — `{apply,{c,erlangrc,[]}}` — is the
first such caller (`c:erlangrc/0` → `file:path_eval/2` → `file:open("$HOME/.erlang",[read])`).
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
from the open mode) and `{put_chars,Enc,Chars}`. Every other request is the typed `unsupported` gap.

#### Honest capability gaps (never a fabricated term)
* **`{get_until,Enc,_Prompt,Mod,Func,XtraArgs}`** — the request `io:parse_erl_exprs/4`/
  `scan_erl_exprs/4` use. `file_io_server` answers it with `io_lib:get_until/4` →
  `apply(erl_scan, tokens, [Cont, Chars, Pos0, Options])` (`erl_scan:tokens/4`) — real Erlang
  tokenizer code, and a native service process hosts no engine. Owner: a **`beam_engine`**-class
  capability (an Erlang-engine host inside the native step), never a `beam_rt` gap to fake. The same
  applies to `get_line`/`setopts`/`getopts`/`pread`/`pwrite`/`advise`/`allocate`/`read_handle_info`,
  the legacy `put_chars` arities, a non-`latin1` encoding and `read_file_info`/`write_file`.
* **A scheduled process's `gen_server:call` to the service cannot complete** — the engine's
  non-blocking `fold_outcome` folds the callee's suspend into `error::blocked()`. Owner:
  **`beam_engine`**.
* **`io:request/2` is not served by this crate's native `io` handling** (`../native.rs`'s `io` arm
  covers only `put_chars/1,2`), so it loads the `io` module body and dies on `net_kernel:dflag_unicode_io/1`
  — the typed `unsupported` gap `unsupported: net_kernel:dflag_unicode_io/1`. Consequently
  `file:read/2`/`file:read_line/1` (the io-device read path) is unreachable from library code even
  though the device itself serves `{get_chars,…}`. Owner: **`beam_rt`** (`../native.rs`).

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
- **Files ~1000 lines of production code** (pre-existing oversize: `application.rs`, `process.rs`).
  `erts_internal.rs` (~1737 total / ~786 production) and `prim.rs` (~1261 / ~623) are mostly
  comprehensive test modules; each has one responsibility and is kept single-file because the
  routing table names it — split if a second responsibility appears. Split by responsibility;
  every public item documented.

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
- Test fixtures that need a real `.beam` or the system Erlang skip **loudly**
  (`system_runtime()` / the file-local `lists_beam()`); the arithmetic/`term_type` suites need none.
- Cross-crate integration (CLI/exit codes, the preloaded `boot/*` corpus, `-eval`) is pinned by the
  `../tests/` suites, not here.

## Routing Table
- `../native.rs` → the dispatch table + the `erlang`-core items and shared helpers (read for the
  served set; edit there to add an arm)
- `memory/` → `erlang:process_info/1,2` (this file's detailed surface) + `memory/tests.rs`
- `erts_internal.rs` → the `erts_internal` floor (`erts_internal_call`): `term_type/1`,
  `get_dflags/0`, `beamfile_chunk/2`, `map_to_tuple_keys/1`, `binary_to_integer/2`,
  `list_to_integer/2`, `time_unit/0`, `perf_counter_unit/0`, `get_creation/0`,
  `is_process_alive/1`, `no_aux_work_threads/0`, `processes_next/1`, `term_to_string/1,2` + its
  unit tests
- `prim.rs` → the `prim_file` floor (`prim_file_call`) + its unit tests: the path contract
  (`encode_path`/`collect_path`), the `errno`→atom table, and the served entries `get_cwd/0,1`,
  `read_file/1`, `delete/1`, `rename/2`, `make_dir/1`, `del_dir/1`, `read_link/1`, `make_link/2`,
  `make_symlink/2`, `altname/1`
- `file_server.rs` → the native `file_server_2` service (`FileServices`, its `Device` io-devices and
  the request/reply wire) + its unit tests; the pass step is `Runtime::service_file_requests`
  (owned by `../runtime/scheduler.rs`)
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
- **Tracing / breakpoints (`dbg`):** `breakpoint/4`, `notify_breakpoint_hit/3`, `trace/3,4`,
  `trace_info/3`, `trace_pattern/3,4`, `trace_session_create/3`, `trace_session_destroy/1`. Needs
  the trace/breakpoint machinery, the match-spec compiler and per-process trace flags (plus a
  group leader to deliver trace messages to).
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
  `counters_get/2`, `counters_info/1`, `counters_put/3`, `erase_persistent_terms/0`. Needs the
  off-heap refcounted resource model for `atomics`/`counters` and `persistent_term`'s global table.
- **ETS internals:** `ets_info_binary/1`, `ets_lookup_binary_info/2`, `ets_raw_first/1`,
  `ets_raw_next/2`, `ets_super_user/1`. Needs the raw ETS binary-block traversal (`misc` / `db`
  internals) and ETS protection-level / super-user bookkeeping.
- **Process & scheduler control/introspection:** `garbage_collect/1`,
  `process_display/2`, `process_flag/3`, `suspend_process/2`, `is_process_alive/2`,
  `await_result/1`, `flush_monitor_messages/3`, `'@flush_monitor_messages_refopt'/0`. Needs
  GC-on-demand, suspend/resume signalling, and the `{Ref, Result}` await protocol. (`process_display/2`
  additionally needs a process **stack dump**, which the engine exposes no accessor for;
  `flush_monitor_messages/3` and `'@flush_monitor_messages_refopt'/0` are **pure Erlang** receive
  loops, not BIFs at all — they are not floor candidates.)
- **Map internals:** `map_hashmap_children/1`, `map_next/3`. Needs a HAMT **walker** and the
  4-bits-per-level path encoder (`erts_debug:size/1`'s iterator) — `beam_term::map`'s public API is
  `map_pairs`/`map_size`/`map_get` only, with no interior-children or path accessor, so these can be
  served only after that LCA crate grows one.
- **Allocation/IO introspection & misc:** `gather_alloc_histograms/1`, `gather_carrier_info/1`,
  `gather_io_bytes/2`, `get_internal_state_blocked/1`, `crasher/6`. Needs the `erts_alloc`
  carrier/histogram readers, the async-thread pool, and the crash-dump/internals-dump path.
  (`erts_internal:beamfile_module_md5/1` is covered above; `module_info/0,1` is the ordinary
  compiler-generated surface.)
- **The port-BIF family** (`open_port/2`, `port_call/3`, `port_close/1`, `port_command/3`,
  `port_connect/2`, `port_control/3`, `port_info/1,2`) is **not** claimed *here*: the `erlang:*`
  counterparts are served by `../port.rs` + `beam_bifs`' `PortHook` seam. `erts_internal`'s own
  entries stay unclaimed (the preloaded wrapper is irrelevant to M1 callers).
- **`prim_*` preloaded driver modules:** the `prim_file` subset `prim.rs` + `prim_file.rs` serve (the
  16 entries in their tables) is on the floor; **every other** `prim_file` entry and all of
  `erl_prim_loader`, `prim_inet`, `prim_socket`, `prim_net`, `prim_zip`, `prim_buffer`, `prim_eval`
  are **Not implemented** and still fall through to a real preloaded-`.beam` load (they ship in the
  `erts` `ebin`, which is on the code path) whose `-nifs()`/`erlang:load_nif` entry then needs the
  matching host. Families still deliberately unclaimed:
  * the **rest of the fd family** — `pread`/`pwrite`/`allocate`/`truncate`/`sync`/`datasync`/
    `advise`/`read_handle_info`/`copy/3`/`write_file/2` (the last a pure Erlang composition) — plus
    a `directory`-mode `open/2`, which `std::fs::File` cannot `OPENDIR` (that open is the honest
    `unsupported` capability gap, never a fabricated handle);

  The served fd family keeps the oracle's `#file_descriptor{}` shape so the Erlang glue above it
  (`raw_file_io`, `raw_file_io_list`, `file_io_server`) pattern-matches unchanged, with three
  documented divergences: the fd registry is keyed by a **private `fd_registry_id`** map entry
  instead of decoding the `handle` reference (decoding needs `unsafe`); `r_buffer` holds a
  **reference marker** rather than a real `prim_buffer` (dead code: `prim_buffer`'s only export is
  `on_load/0`, and `read_1/5`'s `RBufSz >= RSz` clause always fires with one `read(2)`); and a
  malformed `Data` answers `{error,badarg}` where the oracle sometimes raises `{badmatch,_}`.  * **`read_file_info/1,2` / `read_link_info/1,2`** — the `#file_info{}` tuple is fillable from
    `std::fs::Metadata`/`MetadataExt` for every field **except** `access` (the C calls `access(2)`,
    absent from safe `std::` and the crate is `deny(unsafe_code)`), and the *default* time type is
    `local`, needing a TZ conversion the runtime does not have. Only `[{time,posix}]` (and
    `[{time,universal}]` by pure arithmetic) would be exact — deferring the whole family keeps the
    default form from answering a wrong value.
  * `list_dir/1` is **not** claimed: its entries come back in `readdir(3)` order, which the oracle
    does not sort and this runtime cannot pin. `prim_zip:splitter/3` is likewise unclaimed — pure
    Erlang over iolists (not a runtime primitive) whose error paths this floor does not reproduce.
  * Remaining machinery: the socket/NIF resource machinery (`prim_socket`, `prim_net`,
    `prim_inet`), zlib (`prim_zip`), a binary-buffer resource (`prim_buffer`), the boot loader
    protocol (`erl_prim_loader`), and the `prim_eval` iteration protocol. M1's own file/socket/OS
    floor is deliberately minimal (`os:cmd/1`, `beam_io`'s spawn driver, the port write stream) and
    is not a `prim_*` implementation.

## Handoffs (owned by OTHER nodes)
The per-call dispatch arm for every native BIF is a registration in `../native.rs` (one level up, the parent node — outside *this* node's scope).
`../native.rs`'s `try_native` carries the `("code", "get_mode", 0)` and `("application", "get_application", 0)` arms, so both entries are reachable and agree with the oracle (`code:get_mode()` ⇒ `interactive`; `application:get_application()` ⇒ `undefined`).
`../native.rs`'s module-doc table still omits those two names, so the parent node should add them there.
`init:get_argument(root)` and `init:get_argument(home)` agree with the oracle through the same `init:get_argument/1` arm.
`init:get_argument(progname)` is a deliberate, invocation-dependent gap: both runtimes report the basename **they** were invoked as (the oracle's `"erl"` comes from the `erl` shell script's `$0`), so the value is a property of the harness's invocation, not one this runtime may fabricate — never a wrong result.

## See Also
- `../../CONTEXT.md` → the crate façade, the CLI/`system_info` surfaces, and the A.8 contract.
- `../beam_gc`, `../beam_process`, `../beam_message` → the state this floor reads.
- `../../../../erts/emulator/beam/erl_bif_info.c` → the C ground truth (`pi_1_keys[]`,
  `process_info_2`'s forms, the `always_wrap`/`nil`-skip rules).
- `../../../../erts/emulator/beam/erl_map.c` → `term_type/1`'s family table; `dist.c` →
  `get_dflags/0`/`get_creation/0`; `beam_bif_load.c` → `beamfile_chunk/2`.
- `../../../../erts/emulator/nifs/` → `prim_file`'s C NIFs (`{common,unix}/prim_file_nif.c`,
  `unix/unix_prim_file.c`) and `../../../../erts/preloaded/src/prim_file.erl` → the wrappers.
