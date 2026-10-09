# `beam_bifs::runtime` — the runtime-coupled BIF families (`src/runtime/`, the `runtime_bif!` macro)

## Intent
Every registered BIF whose implementation needs the *running* runtime (Stage 5, Part I §A.7): process control, timers, code loading, system/nodes introspection, the ETS C interior, the `erts_debug`/`erl_debugger` tools, the `re` engine, the `erl_ddll` loader and the distribution machinery. Each such BIF bridges through the single `RuntimeHooks` seam (`erlang:memory/0,1` through `MemoryHook`); with no hook installed a call answers the typed `unsupported_bif` capability gap — never `undef`, never a fabricated value.
**195 keys** are registered here through `RuntimeHooks`, plus `erlang:memory/0,1` through `MemoryHook`. The 13 `atomics`/`counters` resource-object keys and the self-contained `erl_ddll:format_error_int/1` are **not** seam-backed: they are real implementations in `src/runtime/atomics.rs` and `src/runtime/ddll.rs` (see below).
Ground truth (read-only): the corresponding C in `../../../erts/emulator/beam/` (`erl_bif_*.c`, `bif.c`, `erl_time_sup.c`, `erl_db_*.c`, `erl_bif_atomics.c`, `erl_bif_counters.c`), plus `string:misc`/`global` semantics for the tools' surfaces.

## API Surface
Keys by file:
- `src/runtime/mod.rs`: the module wiring, the `runtime_bif!` macro and the `register` order (`memory`, `erlang`, `erts_internal`, `erts_debug`, `other`, then `atomics` **last** so the real resource-object registrations win the last-registration-wins race).
- `src/runtime/memory.rs` (2): `erlang:memory/0,1` — the one family implemented against a real seam: `memory/0` renders the hook's `Vec<(&'static str, u64)>` as `{Key, Value}` in the hook's (oracle) key order — `total, processes, processes_used, system, atom, atom_used, binary, code, ets`; `memory/1` gives a plain integer for a single atom key, a `{K,V}` list in **requested** order for an atom list, and `badarg` for an unknown key or a malformed argument.
- `src/runtime/erlang.rs` (77): process control, timers, code loading, system/nodes introspection.
- `src/runtime/erts_internal.rs` (56): port/process-control/code-loading internals, the trace sessions, the distribution internals, `term_to_string`.
- `src/runtime/erts_debug.rs` (18): `interpreter_size`/`disassemble`/`lcnt_*`/`set_internal_state`/….
- `src/runtime/other.rs` (44): `erl_debugger` 13, `re` 8, `erl_ddll` 6, `code` 7 coverage, `ets` 2 (`internal_request_all/0`, `slot/2`), `erts_trace_cleaner` 2, `erts_literal_area_collector` 2, `prim_file`/`os`/`net_kernel`/`error_logger` 1 each.
- `src/runtime/atomics.rs` (13, **real implementations**, not seam-backed): the `atomics`/`counters` resource objects — see `## The atomics/counters resource objects`.
- `src/runtime/ddll.rs` (1, **real implementation**, not seam-backed): `erl_ddll:format_error_int/1` — see `## The erl_ddll error table`.
- `runtime_bif!(reg, "module", function, arity)` defines a block-scoped `fn` forwarding to `BifContext::call_runtime_bif(module, function, args)`, so one `register` body holds any number of keys (one source line per BIF); no `BifFn` per key in a table.
- **With no hook every seam-backed key answers the typed `unsupported_bif`** — no key is special-cased to a constant return, no fabricated value, no `undef`. `RuntimeHooks::call` is a single dispatcher: a partial adopter matches the keys it implements and returns `unsupported_bif` for the rest. (Empirically, all 195 currently raise under `beam_rt`, which has no such hook; registering them can only turn exit 1 into the honest exit 3, never shadow a working result — `beam_rt`'s native floor is dispatched *before* the registry.)

## The atomics/counters resource objects (`atomics.rs`, 13 keys, real)
The 13 `bif.tab` keys of the `atomics`/`counters` resource objects are implemented for real as plain `BifFn`s (registered last in `register`): `erts_internal:atomics_new/2`, `atomics:get/2`, `atomics:put/3`, `atomics:add/3`, `atomics:add_get/3`, `atomics:exchange/3`, `atomics:compare_exchange/4`, `atomics:info/1`, `erts_internal:counters_new/1`, `erts_internal:counters_get/2`, `erts_internal:counters_add/3`, `erts_internal:counters_put/3`, `erts_internal:counters_info/1`.

- **Representation.** The handle is an on-heap internal `Ref` thing whose id is drawn from `RESOURCE_ID_BASE = 1 << 52`, disjoint from every other reference id space in the crate (compiled patterns `1 << 32`, ETS continuations `1 << 40`, reverse continuations `1 << 41`, compiled specs `1 << 48`). The cells live in a module-local process-global registry — `static OnceLock<Mutex<HashMap<u64, Resource>>>` — exactly like the compiled-pattern (`src/binary.rs`) and ETS continuation/spec registries. A `Resource` is either `Atomics { signed, values: Vec<u64> }` or `Counters { values: Vec<i64> }`.
- **Semantics.** An `atomics` cell is a raw `u64`; `Opts` bit 0 (`OPT_SIGNED`) selects whether it is read/written as `i64` or `u64`, so one variant serves both. 64-bit arithmetic wraps (`wrapping_add`). `atomics_new/2` rejects a `0` element count (`badarg`) and an overflowing `cnt * 8` (`system_limit`), and reserves fallibly so an unservable size is `system_limit`, never an abort; `counters_new/1` rejects `0` (`badarg`) and a count above `COUNTERS_MAX_SIZE = (1 << 32) - 1` (`system_limit`).
- **Return conventions.** `get/2` the current value (rendered for signedness); `put/3`/`add/3` `ok`; `add_get/3` the **new** value; `exchange/3` the **previous** value; `compare_exchange/4` `ok` when the current cell equals `Expected` (as raw bits, storing `Desired`), otherwise the **actual** current value with nothing written. `counters_get/2` the signed value; `counters_add/3`/`counters_put/3` `ok` (`counters_put/3` requires a value that fits `i64`; `counters_add/3` accepts any `i64`- or `u64`-fitting increment as a wrapping add).
- **`info`.** `atomics:info/1` returns the flat map `#{max, memory, min, size}` in the oracle's canonical key order `max, memory, min, size`, where `max`/`min` are the signedness bounds (`i64::MAX`/`i64::MIN`, or `u64::MAX`/`0`) and `memory = 40 + 8 * N`. `erts_internal:counters_info/1` returns `#{memory, size}` in key order `memory, size`, where `memory = 32 + ATOMICS_PER_COUNTER * ceil(N / ATOMICS_PER_CACHE_LINE) * ERTS_CACHE_LINE_SIZE` with `ATOMICS_PER_COUNTER = available_parallelism() + 1` and cache line 64 (an ERTS-model approximation of the C's per-scheduler replication, so the figure tracks the host's parallelism, not a scheduler count).
- **Scope.** Only the 13 `bif.tab` keys are registered here; the stdlib `atomics:new/2` and `counters:new/2` wrappers are pure Erlang (they call `erts_internal:atomics_new/2`/`counters_new/1`).
- **Known Milestone-1 caveat (leak).** Registry entries are **never reclaimed** — there is no off-heap finalizer for a resource object yet, so a handle that goes out of scope leaks its cells. This is the same bounded M1 leak as the `binary.rs` compiled-pattern and `ets` continuation/compiled-spec registries (crate `CONTEXT.md`).

## Handoff to `beam_abi` — `erl_ddll` and the driver loader
The driver loader is **not** the NIF loader. `beam_abi` exposes a real `dlopen`-based driver loader (`crates/beam_abi/src/driver/loader.rs`, `load(path, name) -> DriverLibrary`: validates the `ErlDrvEntry`, runs `start`, and on `Drop` runs `finish` + `dlclose`). But the **live driver registry** and `open_port({spawn_driver, Name})` live in `beam_rt` (`crates/beam_rt/src/port/driver.rs`), and `beam_io::abi::DriverTable` is not on that path. So the seven `erl_ddll` keys split:
- **Implemented for real** (no loader registry): `erl_ddll:format_error_int/1` — a pure error-atom/code → message table in `src/runtime/ddll.rs` (see below).
- **Needs the live registry** (owned by `beam_rt`, so typed `unsupported_bif` from inside `beam_bifs` alone): `loaded_drivers/0`, `try_load/3`, `try_unload/2`, `monitor/2`, `demonitor/1`, `info/2`. (On the oracle `loaded_drivers()` → `{ok,["tcp_inet","udp_inet","ram_file_drv"]}`.)

## The `erl_ddll` error table (`ddll.rs`, 1 key, real)
`erl_ddll:format_error_int/1` is the one self-contained `erl_ddll` key: a pure error-description → charlist translation, registered as a plain `BifFn` (not a `runtime_bif!` seam). Ground truth `erl_bif_ddll.c:813/1201/1709/1722` + `erl_unix_sys_ddll.c:286`.
- **A bare atom** resolves through the C's two sources in order: the Erlang-interface-only `switch` (9 atoms — `inconsistent`, `linked_in_driver`, `permanent`, `not_loaded`, `not_loaded_by_this_process`, `not_pending`, `already_loaded`, `pending_reload`, `pending_process`) and then the `errcode_tab` (9 atoms — `no_error`, `no_driver_init`, `driver_init_failed`, `bad_driver_name`, `driver_name_to_long`, `driver_incorrect_version`, `no_ddll_available`, `unspecified_error`, `symbol_not_found`); the messages are byte-exact, including the C's `"permanent an cannot"` / `"name to long"` typos. Any other atom is `badarg`.
- **A `{open_error, Code}`** (2-tuple, `Code` a **small** integer — a float, a bignum, a non-`open_error` tag or any other arity is `badarg`) maps through the fixed code table `0, -1..=-8`, then the UNIX `erts_sys_ddll_error` rule with an empty dynamic errcode table: `Code > -10` → `"Unspecified error"`, else `"Unknown dlload error"`. The C stores the code in a 32-bit `int`, so a small integer wider than 32 bits **truncates** first — observable and reproduced (`{open_error, 1 bsl 58}` → `"No error"`, `{open_error, 4294967295}` → `"No driver init in dynamic library"`).
- **Anything else** (a plain integer, a list, a binary, a `{open_error, NonSmallInt}`) is `badarg`.

## `re` — why the eight keys stay gap-backed
All 8 `re:*` keys (`compile/1,2`, `import/1`, `inspect/2`, `internal_run/4`, `run/2,3`, `version/0`) stay the typed `unsupported_bif`: a faithful `re` needs PCRE2, and the workspace licensing policy (Apache-2.0/MIT only) forbids adding a third-party regex engine. (On the oracle `re:version()` → `<<"10.47 2025-10-21">>`.)

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; a missing/incomplete seam is the typed `unsupported_bif` capability gap, never a fabricated value and never `undef`.
- `#![deny(unsafe_code)]` at the crate root; nothing here needs `unsafe`.
- One `runtime_bif!` invocation per `(module, function, arity)` seam key; the seam is total, so a family module never branches on whether a hook is installed beyond forwarding. Two families are the exception — they register real `BifFn`s: `src/runtime/atomics.rs` (the resource objects) and `src/runtime/ddll.rs` (the error table).
- The hook contract is fixed by `crate::context`: `RuntimeHooks::call(heap, atoms, module, function, args)`; `MemoryHook::memory() -> Vec<(&'static str, u64)>`.

## Routing Table
- `./mod.rs` -> module wiring + the `runtime_bif!` macro + the `register` order
- `./memory.rs` -> `erlang:memory/0,1` (the `MemoryHook` seam)
- `./erlang.rs` -> the 77 `erlang` runtime-coupled BIFs (process control, timers, code loading, system/nodes introspection)
- `./erts_internal.rs` -> the 56 `erts_internal` keys (port/process-control/code-loading internals, the trace sessions, the distribution internals, `term_to_string`)
- `./erts_debug.rs` -> the 18 `erts_debug` keys
- `./other.rs` -> the 44 remaining keys (`erl_debugger` 13, `re` 8, `erl_ddll` 6, `code` 7, `ets` 2, `erts_trace_cleaner` 2, `erts_literal_area_collector` 2, `prim_file`/`os`/`net_kernel`/`error_logger` 1 each)
- `./atomics.rs` -> the 13 real `atomics`/`counters` resource-object BIFs (the `Ref`-thing handle + process-global cell registry)
- `./ddll.rs` -> the real `erl_ddll:format_error_int/1` error table (atoms + `{open_error, Code}` → charlist)
- `../context.rs` -> the `RuntimeHooks`/`MemoryHook` seams and `BifContext::call_runtime_bif`/`memory`
- `../ets/` -> the `ets:*` surface the 2 `ets` keys here are the residue of
- `../CONTEXT.md` -> the crate root (Intent/API Surface/Constraints/Test Strategy/Routing Table)
