# `beam_rt::info` — `erlang:system_info/1` (contract A.8)

## Intent
The runtime introspection surface: `erlang:system_info/1`.
The source of truth is a **heap-free** function returning an [`InfoValue`], materialized later on a heap ([`InfoValue::to_eterm`]) exactly the way the oracle does — so every key is testable without allocating, and the term writer lives in one place.
The key surface is large, so `src/info.rs` is a thin parent and the per-key logic lives in family child modules.

The keys were verified key-for-key against the **system oracle** (OTP 29.1.1 / erts-17.1) with `erl -noshell -eval 'io:format("~p~n",[catch erlang:system_info(KEY)]), halt(0).'`.

## Module split
| module | keys |
|---|---|
| `info.rs` (parent) | docs · `InfoValue` + `to_eterm` · `SUPPORTED_KEYS` / `UNSUPPORTED_KEYS` · `INFO_ATOMS` · `system_info_value` dispatcher · `allocator_report` · shared helpers · re-exports `host::{os_type, system_architecture}` · `#[cfg(test)] test_support` |
| `info/host.rs` | the host-identity derivations `os_type` (`{unix,<os>}` / `{win32,nt}` from `std::env::consts::OS`) and `system_architecture` (best-effort target triple), via the pure `os_type_for` / `architecture_for` helpers |
| `info/system.rs` | build/host identity: `version`, `otp_release`, `system_architecture`, `system_version`, `nif_version`, `driver_version`, `wordsize`, `emu_flavor`, `emu_type`, `compat_rel`, `endian`, `machine`, `threads`, `smp_support`, `async_dist`, `os_type`, `os_version`, `break_ignored`, `debug_compiled`, `dynamic_trace`, `dynamic_trace_probes`, `modified_timing_level`, `update_cpu_info`, `trace_control_word`, `port_parallelism`, `kernel_poll`, `halt_flush_timeout`, `creation`, and the emulator's own implementation flags `eager_check_io`, `io_thread`, `port_tasks`, `lock_checking`, `lock_counting`, `ets_always_compress`, `ets_realloc_moves`, `hipe_architecture`, `literal_test`, `sequential_tracer` |
| `info/limits.rs` | `atom_count`, `atom_limit`, `process_count`, `process_limit`, `port_count`, `port_limit`, `ets_count`, `ets_limit`, `outstanding_system_requests_limit`, `dist_buf_busy_limit`, `delayed_node_table_gc` |
| `info/schedulers.rs` | `logical_processors` / `_available` / `_online`, `schedulers`, `schedulers_online`, `schedulers_total`, `schedulers_active`, `schedulers_state`, `all_schedulers_state`, `run_queues`, `cpu_quota`, `scheduling_statistics`, `total_scheduling_statistics`, `scheduler_id`, `dirty_cpu_schedulers` / `_online`, `dirty_io_schedulers`, `scheduler_bind_type`, `scheduler_bindings`, `multi_scheduling`, `multi_scheduling_blockers`, `normal_multi_scheduling_blockers`, `thread_pool_size`, `context_reductions` |
| `info/memory.rs` | `min_heap_size`, `min_bin_vheap_size`, `max_heap_size`, `fullsweep_after`, `heap_sizes`, `heap_type`, `message_queue_data`, `garbage_collection`, `alloc_util_allocators`, `allocated_areas` |
| `info/dist.rs` | `dist`, `dist_ctrl` — the distribution-state keys |
| `info/time.rs` | `start_time`, `end_time` — the ERTS monotonic-time **interface constants** |
| `info/unsupported.rs` | the 27 oracle-accepted keys this runtime honestly refuses |

Each family exposes `pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>>`; `None` means "not my key".
`system_info_value` chains the families (`system → limits → schedulers → memory → dist → time → unsupported`) and falls through to `error::unknown_info_key` (the oracle's `badarg`).

## The three disjoint key sets (the honesty rule)
| set | count | meaning |
|---|---|---|
| `SUPPORTED_KEYS` | 88 | answered with the oracle's exact value **shape** (`{allocator, A}` and `{wordsize, W}` are the two **tuple** keys, dispatched by `tuple_key_value`) |
| `UNSUPPORTED_KEYS` | 27 | the oracle accepts the key, but it describes a facility this runtime does not have (or one whose truthful own-state value cannot match the oracle's) → a **capability gap** raise whose reason atom starts with `unsupported` (exit 3 for a `-eval` caller), **never** a fabricated value or a hardcoded oracle number |
| everything else | — | `BeamError::BadArg`, exactly the oracle's reason for a key it does not accept |

### `SUPPORTED_KEYS`, by source of the value
**Genuine build/host constants** (oracle-equal on this host): `version` (our `beam_common::ERTS_VERSION`), `otp_release` (our OTP major), `system_architecture`, `nif_version` (`2.18`), `driver_version` (`3.3`), `wordsize` (8), `emu_flavor`, `emu_type`, `machine` (`"BEAM"`), `threads`, `smp_support`, `async_dist`, `os_type`, `os_version`, `break_ignored`, `debug_compiled`, `dynamic_trace`, `dynamic_trace_probes`, `modified_timing_level`, `update_cpu_info`, `trace_control_word` (this runtime's trace-control word, which is `0` — see below), `port_parallelism`, `kernel_poll`, `halt_flush_timeout`, `creation`, `atom_limit` (1048576), `process_limit` (1048576 or `+P N`), `ets_limit` (8192), `outstanding_system_requests_limit` (32), `dist_buf_busy_limit` (1048576), `delayed_node_table_gc` (60), `context_reductions` (`CONTEXT_REDS`, `erl_vm.h:47` — the ERTS reduction quantum, an interface constant), `heap_type`, `message_queue_data`, `scheduler_bind_type`, `min_heap_size`, `min_bin_vheap_size`, `fullsweep_after`, `garbage_collection`, `alloc_util_allocators`, `multi_scheduling_blockers`, `normal_multi_scheduling_blockers`, the **emulator's own build/implementation flags** — `endian` (`cfg!(target_endian)`, the C's `WORDS_BIGENDIAN`), `eager_check_io` (`true` — the C arm is `am_true` unconditionally and this runtime's scheduler pass checks I/O every step), `io_thread` (`false` — the `beam_io` poller is driven from the scheduler pass), `port_tasks` (`true`), `lock_checking`/`lock_counting`/`ets_always_compress`/`ets_realloc_moves` (`false` — the absent `ERTS_ENABLE_LOCK_CHECK`/`_COUNT` and ETS diagnostics), `hipe_architecture` (`undefined` — the C arm is `BIF_RET(am_undefined)`) and `sequential_tracer` (`{sequential_tracer,false}` — no system tracer installed) — the **monotonic-time interface constants** `start_time` (`-576460752000000000`) / `end_time` (`8646911283000000000`) — see below — and the `{wordsize, internal|external}` tuple key (`size_of::<Eterm>()` / `size_of::<usize>()`, `erl_bif_info.c:2521-2527`).

**Derived from this runtime's own configuration** — a real emulator rule reproduced from the build's constant tables rather than a literal: `port_limit` (`min(next_power_of_two(request), 134217727)`, or the default table `65536`; the request is `+Q N` or the clamped `ERL_MAX_PORTS` — `erts_ptab_init_table`/`erts_ptab_max`, `erl_ptab.c:383-388`/`erl_ptab.h:266-270`), `{allocator, A}` (`allocator_report`), `thread_pool_size` (the async-thread-pool size: the `+A N` the CLI preserves in `emulator.extra_emulator_flags`, reported as `max(1, N)` — matching the oracle's `+A 4`→`4` / `+A 0`→`1`; the oracle's own default `1` when `+A` is absent).

**Answered exactly in the oracle's non-distributed state** (`info/dist.rs`): `dist` is the **binary** `<<"=node:'nonode@nohost'\n=no_distribution\n">>` (39 bytes, each `\n` a real `0x0a`) when this runtime is not a node (`Runtime::dist_node() == None`), and `dist_ctrl` is the empty list `[]` when this runtime is not a node **or** its `DistNode::connections()` is empty.
`dist` falls back to the typed capability gap as soon as this runtime *is* a node (“no live distribution state”); `dist_ctrl` once that node carries a live link (“no distribution controllers”): the oracle's `dist` is a full wire dump (`=node:…`/`=proc:…` sections) and `dist_ctrl` one entry per controller, both needing the `net_kernel` protocol this runtime does not implement.
`RuntimeConfig::default()` is `nonode@nohost`, so the differential test's runtime observes the `[]` / 39-byte-binary answers.

**Self-derived from *this* runtime's own state** (correct-but-smaller — or simply different — from the oracle's run-time figure; the same rule `erlang:processes/0` / `ports/0` already follows; these must **never** be "fixed" to a hardcoded oracle number): `atom_count` (atom-table length), `process_count` (process-table length), `port_count` (live `PortRegistry` length), `ets_count` (live table-registry length), `system_logger` (the pid registered as `logger`, else `undefined` — see below), `allocated_areas` (one `size_of::<usize>()` slot per entry of this runtime's own live tables — `export_table` / `fun_table` from the loaded modules' `ExpT` exports / `FunT` lambda descriptors, `module_refs` / `module_table` / `loaded_code` from the loaded-module set, `dist_table` / `node_table` from `Runtime::dist_node()`; an item with no counterpart is an honest `0`), `heap_sizes` (`beam_gc::heap_sizes`), `logical_processors` / `_available` / `_online` (`available_parallelism`), `schedulers` / `schedulers_online` (`+S N` or the processor count), `schedulers_total` / `schedulers_active` / `run_queues` (aliases of the scheduler count — `schedulers_active` follows the same rule that every scheduler this runtime has is active), `schedulers_state` (`{Total,Online,Active}`, all the one count) and `all_schedulers_state` (the `normal` row always, plus a `dirty_cpu`/`dirty_io` row only when `+SDcpu`/`+SDio` configured a non-zero pool — the C omits a pool whose total is `0`), `cpu_quota` (`logical_processors`), `scheduling_statistics` / `total_scheduling_statistics` (the oracle's five `{Prio,Executed,Migrated}` rows with this runtime's honest `0` counters — no scheduling statistics are collected), `scheduler_id` (1), `dirty_cpu_schedulers` / `_online` and `dirty_io_schedulers` (`+SDcpu`/`+SDio`, else 0), `scheduler_bindings` (`unbound` per scheduler), `multi_scheduling` (derived from the scheduler count), `max_heap_size` (the emulator's heap options), and **`system_version`** — this runtime's own one-line self-description, deliberately its own string with the oracle's bracket-group shape (`Erlang/OTP <major> [erts-<vsn>] [source] [<bits>-bit]`).
The oracle appends further groups derived from emulator state this runtime does **not** model (the `smp:`/`ds:` scheduler and dirty-scheduler counts, the async-thread pool size, the JIT flavor); they are **omitted**, never copied or guessed — so `system_version` is compared **shape-only**.

The two **tuple** keys are answered by `info.rs::tuple_key_value(tag, arg)` (dispatched by `src/native.rs`): `{allocator, A}` → the flat, real `beam_alloc` rows (the oracle wraps them in one `{instance, N, …}` entry per allocator instance, which this runtime does not model — a documented divergence) and `{wordsize, internal|external}` → the host word size.
Any other tuple tag/argument is `badarg`, exactly as in the oracle (`{wordsize,foo}`, `{nonsense,x}`).

### `start_time` / `end_time` (`info/time.rs`)
**Compile-time interface constants of the ERTS monotonic-time interface — not measured instants and not a time server.**
`erts_get_monotonic_{start,end}_time` return `make_time_val(ERTS_MONOTONIC_TIME_{START,END}_EXTERNAL)` (`erl_time_sup.c:2148-2158`), dispatched from `am_start_time`/`am_end_time` (`erl_bif_info.c:2857-2859`), so every run of every erts-17.1 build answers the same pair: `start_time` = `-576460752000000000`, `end_time` = `8646911283000000000`.
The arithmetic is the C macro's, reproduced as Rust `const`s: `UNIT = 10⁹` ns (`erl_unix_sys.h:195`), `START_EXTERNAL = (MIN_SMALL/UNIT)*UNIT` with `MIN_SMALL = -(1<<59)` (`erl_time.h:312-323`, `erl_term.h:270`), `BEGIN = UNIT`, `END = (i64::MAX/UNIT)*UNIT`, `END_EXTERNAL = START + (END - BEGIN)` (the `START < 0` branch of `erl_time.h:403-407`); the internal→external offset is `START_EXTERNAL - BEGIN` (`erl_time.h:331-332`). The C's own invariants (`ASSERT(TIME_MIN < TIME_MAX)`, "START *need* to be a multiple of UNIT") are compile-time `assert!`s in the module.
`end_time` exceeds the 60-bit small range, so `make_time_val` materializes a bignum (`erts_sint64_to_big`, one `ErtsDigit` on `ARCH_64`); `InfoValue::Int` reproduces that choice (small when it fits, else `beam_codec::term::make_big`).
These are the only two keys whose value **must** be byte-equal to the oracle's — the recorded figures are re-probed in `tests/system_info_oracle.rs`.
`mod time` is `pub(crate)` because `crate::timers` shares the two constants: its `ERTS_MONOTONIC_START_MSEC`/`ERTS_MONOTONIC_END_MSEC` are `START_TIME`/`END_TIME` divided by `1_000_000`, the inclusive window (`-576460752000` … `8646911283000` ms) an `{abs, true}` timer time must fall in.

### `trace_control_word`
This runtime's trace-control word, which is always `0`.
The word is an independent node-global knob whose only writers are `erlang:system_flag(trace_control_word, N)` / `erts_internal:set_tcw` (`erl_db_util.c:1080-1110`), neither of which this runtime implements (`src/seq_trace.rs::system_flag_2` serves only `reset_seq_trace` and `system_logger`, and is a typed gap for every other flag).
Process tracing does **not** change it: the oracle returns `0` both before and after `erlang:trace(self(),true,[call])` + `trace_pattern/3`, so `0` is this runtime's truthful live value and must **not** be derived from the process-trace state (that would report a wrong value).
Making it live for a future `system_flag(trace_control_word, _)` implementation would need a `u32` field on `TraceState` (e.g. `TraceState::trace_control_word(&self) -> u32` + a setter), read through `Runtime::trace()`.

### `system_logger` (`info/system.rs`)
**The value of this runtime's `system_logger` flag — the same slot `erlang:system_flag(system_logger, _)` exchanges.**
The C is `BIF_RET(erts_get_system_logger())` (`erl_bif_info.c:3560-3561`): it holds the atom `logger` initially (`erl_trace.c:339`), the pid `logger_proxy` stores with `erlang:system_flag(system_logger, self())` at its start-up (`kernel/src/logger_proxy.erl:117`), and the atom `undefined` after `erlang:system_flag(system_logger, undefined)` (`:147`).
The flag lives on `SeqTrace` as `SystemLogger` (`Logger` — the default — / `Undefined` / `Pid(ProcessId)`), exchanged by `src/seq_trace.rs::system_flag_2`'s `system_logger` arm (which accepts exactly the C's three values and answers the **previous** one; anything else is `badarg`, `erts_set_system_logger`, `erl_trace.c:2485-2491`) and read here through `Runtime::seq_trace()`, so the two surfaces cannot drift.
The value is the **only** pid-valued key, so `InfoValue` gained the `Pid(ProcessId)` variant; a pid is an immediate, so `to_eterm` needs neither heap space nor an atom (`InfoValue::Pid(pid)` ⇒ `pid.to_eterm()`, byte-identical to `erlang:whereis/1`).
`tests/system_info_oracle.rs` classifies it in its own list (`ORACLE_OWN_PID_KEYS`): the oracle's own answer is probed as a **pid** (its `logger_proxy`), this runtime's as a **pid, the atom `logger` or `undefined`** — the three shapes the C's flag can hold.

### `UNSUPPORTED_KEYS` (all 27 are oracle-**accepted**) — oracle answer + verdict
Per-key rationale lives in `info/unsupported.rs`; the oracle answer (OTP 29.1.1 / erts-17.1) and the close/keep verdict per key:
- `build_type` → atom `opt`. Our honest value would be our own build tier (`debug` in a dev build, `opt` in a release build, derived from `cfg!(debug_assertions)`), which **disagrees** with the oracle's `opt` by construction — so it is a **typed gap**, never a hardcoded `opt` (a stable-but-disagreeing own-state value must not be traded for the oracle's figure). It shares the C arm with `emu_type` (`erl_bif_info.c:2800`, `am_build_type || am_emu_type`); `emu_type` remains **served shape-only** — a consistent family decision is flagged for the next pass (if `emu_type` is later reclassified too, it must move here in lockstep).
- `allocator` → `{glibc,[2,44],[…12 allocator names…],[{sys_alloc,…},…]}` — host C-allocator identification; this runtime allocates through `beam_alloc`/`libc` and identifies no glibc. **KEEP.**
- `time_offset` → atom `volatile`; `time_correction` → `true`; `time_warp_mode` → `multi_time_warp`; `tolerant_timeofday` → `disabled` — the live state/flags of the time-correction service, which this runtime does not have. **KEEP.** (The two monotonic-time *interface constants* `start_time`/`end_time` used to sit in this group behind a "no time server" rationale that was wrong — they are reproducible build constants and are now **served**; see the section above.)
- `os_monotonic_time_source` / `os_system_time_source` → a 7-/5-item proplist whose last element is the **live** `{time,N}` reading (plus `clock_gettime`/`CLOCK_MONOTONIC`/`resolution`/`extended`/`parallel`). The `time` field is per-call and `extended`/`parallel` are ERTS-internal. **KEEP.**
- `cpu_topology` → `[{processor,[{core,{logical,0}},…]}]` — core/thread topology detection this runtime has none of (`available_parallelism` only). **KEEP.**
- `check_io` → a list of poll-set introspection entries (`{name,erts_poll}`, `primary`, `kernel_poll`, `memory_size`, `total_poll_set_size`, `lazy_updates`, `pending_updates`, `fallback`, `max_fds`, `active_fds`, `poll_threads`, …). `beam_io::Poller` (the runtime's live poller) exposes only `len`/`is_empty`/`registration` — the ERTS field vocabulary is absent and would need a new `beam_io` API. **KEEP.**
- `c_compiler_used` → `{gnuc,{16,2,0}}` — not built by a C compiler. **KEEP.**
- `embedded_3pps` → `#{included=>[asmjit,pcre2,ryu,'STL',zstd],excluded=>[zlib]}` — this runtime embeds none of ERTS's third-party C libraries; its Rust dependencies (`mio`, `libc`, optionally Cranelift) are a different set. **KEEP.**
- `system_logger` → a **pid** in the oracle (its `logger_proxy`). **SERVED**, as the value of *this* runtime's own `system_logger` flag — the atom `logger` (the C's initial value) until something runs `erlang:system_flag(system_logger, _)`, then a pid or `undefined`. See the `system_logger` section above.
- `info` / `procs` / `loaded` → **binaries** (547918 / 69206 / 1906 bytes on this host): ERTS-internal text dumps (`info()` = memory+hash-table+`allocated_areas`+allocator dump; `process_info()` = per-process dump; `loaded()` = the `Current code: N\nOld code: N\n<module> <bytes>` code-ix dump — `erl_bif_info.c:2951-2980`). The memory/table *figures* are reachable (`erlang:memory/0`, `erlang:processes/0`, the code server's loaded set), but the byte-level dump format is a different emulator's. **KEEP.**

### `badarg` — keys the oracle **rejects** (pinned in tests)
`allocator_sizes`, `beam_jit_available`, `busy_limits`, `c_library_version`, `catchlevel`, `export_count`, `export_limit`, `instruction_counts`, `memory`, `module_count`, `module_limit`, `net_ticktime`, `node`, `pure`, `run_queue`, `safe_string_hash`, `total_run_queue_lengths`, `wall_clock` — plus any unknown name (`nonsense`, `Version`).
(`instruction_counts` is pinned only in the integration suite's `ORACLE_REJECTED_KEYS`; the `src/info.rs` unit list is the same set minus it.)

### Per-key decisions (A.8 `system_info`)
Explicit close/keep decisions, so the next pass does not re-litigate them; each is a key where "answer = the oracle's figure" was considered and rejected:
- `endian` — oracle `little`; **served** as `cfg!(target_endian)`, which equals the oracle's `little` on this host. The value is pinned (not shape-only) in `tests/system_info_oracle.rs`.
- `build_type` — oracle `opt`; **typed gap** (our own build tier is `debug` in a dev build and disagrees with `opt` by construction; never hardcode `opt` — see the `### UNSUPPORTED_KEYS` bullet above).
- `heap_sizes` — oracle 137 entries ending `885767171276247`; **served shape-only** (the value is `beam_gc::heap_sizes`, this runtime's own growth table). The one-extra-trailing-entry divergence is a `beam_gc` bug — see Known Issues.
- `loaded` — oracle a binary (the code-ix dump); **typed gap**. This runtime's loaded-module set differs by construction, and a truthful dump would need the code server's current/old set in the C's `Current code: N\nOld code: N\n<module> <bytes>` format (`erl_bif_info.c:2951-2980`).
- `check_io` — oracle a list of poll-set introspection entries; **typed gap**. `beam_io::Poller` exposes only `len`/`is_empty`/`registration`; closing it needs a new `beam_io` API exposing the ERTS poll-set vocabulary (`name`/`primary`/`kernel_poll`/`memory_size`/`total_poll_set_size`/`lazy_updates`/`pending_updates`/`fallback`/`max_fds`/`active_fds`/`poll_threads`).

## `InfoValue` variants and materialization
The scalar variants are `Int`, `Str`, `Atom`, `Bool`, `Tuple` (a two-atom tuple), `Binary(Vec<u8>)` and `Pid(ProcessId)`.
`Pid` is the only process-valued variant (the `system_logger` key): a `ProcessId` encodes to its internal-pid immediate with no heap and no atom (`ProcessId::to_eterm`), so the materialized term is byte-identical to `erlang:whereis/1`'s answer for the same process.
`Binary` carries raw bytes and is materialized through the safe on-heap binary writer `beam_codec::term::make_binary` — the crate is `#![deny(unsafe_code)]`, so no heap word is written by hand.
The container variants are `List`, `Tupled` and `Map` (`Map` goes through `beam_codec::term::make_flatmap`).
`Int` is a 64-bit signed integer materialized as a small-int immediate when it fits the 60-bit small range and as a one-digit bignum otherwise — the C's `make_time_val` choice (`erl_time_sup.c:2133-2146`), written through `beam_codec::term::make_big`.
`Binary` is distinct from `Str`: `Str` is an Erlang **string** (a list of byte-sized small ints, which is what the oracle returns for `version`/`nif_version`/…), whereas `Binary` is a byte string, the shape of `dist`.

## Atom discipline
`InfoValue::to_eterm` resolves an `InfoValue::Atom` through the runtime's own atom table and returns `BeamError::TermNotFound` for a name it lacks — never an invented atom.
So `INFO_ATOMS` is the **complete** atom vocabulary of the surface (including the `process_info/2` items), kept as `pub const INFO_ATOMS: &[&str]` because `crate::boot::seed_core_atoms` interns it.
It is **append-only**: existing indices stay stable.
A test seeds exactly `INFO_ATOMS` and materializes every `SUPPORTED_KEYS` entry.
No new atom names were needed for `dist`/`dist_ctrl`/`Binary` (a binary carries no atom).

## Constraints
- Read-only with respect to the rest of the crate: this node owns `src/info.rs`, `src/info/`, and `tests/system_info_oracle.rs` only.
- No `unsafe`: `InfoValue::Map` is written through `beam_codec::term::make_flatmap`, `InfoValue::Binary` through `beam_codec::term::make_binary`, and an out-of-small-range `InfoValue::Int` through `beam_codec::term::make_big`.
- No invented values: a facility this runtime does not have ⇒ the `unsupported` gap.
- Every key's value is derived from this runtime's own configuration/tables or from a genuine build/host constant. `Runtime` accessors used: `config()` (emulator features / schedulers / dirty schedulers / `max_processes` / `max_ports`), `heap.policy()`/`heap.max_heap_flags()`, `code_server.atoms()`, `processes()`, `loaded_modules()`, `ports`, `tables`, `dist_node()`. **No new `Runtime` getter was needed.**

## Known Issues
- `os_type` / `system_architecture` live in `info/host.rs` and derive from `std::env::consts::OS` / `ARCH` (`os_type_for` / `architecture_for`): `linux`, `macos`(`darwin`) and the BSDs map to the oracle's `{unix,<name>}`, `windows` to `{win32,nt}`, and any other target falls back to the OS constant itself — a best-effort name, not the oracle's build-time `os_type` string. `system_architecture` falls back to the vendor-less `"{arch}-unknown-{os}"` (the vendor is never invented). `info/system.rs`'s `os_type` unit test still asserts the Linux `{unix,linux}` literally (correct here, host-specific); the host-independent mapping is covered by `info/host.rs`'s tests.
- The `{allocator, A}` rows are a live `beam_alloc` snapshot (`beam_alloc::registry::all_stats`) and the counts are real table lengths, but no test drives an allocation and observes a counter rise — "measured" here rests on the `beam_alloc` registry, not on a regression test in this node.
- **The three key sets now cover every oracle-accepted key** (88 supported + 27 unsupported = the oracle's 115 accepted names), so an unclassified oracle-accepted key is the only way this runtime could raise `badarg` where the oracle answers a value; `tests/system_info_oracle.rs` fails if one appears. The **C-emulator internals** that stay in `UNSUPPORTED_KEYS` (each a typed exit-3 gap rather than a copied oracle figure, per-key rationale in `info/unsupported.rs`) are: `compile_info` (no C toolchain), `ethread_info` / `ethread_used_tse` (no C ethread library), `constant_pool_support` / `beam_jump_table` (no ERTS constant-pool code loading), `snifs` / `taints` (no static NIF table / taint tracking), `emu_args` (no emulator argument vector), `reader_groups_map` / `decentralized_counter_groups_map` (no reader groups / decentralized counters), `max_integer` (no `BIG_ARITY_MAX` cap), `thread_progress` (no thread-progress debug state). `literal_test` is **served** as `tag` (this runtime's frozen layout detects literals with a tag bit — the C's own word for a tag-based test) where the oracle's 64-bit build answers `range`; it is compared **shape-only**.
- **Several `info/system.rs` literals are ERTS *runtime* flags this runtime reports at the oracle's default** (they would diverge if the flag were passed, because the CLI models none of them): `port_parallelism` (C `erts_port_parallelism`, `+spp`), `break_ignored` (C `ignore_break`, `erl_bif_info.c:3185`), `modified_timing_level` (C `erts_modified_timing_level`, `+T`), `halt_flush_timeout` (C `erts_halt_flush_timeout`, `-shutdown_time`, `infinity` = the negative default). Two `info/limits.rs` constants are flag-settable too and were confirmed against the oracle: `dist_buf_busy_limit` (`+zdbbl` — `+zdbbl 2048` ⇒ `2097152`) and `delayed_node_table_gc` (`+zdntgc` — `+zdntgc 120` ⇒ `120`); their default literals diverge under those flags. The CLI has no arm for any of them, and `record_unmodeled` **drops** an unmodeled `+flag` (it becomes neither an init arg nor an `extra_emulator_flags` entry), so there is no state to derive from today; making them live needs an A.6 change (preserve the flag) plus a `thread_pool_size`-style derivation here. By contrast these literals are genuine build constants and must stay as-is: `threads` (`true`), `async_dist` (`false`), `dynamic_trace` (`none`), `dynamic_trace_probes` (`false`), `kernel_poll` (`cfg!(unix)`, matching the C's `ERTS_ENABLE_KERNEL_POLL` build constant which `+K` does **not** affect), `update_cpu_info` (`unchanged` — the steady-state answer of the oracle's stateful `erts_update_cpu_info()` probe), `trace_control_word` (`0`), and `outstanding_system_requests_limit` (fixed at `32` — `+sss 64` does not move the oracle's value).
- `heap_sizes` currently reports one extra trailing entry vs the oracle (138 vs 137): the C's `erl_gc.c:283-291` loop decrements `i` after detecting the over-cap step (`i--; break;`), so the C's `num_heap_sizes` **also drops the last under-cap entry** (`num = first_offending_index - 1`), whereas `beam_gc::build_heap_sizes` (`erts_rust/crates/beam_gc/src/policy.rs:73`, `break`-before-`push`) keeps it; the faithful table (and the oracle's) is 137 entries ending `885767171276247`. The fix belongs to `beam_gc` (out of this node's scope) and has been reported. `heap_sizes` stays **served** and compared **shape-only**.

## Test Inventory (in-file `#[cfg(test)]`, current counts)
Every `*.rs` in this node (the parent `src/info.rs` + all 8 child modules) carries an in-file `#[cfg(test)]` module:

| file | `#[test]` |
|---|---|
| `src/info.rs` (parent) | 15 |
| `info/schedulers.rs` | 10 |
| `info/system.rs` | 9 |
| `info/memory.rs` | 8 |
| `info/host.rs` | 6 |
| `info/limits.rs` | 4 |
| `info/dist.rs` | 4 |
| `info/time.rs` | 3 |
| `info/unsupported.rs` | 1 |
| **total (this node)** | **60** |

The A.8 oracle gate is the sibling integration suite `tests/system_info_oracle.rs` (**7** `#[test]`s, skips loudly without a system `erl`); the `process_info/2` / `module_info/1` sibling surfaces have their own suites (`tests/process_info_items.rs`, `tests/module_info.rs`).

## Test Strategy
- Per-key unit tests colocated in each family module: the value **shape** (and the exact value for the constants), plus `try_value(other_key) == None` and the unknown-key `badarg`.
  `info/limits.rs` additionally pins the whole `+Q`/`ERL_MAX_PORTS` sizing matrix (`1024→1024 … 134217727→134217727`, `legacy`/none→65536) and the default table.
  `info/schedulers.rs` pins `thread_pool_size` against the `+A` flag (`+A 4`→4, absent→1, `+A 0`→1, a malformed preserved flag→the default); `info/host.rs` pins the OS/triple mapping (`os_type_for`/`architecture_for` incl. the macOS/BSD/Windows cases and the fallback); `info/memory.rs` pins the `allocated_areas` derivations (the real items rise with a loaded module; the counterpart-less items stay `0`).
  `info/dist.rs` pins the non-distributed answers (the 39-byte binary, `[]`), that `other_keys` are not its own, and that a configured node turns `dist` into the typed capability gap while `dist_ctrl` stays `[]` until a link exists (skipping loudly if the listener cannot bind).
  `info/time.rs` pins the two interface constants against their recorded oracle values, their C derivation, that `other_keys` are not its own, and that `start_time` materializes as a small while `end_time` materializes as a positive one-digit bignum.
- `src/info.rs` tests: `SUPPORTED_KEYS.len() == 87`, `UNSUPPORTED_KEYS.len() == 28`, the two sets duplicate-free and disjoint, every supported key answers and **materializes** once `INFO_ATOMS` is seeded, every unsupported key classifies as `unsupported`, the oracle-rejected list stays `BadArg`, each `InfoValue` variant materializes (incl. the map and the binary), and the `{allocator, A}` report carries the measured counters.
- `tests/system_info_oracle.rs` (the gate): one `erl` invocation answers every probed key; the test then asserts (a) the oracle accepts every `UNSUPPORTED_KEYS` key and rejects every `badarg` key, (b) **value + shape** equality for the interface/host constants (incl. `dist`/`dist_ctrl` on a default, non-distributed runtime, and the monotonic-time constants), (c) **shape** equality (never the number) for the self-derived keys and this runtime's own version strings, and (d) that the key sets here partition `SUPPORTED_KEYS` exactly (a key added without classification fails).
  Two further tests cross-check the **oracle's own printed value** for the whole `+Q`/`ERL_MAX_PORTS` `port_limit` matrix (`+Q N` on the command line and `ERL_MAX_PORTS` through `parse_with_env`) and for the `{wordsize, internal|external}` tuple key (including the oracle's `badarg` for `{wordsize,foo}`).
  `the_monotonic_time_interface_constants_match_the_oracle` probes the oracle **twice** for `start_time`/`end_time`, requires both runs (and the **recorded** figures) to agree, and pins that the oracle's `start_time` is a small integer while its `end_time` is a bignum — the recorded table cannot go stale silently.
  It **skips loudly** when no system `erl` is executable.

## Legacy C ground truth (what this node ports)
This node reimplements the `system_info/1` arm of the legacy C emulator (read-only reference, never modified):
- `erts/emulator/beam/erl_bif_info.c` — `system_info_1` (`:2774`) is the C entry the whole surface mirrors; the `start_time`/`end_time` arms dispatch at `:2857-2859`, the tuple keys `{wordsize,W}`/`{allocator,A}` at `:2521-2546`, the scheduler-state/statistics keys at `:3258-3352`, the `build_type`/`emu_type` shared arm at `:2800`.
- `erts/emulator/beam/erl_time_sup.c` (`make_time_val` `:2133-2146`, `erts_get_monotonic_{start,end}_time` `:2148-2158`) + `erts/emulator/beam/erl_time.h` — the `start_time`/`end_time` interface constants (`info/time.rs`).
- `erts/emulator/beam/erl_alloc.c` (`erts_allocated_areas` `:2580-2699`, `erts_alloc_util_allocators` `:2789`) — `alloc_util_allocators` / `allocated_areas` / `{allocator,A}` (`info/memory.rs`, `allocator_report`).
- `erts/emulator/beam/erl_gc.c` (`erts_init_gc` `:262-291`) — `heap_sizes` / `garbage_collection` (values produced by `beam_gc`).
- `erts/emulator/beam/erl_ptab.c` / `erl_ptab.h` + `erl_port.h` — the `port_limit` table-sizing rule (`info/limits.rs`).
Each source module cites its own C line numbers inline; the mapping is also recorded in this file's See Also.

## See Also
- `../CONTEXT.md` → the crate façade (`system_info_value`/`InfoValue` re-exports, the `{allocator, A}` route in `src/native.rs`, the A.8 contract).
- `../../CONTEXT.md` §1 A.8 → the introspection contract.
- `../../../../erts/emulator/beam/erl_bif_info.c` → the C ground truth for the key set.
