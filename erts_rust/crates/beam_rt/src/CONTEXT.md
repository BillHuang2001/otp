# `beam_rt` source (`erts_rust/crates/beam_rt/src`) — runtime glue

## Intent
The implementation behind the `beam_rt` binary (crate docs live in `../CONTEXT.md`). This node is the
**glue layer**: it adapts `beam_engine`'s seam traits onto the real `beam_bifs` registry / `beam_alloc`
heaps / `beam_process` tables and owns every runtime-level store. `#![deny(unsafe_code)]`; errors are
return-based (`beam_common::Result`); terms are built/inspected only through `beam_term`.

## API Surface (file map)
- `lib.rs` / `main.rs` → crate root + thin CLI shim.
- `runtime.rs` (+ `runtime/`) → the `Runtime` façade (heaps, tables, the slice, the spawn API, the
  scheduler pass). Owns `Runtime::eval_heap` (pointer-stable arena) and `Runtime::eval_off_heap` (the
  **destination** off-heap table a transport copy writes its payload refs into). `SourcePayloads`
  (runtime.rs:496-503) + `stabilize_term[_with]` (:1041) + `source_payloads` (:1082).
- `context/mod.rs` (+ `context/`) → `RuntimeContext` + the single `impl ExecutionContext`, the delivery
  seams, the in-machine cross-module call seam (`resolve_external_call` :2135) and the nested-run entry
  points.
- `context/gc.rs` → the heap-admission points + the float guard (`module_uses_floats` :90) + the
  unscanned-root valve. `runtime/gc.rs` → `try_stabilize_escaped[_with]` (:427) + the GC valves.
- `runner/` → the scheduled-process driver (`entry.rs` holds the long-lived per-pid `Machine`).
- `native/` → the runtime-native BIF floor + the native `file_server_2` service (`native/file_server.rs`).
- `boot/`, `cli/`, `info/`, `port/`, `eval/` → boot script, CLI, `system_info`, ports, the `-eval`
  evaluator. `mailbox.rs` → per-pid receive scan state. `nif.rs`, `codeserver.rs`, `format.rs`,
  `error.rs`, `timers.rs`, `dist.rs`, `trace.rs`, `seq_trace.rs`, `crash_dump.rs`, `perf_map.rs`.

## Constraints
- **Call down, never duplicate**; every fallible API returns `beam_common::Result`; no `unwrap()` on
  runtime data; no crate-local error enum.
- **Pooled BIF heap is NOT reset.** `RuntimeContext::call_registered` borrows the runtime's pooled
  16 MiB BIF heap (`Runtime::take_bif_heap`/`put_bif_heap`) and returns it; the pool is a stack whose
  heaps are never reset/heap-reset. A term a BIF built stays valid — the pool is **not** an aliasing
  source.
- **One shared `AtomTable`** (the code server's); pending interned names are committed only when the
  interner is non-empty.

## Design Decisions
- **`SourcePayloads` union contract.** A transport copy resolves an off-heap refc payload through the
  copy's **source** `OffHeapTable`, which `stabilize_term_with` builds by cloning each
  `SourcePayloads.binaries` handle (`push_binary`) and each `off_heap` MSO handle (`push`), **in order**.
  `nodes` is carried but **never pushed** (a source node list is never read, and pushing would zero a
  shared count). `source_payloads(pid, machine_handles)` (runtime.rs:1082) unions the executing machine's
  `Machine::refc_binaries()` with the pid's mailbox-retained handles.
- **Relocation exemption.** `with_relocation_exempt` marks a process while a runtime-local term (the
  `-eval`/nested-run machines' `x`/stack/env) is outside the GC root set, so a delivery-driven collection
  cannot relocate words out from under it.
- **The scheduler pass moves the process table (and the runner's machine map) out of the `Runtime`**
  (`take_processes`/`put_processes`) because a pass needs `&mut Runtime` and `&mut ProcessTable` at once.

## Known Issues (refc-escape hazard — who owns a payload that leaves its building scope)
A refc binary's payload is owned **solely** by the `beam_alloc::RefcBinary` handle in
`Machine::refc_binaries` (`beam_engine/src/machine/mod.rs:210`; pushed only by `alloc_refc_binary`
:463-468; there is **no `impl Drop for Machine`**, and `Machine::reset` never clears the Vec). Dropping a
`Machine` releases every payload it built. `beam_bifs` builds only **on-heap** bitstrings
(`access::make_heap_bitstring`), so every off-heap refc payload comes from the engine
(`bs_init_writable`/`bs_create_bin` → `raw.rs::{make_binary,make_writable}`).

**A `Machine` that is created and dropped per call while a term it built escapes has no adopter.** Sites
that build such a machine (its `refc_binaries` dies with it):
- `RuntimeContext::run_export_blocking` (`context/mod.rs:881-898`, machine at :892) — reached by
  `run_export` (:812-837) and by `call_bif`'s external-module fallback (:2080). **Every `-eval`
  `Mod:Fun(...)` call goes through here** (`eval/mod.rs:1785` → `call_bif` → `run_export`), as does a
  `run`/`-s`/`-run` entry and every `call_bif`-dispatched (non-in-machine) module call.
- `RuntimeContext::run_label_blocking` (`context/mod.rs:941-958`, machine at :952) — reached by
  `apply_fun`'s `LocalFunTarget` arm (:2372) and the engine's `call_fun`/`call_fun2` arms.
- `boot::drive::apply_boot_call` (`boot/drive.rs:335`), and the native file-server io-device continuation
  machine (`native/file_server/get_until.rs:296`).
- The runner's per-pid machine (`runner/entry.rs:109`, `:201`) is **long-lived** and IS handed off
  (`runner/run.rs:452`, `:526`).

**Delivery/copy seams that resolve a refc payload.** Refc-aware: `send_message_with_off_heap`
(`context/mod.rs:2422-2456`, builds a local source from `Machine::refc_binaries()`), `stabilize_term_with`
(runtime.rs:1041), `source_payloads` (runtime.rs:1082), `try_stabilize_escaped_with` (runtime/gc.rs:427).
**Empty-source (a payload present ⇒ `BeamError::BadArg` from `beam_message::copy::copy_refc_binary`):**
`deliver_message` (`context/mod.rs:537-545`, `OffHeapTable::new()`), `deliver_in_table` (:617-625),
`deliver_message_with_off_heap` (:578-591) when its caller passes an empty `src`, and every runtime
delivery step that funnels through them — `Runtime::service_file_requests` (native file server replies),
`Runtime::deliver_port_messages`, `Runtime::fire_due_timers`, `Runtime::drain_ets_owner_exits` — plus
`native.rs:541`, `native/prim.rs`, `native/group_leader.rs`, `native/process.rs`, `runtime/ets_owner.rs`,
`trace.rs`, `context/runtime_hooks.rs` (all via `try_stabilize_escaped` = empty `SourcePayloads`). ETS /
persistent-term / group-leader / io-device paths take no `SourcePayloads` at all. **There is no
runtime-level owner store for a payload that escapes a dropped machine** — `Runtime::eval_off_heap` is a
*destination* table only.

## Routing Table
- `context/` → `RuntimeContext`, the `ExecutionContext` impl, delivery + call seams (`context/CONTEXT.md`)
- `runner/` → the scheduled-process driver (`runner/CONTEXT.md`)
- `native/` → the native BIF floor + file server (`native/CONTEXT.md`)
- `boot/` → preloaded modules + boot-script executor (`boot/CONTEXT.md`)
- `cli/` → the CLI flag surface (`cli/CONTEXT.md`)
- `info/` → `system_info/1` keys (`info/CONTEXT.md`)
- `port/` → the A.4 port layer (`port/CONTEXT.md`)
- `runtime/` → the `Runtime` façade sub-nodes (`runtime/CONTEXT.md`)
- `eval/` → the `-eval` expression evaluator
- `../..` → the crate node: `RunOutcome`, exit codes, CLI table, A.9 status (`../CONTEXT.md`)

## See Also
- `../CONTEXT.md` → the crate façade, the CLI table, the A.9 status, `M1_GAPS.md`/`TRACE_AND_PERF.md`.
- `../../beam_engine/HANDOFF_bs_init_writable.md` → the `bs_init_writable`/`bs_create_bin` refc path.
- `../../beam_message/src/copy.rs` → `copy_refc_binary` / `binary_by_data_ptr` (the resolver).
