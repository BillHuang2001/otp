# `beam_bifs::runtime` — the runtime-coupled BIF families (`src/runtime/`, the `runtime_bif!` macro)

## Intent
Every registered BIF whose implementation needs the *running* runtime (Stage 5, Part I §A.7): process control, timers, code loading, system/nodes introspection, the resource-object stores (`atomics`/`counters`), the ETS C interior, the `erts_debug`/`erl_debugger` tools, the `re` engine, the `erl_ddll` loader and the distribution machinery. Each such BIF bridges through the single `RuntimeHooks` seam (`erlang:memory/0,1` through `MemoryHook`); with no hook installed a call answers the typed `unsupported_bif` capability gap — never `undef`, never a fabricated value.
220 keys are registered here through `RuntimeHooks`, plus `erlang:memory/0,1` through `MemoryHook` (222 runtime-coupled keys in all).
Ground truth (read-only): the corresponding C in `../../../erts/emulator/beam/` (`erl_bif_*.c`, `bif.c`, `erl_time_sup.c`, `erl_db_*.c`), plus `string:misc`/`global` semantics for the tools' surfaces.

## API Surface
Keys by file:
- `src/runtime/mod.rs`: the module wiring, the `runtime_bif!` macro and the `register` order (`memory`, `erlang`, `erts_internal`, `erts_debug`, `other`).
- `src/runtime/memory.rs` (2): `erlang:memory/0,1` — the one family implemented against a real seam: `memory/0` renders the hook's `Vec<(&'static str, u64)>` as `{Key, Value}` in the hook's (oracle) key order — `total, processes, processes_used, system, atom, atom_used, binary, code, ets`; `memory/1` gives a plain integer for a single atom key, a `{K,V}` list in **requested** order for an atom list, and `badarg` for an unknown key or a malformed argument.
- `src/runtime/erlang.rs` (77): process control, timers, code loading, system/nodes introspection.
- `src/runtime/erts_internal.rs` (62): the `counters_*`/`atomics_new`, port/trace/suspend machinery, `term_to_string`, the trace sessions.
- `src/runtime/erts_debug.rs` (18): `interpreter_size`/`disassemble`/`lcnt_*`/`set_internal_state`/….
- `src/runtime/other.rs` (63): `ets` 13 C-interior, `erl_debugger` 13, `re` 8, `erl_ddll` 7, `code` 7 coverage, `atomics` 7, `erts_trace_cleaner` 2, `erts_literal_area_collector` 2, `prim_file`/`os`/`net_kernel`/`error_logger` 1 each.
- `runtime_bif!(reg, "module", function, arity)` defines a block-scoped `fn` forwarding to `BifContext::call_runtime_bif(module, function, args)`, so one `register` body holds any number of keys (one source line per BIF); no `BifFn` per key in a table.
- **With no hook every key answers the typed `unsupported_bif`** — no key is special-cased to a constant return, no fabricated value, no `undef`. `RuntimeHooks::call` is a single dispatcher: a partial adopter matches the keys it implements and returns `unsupported_bif` for the rest. (Empirically, all 220 currently raise under `beam_rt`, which has no such hook; registering them can only turn exit 1 into the honest exit 3, never shadow a working result — `beam_rt`'s native floor is dispatched *before* the registry.)

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; a missing/incomplete seam is the typed `unsupported_bif` capability gap, never a fabricated value and never `undef`.
- `#![deny(unsafe_code)]` at the crate root; nothing here needs `unsafe`.
- One `runtime_bif!` invocation per `(module, function, arity)` key; the seam is total, so a family module never branches on whether a hook is installed beyond forwarding.
- The hook contract is fixed by `crate::context`: `RuntimeHooks::call(heap, atoms, module, function, args)`; `MemoryHook::memory() -> Vec<(&'static str, u64)>`.

## Routing Table
- `./mod.rs` -> module wiring + the `runtime_bif!` macro + the `register` order
- `./memory.rs` -> `erlang:memory/0,1` (the `MemoryHook` seam)
- `./erlang.rs` -> the 77 `erlang` runtime-coupled BIFs (process control, timers, code loading, system/nodes introspection)
- `./erts_internal.rs` -> the 62 `erts_internal` keys (`counters_*`/`atomics_new`, port/trace/suspend machinery, `term_to_string`, the trace sessions)
- `./erts_debug.rs` -> the 18 `erts_debug` keys
- `./other.rs` -> the 63 remaining keys (`ets` 13 C-interior, `erl_debugger` 13, `re` 8, `erl_ddll` 7, `code` 7, `atomics` 7, `erts_trace_cleaner` 2, `erts_literal_area_collector` 2, `prim_file`/`os`/`net_kernel`/`error_logger` 1 each)
- `../context.rs` -> the `RuntimeHooks`/`MemoryHook` seams and `BifContext::call_runtime_bif`/`memory`
- `../ets/` -> the `ets:*` surface the 13 C-interior keys here complete
- `../CONTEXT.md` -> the crate root (Intent/API Surface/Constraints/Test Strategy/Routing Table)
