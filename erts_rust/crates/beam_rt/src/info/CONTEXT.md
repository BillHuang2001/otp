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
| `info/system.rs` | build/host identity: `version`, `otp_release`, `system_architecture`, `system_version`, `nif_version`, `driver_version`, `wordsize`, `emu_flavor`, `emu_type`, `machine`, `threads`, `smp_support`, `async_dist`, `os_type`, `os_version`, `break_ignored`, `debug_compiled`, `dynamic_trace`, `dynamic_trace_probes`, `modified_timing_level`, `update_cpu_info`, `trace_control_word`, `port_parallelism`, `kernel_poll`, `halt_flush_timeout`, `creation` |
| `info/limits.rs` | `atom_count`, `atom_limit`, `process_count`, `process_limit`, `port_count`, `port_limit`, `ets_count`, `ets_limit`, `outstanding_system_requests_limit`, `dist_buf_busy_limit`, `delayed_node_table_gc` |
| `info/schedulers.rs` | `logical_processors` / `_available` / `_online`, `schedulers`, `schedulers_online`, `scheduler_id`, `dirty_cpu_schedulers` / `_online`, `dirty_io_schedulers`, `scheduler_bind_type`, `scheduler_bindings`, `multi_scheduling`, `multi_scheduling_blockers`, `normal_multi_scheduling_blockers`, `thread_pool_size`, `context_reductions` |
| `info/memory.rs` | `min_heap_size`, `min_bin_vheap_size`, `max_heap_size`, `fullsweep_after`, `heap_sizes`, `heap_type`, `message_queue_data`, `garbage_collection`, `alloc_util_allocators`, `allocated_areas` |
| `info/dist.rs` | `dist`, `dist_ctrl` — the distribution-state keys |
| `info/unsupported.rs` | the 17 oracle-accepted keys this runtime honestly refuses |

Each family exposes `pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>>`; `None` means "not my key".
`system_info_value` chains the families (`system → limits → schedulers → memory → dist → unsupported`) and falls through to `error::unknown_info_key` (the oracle's `badarg`).

## The three disjoint key sets (the honesty rule)
| set | count | meaning |
|---|---|---|
| `SUPPORTED_KEYS` | 65 | answered with the oracle's exact value **shape** (`{allocator, A}` and `{wordsize, W}` are the two **tuple** keys, dispatched by `tuple_key_value`) |
| `UNSUPPORTED_KEYS` | 17 | the oracle accepts the key, but it describes a facility this runtime does not have → a **capability gap** raise whose reason atom starts with `unsupported` (exit 3 for a `-eval` caller), **never** a fabricated value or a hardcoded oracle number |
| everything else | — | `BeamError::BadArg`, exactly the oracle's reason for a key it does not accept |

### `SUPPORTED_KEYS`, by source of the value
**Genuine build/host constants** (oracle-equal on this host): `version` (our `beam_common::ERTS_VERSION`), `otp_release` (our OTP major), `system_architecture`, `nif_version` (`2.18`), `driver_version` (`3.3`), `wordsize` (8), `emu_flavor`, `emu_type`, `machine` (`"BEAM"`), `threads`, `smp_support`, `async_dist`, `os_type`, `os_version`, `break_ignored`, `debug_compiled`, `dynamic_trace`, `dynamic_trace_probes`, `modified_timing_level`, `update_cpu_info`, `trace_control_word` (this runtime's trace-control word, which is `0` — see below), `port_parallelism`, `kernel_poll`, `halt_flush_timeout`, `creation`, `atom_limit` (1048576), `process_limit` (1048576 or `+P N`), `ets_limit` (8192), `outstanding_system_requests_limit` (32), `dist_buf_busy_limit` (1048576), `delayed_node_table_gc` (60), `context_reductions` (`CONTEXT_REDS`, `erl_vm.h:47` — the ERTS reduction quantum, an interface constant), `heap_type`, `message_queue_data`, `scheduler_bind_type`, `min_heap_size`, `min_bin_vheap_size`, `fullsweep_after`, `garbage_collection`, `alloc_util_allocators`, `multi_scheduling_blockers`, `normal_multi_scheduling_blockers`, and the `{wordsize, internal|external}` tuple key (`size_of::<Eterm>()` / `size_of::<usize>()`, `erl_bif_info.c:2521-2527`).

**Derived from this runtime's own configuration** — a real emulator rule reproduced from the build's constant tables rather than a literal: `port_limit` (`min(next_power_of_two(request), 134217727)`, or the default table `65536`; the request is `+Q N` or the clamped `ERL_MAX_PORTS` — `erts_ptab_init_table`/`erts_ptab_max`, `erl_ptab.c:383-388`/`erl_ptab.h:266-270`), `{allocator, A}` (`allocator_report`), `thread_pool_size` (the async-thread-pool size: the `+A N` the CLI preserves in `emulator.extra_emulator_flags`, reported as `max(1, N)` — matching the oracle's `+A 4`→`4` / `+A 0`→`1`; the oracle's own default `1` when `+A` is absent).

**Answered exactly in the oracle's non-distributed state** (`info/dist.rs`): `dist` is the **binary** `<<"=node:'nonode@nohost'\n=no_distribution\n">>` (39 bytes, each `\n` a real `0x0a`) when this runtime is not a node (`Runtime::dist_node() == None`), and `dist_ctrl` is the empty list `[]` when this runtime is not a node **or** its `DistNode::connections()` is empty.
`dist` falls back to the typed capability gap as soon as this runtime *is* a node (“no live distribution state”); `dist_ctrl` once that node carries a live link (“no distribution controllers”): the oracle's `dist` is a full wire dump (`=node:…`/`=proc:…` sections) and `dist_ctrl` one entry per controller, both needing the `net_kernel` protocol this runtime does not implement.
`RuntimeConfig::default()` is `nonode@nohost`, so the differential test's runtime observes the `[]` / 39-byte-binary answers.

**Self-derived from *this* runtime's own state** (correct-but-smaller — or simply different — from the oracle's run-time figure; the same rule `erlang:processes/0` / `ports/0` already follows; these must **never** be "fixed" to a hardcoded oracle number): `atom_count` (atom-table length), `process_count` (process-table length), `port_count` (live `PortRegistry` length), `ets_count` (live table-registry length), `allocated_areas` (one `size_of::<usize>()` slot per entry of this runtime's own live tables — `export_table` / `fun_table` from the loaded modules' `ExpT` exports / `FunT` lambda descriptors, `module_refs` / `module_table` / `loaded_code` from the loaded-module set, `dist_table` / `node_table` from `Runtime::dist_node()`; an item with no counterpart is an honest `0`), `heap_sizes` (`beam_gc::heap_sizes`), `logical_processors` / `_available` / `_online` (`available_parallelism`), `schedulers` / `schedulers_online` (`+S N` or the processor count), `scheduler_id` (1), `dirty_cpu_schedulers` / `_online` and `dirty_io_schedulers` (`+SDcpu`/`+SDio`, else 0), `scheduler_bindings` (`unbound` per scheduler), `multi_scheduling` (derived from the scheduler count), `max_heap_size` (the emulator's heap options), and **`system_version`** — this runtime's own one-line self-description, deliberately its own string with the oracle's bracket-group shape (`Erlang/OTP <major> [erts-<vsn>] [source] [<bits>-bit]`).
The oracle appends further groups derived from emulator state this runtime does **not** model (the `smp:`/`ds:` scheduler and dirty-scheduler counts, the async-thread pool size, the JIT flavor); they are **omitted**, never copied or guessed — so `system_version` is compared **shape-only**.

The two **tuple** keys are answered by `info.rs::tuple_key_value(tag, arg)` (dispatched by `src/native.rs`): `{allocator, A}` → the flat, real `beam_alloc` rows (the oracle wraps them in one `{instance, N, …}` entry per allocator instance, which this runtime does not model — a documented divergence) and `{wordsize, internal|external}` → the host word size.
Any other tuple tag/argument is `badarg`, exactly as in the oracle (`{wordsize,foo}`, `{nonsense,x}`).

### `trace_control_word`
This runtime's trace-control word, which is always `0`.
The word is an independent node-global knob whose only writers are `erlang:system_flag(trace_control_word, N)` / `erts_internal:set_tcw` (`erl_db_util.c:1080-1110`), neither of which this runtime implements (`src/seq_trace.rs::system_flag_2` serves only `reset_seq_trace` and is a typed gap for every other flag).
Process tracing does **not** change it: the oracle returns `0` both before and after `erlang:trace(self(),true,[call])` + `trace_pattern/3`, so `0` is this runtime's truthful live value and must **not** be derived from the process-trace state (that would report a wrong value).
Making it live for a future `system_flag(trace_control_word, _)` implementation would need a `u32` field on `TraceState` (e.g. `TraceState::trace_control_word(&self) -> u32` + a setter), read through `Runtime::trace()`.

### `UNSUPPORTED_KEYS` (all 17 are oracle-**accepted**) — oracle answer + verdict
Per-key rationale lives in `info/unsupported.rs`; the oracle answer (OTP 29.1.1 / erts-17.1) and the close/keep verdict per key:
- `allocator` → `{glibc,[2,44],[…12 allocator names…],[{sys_alloc,…},…]}` — host C-allocator identification; this runtime allocates through `beam_alloc`/`libc` and identifies no glibc. **KEEP.**
- `time_offset` → atom `volatile`; `time_correction` → `true`; `time_warp_mode` → `multi_time_warp`; `tolerant_timeofday` → `disabled` — the live state/flags of the time-correction service, which this runtime does not have. **KEEP.**
- `start_time` → the integer `-576460752000000000`; `end_time` → `8646911283000000000` — **both are compile-time constants of the erts-17.1 build, identical across runs, NOT per-run/time-server figures.** `erts_get_monotonic_{start,end}_time` return `make_time_val(ERTS_MONOTONIC_TIME_{START,END}_EXTERNAL)` (`erl_time_sup.c:2148-2158`): `START_EXTERNAL = ((MIN_SMALL/UNIT)*UNIT) = -(576460752·10⁹)` and `END_EXTERNAL = START_EXTERNAL + (END-BEGIN)` (`erl_time.h:295-321`/`403-407`). They are reproducible, so answering them is not the lie the old "no time server" rationale assumed — the only blocker is that the constants encode the external monotonic-time unit/range this runtime does not otherwise model. **CLOSE-recommended** (as `nif_version`-style interface constants) if the runtime chooses to reproduce the erts-17.1 monotonic-time encoding.
- `os_monotonic_time_source` / `os_system_time_source` → a 7-/5-item proplist whose last element is the **live** `{time,N}` reading (plus `clock_gettime`/`CLOCK_MONOTONIC`/`resolution`/`extended`/`parallel`). The `time` field is per-call and `extended`/`parallel` are ERTS-internal. **KEEP.**
- `cpu_topology` → `[{processor,[{core,{logical,0}},…]}]` — core/thread topology detection this runtime has none of (`available_parallelism` only). **KEEP.**
- `check_io` → a list of poll-set introspection entries (`{name,erts_poll}`, `primary`, `kernel_poll`, `memory_size`, `total_poll_set_size`, `lazy_updates`, `pending_updates`, `fallback`, `max_fds`, `active_fds`, `poll_threads`, …). `beam_io::Poller` (the runtime's live poller) exposes only `len`/`is_empty`/`registration` — the ERTS field vocabulary is absent and would need a new `beam_io` API. **KEEP.**
- `c_compiler_used` → `{gnuc,{16,2,0}}` — not built by a C compiler. **KEEP.**
- `embedded_3pps` → `#{included=>[asmjit,pcre2,ryu,'STL',zstd],excluded=>[zlib]}` — this runtime embeds none of ERTS's third-party C libraries; its Rust dependencies (`mio`, `libc`, optionally Cranelift) are a different set. **KEEP.**
- `system_logger` → a **pid** (the oracle's logger process); this runtime runs no `logger`. **KEEP.**
- `info` / `procs` / `loaded` → **binaries** (547918 / 69206 / 1906 bytes on this host): ERTS-internal text dumps (`info()` = memory+hash-table+`allocated_areas`+allocator dump; `process_info()` = per-process dump; `loaded()` = the `Current code: N\nOld code: N\n<module> <bytes>` code-ix dump — `erl_bif_info.c:2951-2980`). The memory/table *figures* are reachable (`erlang:memory/0`, `erlang:processes/0`, the code server's loaded set), but the byte-level dump format is a different emulator's. **KEEP.**

### `badarg` — keys the oracle **rejects** (pinned in tests)
`allocator_sizes`, `beam_jit_available`, `busy_limits`, `c_library_version`, `catchlevel`, `export_count`, `export_limit`, `memory`, `module_count`, `module_limit`, `net_ticktime`, `node`, `pure`, `run_queue`, `safe_string_hash`, `total_run_queue_lengths`, `wall_clock` — plus any unknown name (`nonsense`, `Version`).

## `InfoValue` variants and materialization
The scalar variants are `Int`, `Str`, `Atom`, `Bool`, `Tuple` (a two-atom tuple) and `Binary(Vec<u8>)`.
`Binary` carries raw bytes and is materialized through the safe on-heap binary writer `beam_codec::term::make_binary` — the crate is `#![deny(unsafe_code)]`, so no heap word is written by hand.
The container variants are `List`, `Tupled` and `Map` (`Map` goes through `beam_codec::term::make_flatmap`).
`Binary` is distinct from `Str`: `Str` is an Erlang **string** (a list of byte-sized small ints, which is what the oracle returns for `version`/`nif_version`/…), whereas `Binary` is a byte string, the shape of `dist`.

## Atom discipline
`InfoValue::to_eterm` resolves an `InfoValue::Atom` through the runtime's own atom table and returns `BeamError::TermNotFound` for a name it lacks — never an invented atom.
So `INFO_ATOMS` is the **complete** atom vocabulary of the surface (including the `process_info/2` items), kept as `pub const INFO_ATOMS: &[&str]` because `crate::boot::seed_core_atoms` interns it.
It is **append-only**: existing indices stay stable.
A test seeds exactly `INFO_ATOMS` and materializes every `SUPPORTED_KEYS` entry.
No new atom names were needed for `dist`/`dist_ctrl`/`Binary` (a binary carries no atom).

## Constraints
- Read-only with respect to the rest of the crate: this node owns `src/info.rs`, `src/info/`, and `tests/system_info_oracle.rs` only.
- No `unsafe`: `InfoValue::Map` is written through `beam_codec::term::make_flatmap` and `InfoValue::Binary` through `beam_codec::term::make_binary`.
- No invented values: a facility this runtime does not have ⇒ the `unsupported` gap.
- Every key's value is derived from this runtime's own configuration/tables or from a genuine build/host constant. `Runtime` accessors used: `config()` (emulator features / schedulers / dirty schedulers / `max_processes` / `max_ports`), `heap.policy()`/`heap.max_heap_flags()`, `code_server.atoms()`, `processes()`, `loaded_modules()`, `ports`, `tables`, `dist_node()`. **No new `Runtime` getter was needed.**

## Known Issues
- `os_type` / `system_architecture` live in `info/host.rs` and derive from `std::env::consts::OS` / `ARCH` (`os_type_for` / `architecture_for`): `linux`, `macos`(`darwin`) and the BSDs map to the oracle's `{unix,<name>}`, `windows` to `{win32,nt}`, and any other target falls back to the OS constant itself — a best-effort name, not the oracle's build-time `os_type` string. `system_architecture` falls back to the vendor-less `"{arch}-unknown-{os}"` (the vendor is never invented). `info/system.rs`'s `os_type` unit test still asserts the Linux `{unix,linux}` literally (correct here, host-specific); the host-independent mapping is covered by `info/host.rs`'s tests.
- The `{allocator, A}` rows are a live `beam_alloc` snapshot (`beam_alloc::registry::all_stats`) and the counts are real table lengths, but no test drives an allocation and observes a counter rise — "measured" here rests on the `beam_alloc` registry, not on a regression test in this node.

## Test Strategy
- Per-key unit tests colocated in each family module: the value **shape** (and the exact value for the constants), plus `try_value(other_key) == None` and the unknown-key `badarg`.
  `info/limits.rs` additionally pins the whole `+Q`/`ERL_MAX_PORTS` sizing matrix (`1024→1024 … 134217727→134217727`, `legacy`/none→65536) and the default table.
  `info/schedulers.rs` pins `thread_pool_size` against the `+A` flag (`+A 4`→4, absent→1, `+A 0`→1, a malformed preserved flag→the default); `info/host.rs` pins the OS/triple mapping (`os_type_for`/`architecture_for` incl. the macOS/BSD/Windows cases and the fallback); `info/memory.rs` pins the `allocated_areas` derivations (the real items rise with a loaded module; the counterpart-less items stay `0`).
  `info/dist.rs` pins the non-distributed answers (the 39-byte binary, `[]`), that `other_keys` are not its own, and that a configured node turns `dist` into the typed capability gap while `dist_ctrl` stays `[]` until a link exists (skipping loudly if the listener cannot bind).
- `src/info.rs` tests: `SUPPORTED_KEYS.len() == 65`, `UNSUPPORTED_KEYS.len() == 17`, the two sets duplicate-free and disjoint, every supported key answers and **materializes** once `INFO_ATOMS` is seeded, every unsupported key classifies as `unsupported`, the oracle-rejected list stays `BadArg`, each `InfoValue` variant materializes (incl. the map and the binary), and the `{allocator, A}` report carries the measured counters.
- `tests/system_info_oracle.rs` (the gate): one `erl` invocation answers every probed key; the test then asserts (a) the oracle accepts every `UNSUPPORTED_KEYS` key and rejects every `badarg` key, (b) **value + shape** equality for the interface/host constants (incl. `dist`/`dist_ctrl` on a default, non-distributed runtime), (c) **shape** equality (never the number) for the self-derived keys and this runtime's own version strings, and (d) that the key sets here partition `SUPPORTED_KEYS` exactly (a key added without classification fails).
  Two further tests cross-check the **oracle's own printed value** for the whole `+Q`/`ERL_MAX_PORTS` `port_limit` matrix (`+Q N` on the command line and `ERL_MAX_PORTS` through `parse_with_env`) and for the `{wordsize, internal|external}` tuple key (including the oracle's `badarg` for `{wordsize,foo}`).
  It **skips loudly** when no system `erl` is executable.

## See Also
- `../CONTEXT.md` → the crate façade (`system_info_value`/`InfoValue` re-exports, the `{allocator, A}` route in `src/native.rs`, the A.8 contract).
- `../../CONTEXT.md` §1 A.8 → the introspection contract.
- `../../../../erts/emulator/beam/erl_bif_info.c` → the C ground truth for the key set.
