# `beam_rt/src/native/` — the runtime-native BIF floor

## Intent
The BIFs that are really **runtime** operations: they read/write this runtime's own state
(process table, port registry, code server, atom table, application view, parsed command line,
timer heap) and are therefore served *before* the `beam_bifs` registry is consulted. They exist
because `beam_bifs` owns the static `bif.tab` surface (arithmetic, comparison, term ops,
`lists`/`maps`/`binary`, `io:format`, `ets`, `records`, …) while a handful of names need the
host — and because `init:*`/`code:*`/`application:*`/`erts_internal:*` are **preloaded-module
surface** (contract A.7) whose real `.beam` implementations are written against process/message
machinery Milestone-1 does not run.

The floor is a *fall-through* layer: every entry returns `Option<Result<Eterm>>`; `None` means
"not a native BIF", and the caller (`RuntimeContext::call_bif`, `../context/mod.rs`) then tries
the registry (or loads the module). Nothing here duplicates a registry BIF, and a call the floor
does not claim is never answered with a fabricated value.

## API Surface
### The dispatch (owned by `../native.rs`, not this node)
`RuntimeContext::try_native(module, function, arity, args) -> Option<Result<Eterm>>` — one match
arm per native call (and exactly **one** for the whole `erts_internal` namespace:
`("erts_internal", _, _) => self.erts_internal_call(function, arity, args)`). The arm is the
source of truth for what is native; each submodule below owns the per-function bodies. Files:

| Module | Serves |
|---|---|
| `../native.rs` (parent) | module docs, `try_native`, the shared helpers (`atom`/`atom_name`/`charlist`/`render`/`string_list`/`write_output`/`chars_to_string`), the `erlang` core (`halt/0,1`, `display/1`, `io:put_chars/1,2`, `apply/2,3`, `system_info/1`, `send_after/3`, `cancel_timer/1`, `os:cmd/1`), the binary-surface and scheduler-model test modules |
| `init.rs` | `init:get_argument/1`, `get_arguments/0`, `get_plain_arguments/0`, `get_status/0` |
| `code.rs` | `code:get_path/0`, `all_loaded/0`, `which/1`, `is_loaded/1`, `is_sticky/1,2`, `ensure_loaded/1`, `lib_dir/0,1`, `root_dir/0` |
| `process.rs` | `erlang:make_ref/0`, `monitor/2`, `demonitor/1,2`, `spawn/1,2,3`, `spawn_link/1,2,3`, `spawn_opt/2,3,4`, `ports/0`, `processes/0`, `pre_loaded/0`, `group_leader/0`, `whereis/1`, `register/2`, `unregister/1`, `module_loaded/1` |
| `memory.rs` | `erlang:process_info/2` — the GC/heap items (`garbage_collection`, `heap_size`, `total_heap_size`) |
| `port.rs` | `erlang:port_command/2,3` and the closed-port half of `erlang:port_info/1,2` (the `PortHook` seam cannot express those) |
| `application.rs` | `application:get_env/1,2,3`, `get_all_env/1`, `get_key/2`, `loaded_applications/0`, `which_applications/0` over a minimal native `.app` view |
| `erts_internal.rs` | the `erts_internal` floor — see below |
| `prim.rs` | the `prim_file` floor (`get_cwd/0,1`, `read_file/1`) — see below |
| `test_support.rs` | the shared test fixtures (`runtime`/`system_runtime`/`call`/`atom`/`chars`/`string_of`) |

### `erts_internal.rs` — the served BIFs (contract A.7)
`RuntimeContext::erts_internal_call(function, arity, args) -> Option<Result<Eterm>>`; `beam_bifs`
owns exactly **one** `erts_internal` BIF (`cmp_term/2`), which this module deliberately does not
claim so it falls through to the registry.

| BIF | Ground truth | Behaviour (oracle-verified, OTP 29.1.1 / erts-17.1) |
|---|---|---|
| `term_type/1` | `erl_map.c:3336-3417` | `fixnum`, `atom`, `tuple`, `flatmap`, `hashmap`, `pid`, `port`, `nil`, `catch`, `list`, `hfloat`, `fun`, `export`, `sub_binary` |
| `map_to_tuple_keys/1` | `erl_map.c:3318-3328` | a **flat** map's keys tuple (atom keys in atom-index order); a hashmap ⇒ `badarg`; a non-map ⇒ `error:{badmap,Term}` (structured, live reason term) |
| `binary_to_integer/2` | `big.c:4186-4232` | the C's *value* atoms: a small int, `big`, or `badarg` |
| `list_to_integer/2` | `big.c:4234-4355` | `{Value, Rest}`, or `no_integer`/`not_a_list`/`badarg`/`big` |
| `time_unit/0`, `perf_counter_unit/0` | `erl_time_sup.c:2537-2539` | `1000000000` |
| `get_creation/0` | `dist.c:5594-5604` | `undefined` on a non-distributed node, else the creation integer |
| `is_process_alive/1` | `erl_bif_info.c:4069` | `true`/`false` for an internal pid; `badarg` otherwise |
| `processes_next/1` | `bif.c:4111-4120` | `{NextIndex, [Pid]}` / `{Limit, []}`; `badarg` past the limit. **Divergence:** index-addressed over *this* table, one pid per call (the C ptab is paginated) |
| `term_to_string/1` | `bif.c:4160-4189` | the `~w` rendering as a string, one line, no newline |
| `term_to_string/2` | `bif.c:4160-4189` | the `undefined` limit form is exact; a **numeric** limit is a typed `unsupported` (the `~P` depth directive is not exposed by `../format.rs`); a limit `<= 1` or a non-integer/non-`undefined` limit is `badarg` |

Documented divergences (deterministic, never a wrong value): `term_type/1` reports every
byte-aligned binary as `sub_binary`, and returns a typed `unsupported` for the boxed families the
*safe* `beam_term` API cannot name (bignum, reference, external pid/port/ref, non-byte-aligned
bitstring) where the oracle names them.

### `prim.rs` — the served `prim_file` BIFs (contract A.7, the `prim_*` preloaded modules)
`RuntimeContext::prim_file_call(function, arity, args) -> Option<Result<Eterm>>`. `prim_file` is a
**preloaded** module whose BIFs are the C `prim_file_nif.c` NIFs; the floor claims the subset whose
whole observable contract is pinned byte-for-byte against the oracle, backed by the host's own file
system (`std::fs`/`std::env`) — "primitive" is exactly what this layer is for.

| BIF | Ground truth | Behaviour (oracle-verified, OTP 29.1.1 / erts-17.1) |
|---|---|---|
| `get_cwd/0` | `prim_file.erl:787` → `get_cwd_nif` → `efile_get_cwd` (`unix_prim_file.c:1050`, `getcwd(3)`), decoded by `internal_native2name` | `{ok, Cwd}` with `Cwd` a **string** (char list); a native path that is not valid UTF-8 is the C's `{ok,{error,warning}}` (`erl_unicode.c:2774-2789`, warning type `warning`); an `errno` ⇒ `{error, Reason}` |
| `get_cwd/1` | `prim_file.erl:771-778` (pure Erlang clauses) | `[Letter,$:]` (`A`–`Z`/`a`–`z`) ⇒ `{error,enotsup}` (Unix `efile_get_device_cwd` is a constant `ENOTSUP`, `unix_prim_file.c:1042`); any other cons cell (incl. an **improper** list — the `[_|_]` clause) ⇒ `{error,einval}`; anything else ⇒ `{error,badarg}` |
| `read_file/1` | `prim_file.erl:564-570` → `read_file_nif`; the path through `encode_path` = `internal_name2native` (`erl_unicode.c:2629`) | `{ok, Binary}`; `enoent`/`eisdir`/`enotdir`/`eloop`/`eacces`/… via `errno`; a **bad path** ⇒ the **value** `{error,badarg}` (the wrapper's `try … catch error:badarg`, never a raise) |

The path contract (reproduced from `encode_path/1`): a **binary** (its bytes verbatim; a NUL ⇒
badarg) or an **iolist** of code points — nested lists flattened, a nested **binary** ⇒ badarg
(`erts_native_filename_need` never flattens one) — encoded as UTF-8, the Unix native filename
encoding. An atom ⇒ badarg (`erl_unicode.c:2645`); a NUL, a code point that is not a Unicode scalar
value (`:2306-2354`), a non-integer element, or an improper tail ⇒ badarg. The walk is iterative
with an explicit stack (mirroring the C's `ESTACK`), so nesting depth cannot exhaust the Rust stack.
The file contents are built by `beam_codec::term::make_binary` on the pointer-stable `eval_heap`
(no 64-byte limit, unlike `beam_engine::terms::make_heap_binary`); the `errno`→atom mapping is the
C's (`erl_errno_str.c:1229` + `errno_name`: the POSIX name lower-cased), with the C's own
`errno_<n>` form (`:47`) for an errno outside the table.

## Constraints
- **`#![deny(unsafe_code)]`** — the crate-wide rule covers this whole floor. Terms are built and
  read only through `beam_engine::terms` (heap-seam builders/readers), `beam_codec::term`
  (`&mut beam_alloc::Heap` materializers) and `beam_term`; the parent's `render` drives the
  registered `io:format/2` printer rather than a private one.
- **Return-based errors** (workspace `../CONTEXT.md` §3): `Option<Result<Eterm>>`, never a
  crate-local error enum, never `panic!`/`unwrap()` on runtime data, never a silent swallow. A
  "value atom" answer the C *returns* (`badarg`, `big`, `no_integer`, `not_a_list`) is `Ok(atom)`.
- **Consulted before the registry.** A native entry must never shadow or duplicate a `beam_bifs`
  BIF; the `erlang`-namespace fallback classifies a known-unimplemented name as the capability gap
  (`unsupported: …`, CLI exit 3) and anything else as the bare `error:undef`.
- **`code:*` is the native floor, not a live `code_server`** — `code.rs`'s behaviour must not
  change (see `../CONTEXT.md`; OTP's real `code_server.beam` needs facilities outside this crate).
- **Files ~1000 lines** (pre-existing oversize: `application.rs`, `process.rs`; `erts_internal.rs`
  is ~600 production lines plus its unit-test module), split by responsibility; every public item
  documented.

## Test Strategy
- **Unit tests** colocated per module (`#[cfg(test)] mod tests`), driving the floor through the
  crate's own dispatch (`test_support::call` ⇒ `RuntimeContext::call_bif` ⇒ `try_native`) so the
  *served surface* is pinned, not an internal function. Fixtures live once in `test_support.rs`.
- **Oracle values** are quoted in each test's doc comment and were re-verified against the system
  `erl` (`erl -noshell -eval '…'`, OTP 29.1.1 / erts-17.1). Where the runtime answers a typed
  capability gap the oracle answers a value, the test pins the `unsupported` outcome with a
  `// documented divergence:` comment.
- Test fixtures skip **loudly** (`system_runtime()`) when no system Erlang is available; the
  `erts_internal` suite needs none.
- Cross-crate integration (CLI/exit codes, the preloaded `boot/*` corpus, `-eval`) is pinned by the
  `../tests/` suites, not here.

## Routing Table
- `../native.rs` → the dispatch table + the `erlang`-core items and shared helpers (read for the
  served set; edit there to add an arm)
- `erts_internal.rs` → the `erts_internal` floor (`erts_internal_call`) + its unit tests
- `prim.rs` → the `prim_file` floor (`prim_file_call`) + its unit tests (the path encoder and the
  `errno`→atom table)
- `init.rs` / `code.rs` / `application.rs` → the preloaded-module surfaces (contract A.7)
- `process.rs` / `memory.rs` / `port.rs` → the process/registry/reference, GC-introspection and
  port-write surfaces (contracts A.4/A.8)
- `test_support.rs` → the shared fixtures
- `../context/mod.rs` → `RuntimeContext::call_bif` / `try_native`'s caller and the host seams
- `../runtime.rs`, `../codeserver.rs`, `../boot.rs`, `../port.rs`, `../timers.rs`, `../dist.rs` →
  the runtime state these BIFs read (owned elsewhere)
- `../error.rs` → `unsupported`/`undef`/`halt`/`live_reason_term` and the known-unimplemented list
- `../../../../erts/emulator/beam/` → legacy C ground truth (read-only)

## Not implemented — the `erts_internal` / `prim_*` BIFs left falling through
Every name not in the table above answers `None` from `erts_internal_call`, so the call falls
through to the `beam_bifs` registry and then to a real `erts_internal` module load (`erts` `ebin`
is on the code path) — an honest failure, never a fabricated value. Grouped by the machinery each
needs:

- **Distribution / `net_kernel` (A.5 phase 2):** `abort_pending_connection/2`,
  `await_port_send_result/3`, `create_dist_channel/3`, `dist_ctrl_put_data/2`,
  `dist_spawn_init/1`, `dist_spawn_request/4`, `dynamic_node_name/0,1`, `get_dflags/0`,
  `new_connection/1`, `spawn_request/4`, `spawn_init/1`. Needs the distribution controller
  protocol, a live node identity feeding `erlang:node/0`, connection/channel multiplexing, remote
  spawn, and the `-connect_all`/`+zdbbl` configuration surface.
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
  system processes, plus the monitor subscription set.
- **Code purge / literal areas / module loading:** `check_dirty_process_code/2`,
  `check_process_code/1,3`, `is_process_executing_dirty/1`, `purge_module/2`,
  `prepare_loading/2`, `beamfile_chunk/2`, `beamfile_module_md5/1`,
  `set_code_and_literal_cleaner_prio/1`, `wait_release_literal_area_switch/1`,
  `dirty_process_handle_signals/1`. Needs the code server's purge protocol (scanning every
  process's stack for references to a module's code) and the literal-area release handshake.
- **Atomics / counters / persistent terms:** `atomics_new/2`, `counters_new/1`, `counters_add/3`,
  `counters_get/2`, `counters_info/1`, `counters_put/3`, `erase_persistent_terms/0`. Needs the
  off-heap refcounted resource model for `atomics`/`counters` and `persistent_term`'s global table.
- **ETS internals:** `ets_info_binary/1`, `ets_lookup_binary_info/2`, `ets_raw_first/1`,
  `ets_raw_next/2`, `ets_super_user/1`. Needs the raw ETS binary-block traversal (`misc` / `db`
  internals) and ETS protection-level / super-user bookkeeping.
- **Process & scheduler control/introspection:** `garbage_collect/1`, `group_leader/2,3`,
  `process_display/2`, `process_flag/3`, `suspend_process/2`, `is_process_alive/2`,
  `await_result/1`, `flush_monitor_messages/3`, `'@flush_monitor_messages_refopt'/0`. Needs
  GC-on-demand, suspend/resume signalling, and the `{Ref, Result}` await protocol.
- **Map internals:** `map_hashmap_children/1`, `map_next/3`. Needs the HAMT iteration protocol
  behind `maps:iterator/2` ordered traversal.
- **Allocation/IO introspection & misc:** `gather_alloc_histograms/1`, `gather_carrier_info/1`,
  `gather_io_bytes/2`, `no_aux_work_threads/0`, `get_internal_state_blocked/1`, `crasher/6`.
  Needs the `erts_alloc` carrier/histogram readers, the async-thread pool, and the
  crash-dump/internals-dump path. (`module_info/0,1` is the ordinary compiler-generated surface.)
- **The port-BIF family** (`open_port/2`, `port_call/3`, `port_close/1`, `port_command/3`,
  `port_connect/2`, `port_control/3`, `port_info/1,2`) is **not** claimed *here*: the `erlang:*`
  counterparts are served by `../port.rs` + `beam_bifs`' `PortHook` seam. `erts_internal`'s own
  entries stay unclaimed (the preloaded wrapper is irrelevant to M1 callers).
- **`prim_*` preloaded driver modules:** the `prim_file` subset `prim.rs` serves
  (`get_cwd/0,1`, `read_file/1`) is now on the floor; **every other** `prim_file` entry
  (`open`/`read`/`write`/`pread`/`pwrite`/`position`/`stat`/`readdir`/links/permissions/…) and all
  of `erl_prim_loader`, `prim_inet`, `prim_socket`, `prim_net`, `prim_zip`, `prim_buffer`,
  `prim_eval` are **Not implemented** and still fall through to a real preloaded-`.beam` load (they
  ship in the `erts` `ebin`, which is on the code path) whose `-nifs()`/`erlang:load_nif` entry
  then needs the matching host. `list_dir/1` is deliberately **not** claimed by `prim.rs`: its
  entries come back in `readdir(3)` order, which the oracle does not sort and this runtime cannot
  pin. `prim_zip:splitter/3` is likewise unclaimed — it is pure Erlang over iolists (not a runtime
  primitive) whose error paths this floor does not reproduce. The remaining machinery: the open
  file-descriptor resource (open/read/write/pread/pwrite/seek/stat/readdir/links/permissions), the
  socket/NIF resource machinery (`prim_socket`, `prim_net`, `prim_inet`), zlib (`prim_zip`), a
  binary-buffer resource (`prim_buffer`), the boot loader protocol (`erl_prim_loader`), and the
  `prim_eval` iteration protocol. M1's own file/socket/OS floor is deliberately minimal
  (`os:cmd/1`, `beam_io`'s spawn driver, the port write stream) and is not a `prim_*` implementation.
