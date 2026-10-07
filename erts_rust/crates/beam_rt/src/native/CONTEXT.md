# `beam_rt::native` — the runtime-native BIF floor

## Intent
The runtime operations `beam_bifs` deliberately does **not** own: BIFs that read/write the
runtime's *own* state (the process/port tables, the code server, the parsed command line, the
application view, the timer heap). `RuntimeContext::try_native` (`../native.rs`) is consulted
**before** the `beam_bifs` registry; `None` means "not native — fall through". The module docs in
`../native.rs` list the full dispatch table; this file records the surface that needs more than a
one-liner (today: `erlang:process_info/1,2`).

## API Surface
Every entry is a `pub(super)` method on `RuntimeContext` returning `Option<Result<Eterm>>`; terms
are built through `beam_engine::terms` on the engine heap (`beam_rt` stays `#![deny(unsafe_code)]`).

## `erlang:process_info/1,2` (contract A.8) — `memory/mod.rs` (+ `memory/tests.rs`)

A **runtime** operation: the answer is read out of this runtime's own `beam_process` table
(`Process::state/mailbox/links/monitors/monitored_by/dict/registers/collector/off_heap`), served
before the registry.

### The reply shapes (oracle-verified against OTP 29.1.1 / erts-17.1)
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

### Failure modes (oracle-verified)
- non-atom / non-list / non-`{dictionary,_}` second arg ⇒ `error:badarg`;
- unknown item name (also **inside** a list, or an improper list) ⇒ `error:badarg`, **even** for a
  non-live pid;
- non-internal-pid first arg ⇒ `error:badarg`;
- a **valid** item with a live-but-absent pid ⇒ the atom `undefined` (all three forms; the list form
  of a dead pid is `undefined`, *not* `[]`).

### Implemented items (real value from this runtime's state)
| item | shape |
|---|---|
| `registered_name` | see the wrapping rules above (the one special item) |
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
| `group_leader` | this runtime's root pid (what `erlang:group_leader/0` answers) |
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

### Honest typed `unsupported` items (valid oracle names this runtime cannot answer)
Each raises the capability gap `unsupported: process_info: <name> …` (exit 3) — never a fabricated
value and never a false `badarg`. The gap names the missing accessor/hook:
| item | missing accessor / hook |
|---|---|
| `current_function`, `current_location`, `current_stacktrace` | the engine's current function/pc: `Machine` is moved **out** of `Runtime` for the duration of a scheduler pass and exposes no current-function/pc accessor |
| `initial_call` | no `$initial_call` / initial-MFA field on `beam_process::Process` |
| `catchlevel` | the runtime has no catch-frame store reachable from here |
| `sequential_trace_token` | no `seq_trace` subsystem |
| `backtrace` | no binary backtrace (see `current_stacktrace`) |
| `magic_ref`, `parent`, `label` | no such PCB fields in M1 |
| `async_dist`, `priority_messages` | no such process flags in M1 |
| `garbage_collection_info` | per-generation block detail beyond `beam_gc::HeapInfo` |
| `binary` | no process registers off-heap refc binaries yet, and `beam_alloc::RefcBinary` exposes no refcount accessor for the `{Ptr,Size,Refc}` third field |

An item this runtime cannot answer is **omitted** by `process_info/1` (the same mechanism the C uses
for an unregistered `registered_name`), so the list is a genuine, shorter-by-construction answer.

### Documented divergences (correct-but-different-by-construction — do NOT "fix" to a hardcoded figure)
- **Count:** `/1` emits 15 (registered) / 14 (unregistered) items vs the oracle's 17 / 16, because
  `current_function`/`initial_call` are honestly omitted as unsupported.
- **`heap_size`** agrees with the oracle's 233 (the 233-word nursery), but **`total_heap_size`** is
  **466** (this collector pre-allocates a 233-word mature generation). The real figure is reported
  deliberately — inventing 233 would be a lie.
- **`memory`** is this runtime's own byte total (smaller than the oracle's, because the heap is).
- **`status`** reflects *this* scheduler's `ProcessState` (a process parked in `receive` is
  `waiting`, matching the oracle).
- **`group_leader`** is this runtime's root pid (the real, live pid `erlang:group_leader/0` answers),
  not the oracle's `<0.70.0>` io process.
- **`reductions`** is this process's real counter (the absolute value differs — the code differs).
- **`error_handler`/`trace`/`suspending`/`last_calls`/`message_queue_data`** are unchanging M1
  defaults (the facts they report are true of this runtime).

## Constraints
- The floor is consulted **before** the `beam_bifs` registry; a native entry must never duplicate a
  registry BIF, and a call it does not claim returns `None` and falls through.
- `beam_rt` is `#![deny(unsafe_code)]` and `#![warn(missing_docs)]`: build terms only through
  `beam_engine::terms`, keep every public item documented.
- Errors are return-based (`beam_common::Result`); a capability gap is a raise whose reason atom
  starts with `unsupported` (`crate::error::unsupported`).

## Test Strategy
Colocated `#[cfg(test)]` modules per file; shared fixtures in `test_support.rs` (a code-path-less
`runtime()`, a system-`runtime()`, `call`/`atom`/`chars`/`string_of`). `memory/tests.rs` pins the
`process_info` shapes, the failure modes, the `unsupported` set, the oracle `registered_name` rules
(the root pid is registered as `init`, so a test can `unregister` it to reach the unregistered case),
and an oracle-comparison test that runs the system `erl` and SKIPS **loudly** when it is absent.
Oracle values are verified against system `erl` (OTP 29.1.1 / erts-17.1).

## Routing Table
- `../native.rs` → the floor's dispatch table (`try_native`) + the shared helpers + the `erlang`-core
  entries (`halt`, `display`, `io:put_chars`, `apply`, `system_info`, `send_after`/`cancel_timer`,
  `os:cmd`)
- `memory/` → `erlang:process_info/1,2` (this file's detailed surface)
- `init.rs` → `init:get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`, `get_status/0`
- `code.rs` → `code:get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1`, `is_sticky/1,2`, `ensure_loaded/1`, `lib_dir/0,1`, `root_dir/0`
- `process.rs` → the `erlang` process/registry/reference entries (`self/0`, `make_ref/0`, monitor family, spawn family, `ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1`)
- `port.rs` → `port_command/2,3` + the closed-port half of `port_info/1,2`
- `application.rs` → `application:get_env/1,2,3`, `get_all_env/1`, `get_key/2`, `loaded_applications/0`, `which_applications/0`
- `test_support.rs` → the shared floor test fixtures

## See Also
- `../../CONTEXT.md` → the crate façade, the CLI/`system_info` surfaces, and the A.8 contract.
- `../beam_gc`, `../beam_process`, `../beam_message` → the state this floor reads.
- `../../../../erts/emulator/beam/erl_bif_info.c` → the C ground truth (`pi_1_keys[]`,
  `process_info_2`'s forms, the `always_wrap`/`nil`-skip rules).
