# Rust ERTS Rewrite — Crate Layer (`erts_rust/crates/`)

## Intent
The implementation layer of the Rust ERTS rewrite: the BEAM runtime decomposed **along its
subsystem seams** (Part II §A/§H) into 15 crates. Each crate is one routing node owning one
subsystem's public API; together they reproduce the **nine compatibility contracts** of
`../CONTEXT.md` §1 at **Level-1 drop-in fidelity** and implement the staged plan §7.
This node owns *all* runtime/engine/ABI code; tooling lives in the sibling `../tools/` node.

## Cross-crate contracts (BINDING on every child crate)
Inherited and refined from `../CONTEXT.md` — do not restate, obey:
- **§2 Frozen term layout.** Term construction/inspection is **only** `beam_term` (`use
  beam_term::prelude::*;`). No crate may assume a different `Eterm` layout or redefine it.
- **§3 Error strategy — no silent swallowing.** All fallible APIs return
  `beam_common::Result<T>` (= `Result<T, BeamError>`). Never invent a crate-local error enum, never
  `unwrap()` on runtime data, never panic-as-control-flow. `beam_term::TermError` is reconciled into
  `BeamError` by an **upstream `From<TermError> for BeamError` impl owned by `beam_term`** (see
  Design Decisions) so `?` works across the boundary.
- **`unsafe` confinement.** `unsafe` only in explicitly-marked modules; every `unsafe fn` has a
  `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale (convention:
  `beam_common::safety`). The *wire/serialization and decode* paths (ETF wire layer, `.beam`
  container parsing/decoding, table logic, allocator bookkeeping, scheduler) must be safe Rust;
  term materialization that writes raw heap words is confined to explicitly-marked modules. `unsafe_op_in_unsafe_fn` stays clean.
- **No duplication across crates.** A type/utility needed by ≥2 crates goes to the **lowest common
  ancestor** crate, never copy-pasted. Adding a new *dependency edge* changes the layer DAG — report
  it to this node; do not add `*_dev`-only cycles.
- **Files ~1000 lines**, split by responsibility. A legitimately long file (generated table, protocol
  table, comprehensive test module) documents *why* at its top or in the crate `CONTEXT.md`.
- **Generated data is source, not output.** `emu/ops.tab`, `bif.tab`, `atom.names`, `erl_alloc.types`
  (`../../erts/emulator/beam/`) are hand-maintained inputs — parse/regenerate, never hand-edit a
  derived artifact.
- **Licensing** Apache-2.0; new external deps must be Apache-2.0/MIT and justified.

### The seam types (the inter-crate interface each crate MUST expose for its dependents)
This is the frozen public-API shape dependents design against; keep it stable.
- `beam_term` → frozen term layout; `map` (flat-map ↔ HAMT/CHAMP builder: `write_map`/`map_words_capacity`/
  `map_size`/`map_get`/`map_pairs`/`map_put`/`map_remove`), `bitstring` (`write_heap_bits`/`write_binary`/
  `write_sub_bits`/public `bitstring_parts`/`bit_at`/`float_bits`/`float_value`), `refc` (`BinRef`/`ErlSubBits`
  layout + accessors), plus `thing`/`compare`/`hashing`/`size`.
- `beam_alloc` → `Allocator`/`AllocRef` (carrier-backed), `Heap`/bump process-heap, size classes; `RefCount`
  (the single `erts_refc_*` primitive); `RefcBinary` (off-heap refcounted large-binary payload).
- `beam_abi` → `#[repr(C)]` NIF (`ERL_NIF_TERM`, `ErlNifEnv`, `ErlNifEntry`, `enif_*` fn table) and
  driver (`ErlDrvEntry`, `ErlDrvBinary`, `erl_drv_*`) types + `Eterm ↔ ERL_NIF_TERM` transmute.
- `beam_codec` → `Encoder`/`Decoder` (ETF), tag constants, `VERSION_MAGIC = 131`, compression.
- `beam_loader` → the **typed block-structured IR** (`Module`, `Function`, `Block`, ops with
  safepoints/failure/branch/catch/line info) that `beam_engine` consumes — see `../CONTEXT.md` §4.
- `beam_engine` → the execution loop + an **`ExecutionContext` trait** (BIF dispatch, term alloc,
  process/stack state, trap/signal handling) so the engine depends only on `beam_loader` and the
  concrete context is supplied by `beam_rt` (keeps the Stage-1/Stage-5 DAG acyclic).
- `beam_ets` → `Table` handle + set/ordered_set/hash ops.
- `beam_gc` → per-process copying collector entry (`gc`/`collect`) + `OffHeap` refcount interface
  (`OffHeapEntry::{Mso, Binary}`); re-exports `beam_alloc::RefCount`.
- `beam_message` → `copy_struct` (term movement across heaps) + send/receive envelope.
- `beam_process` → `Process`/`ProcessId`, `Scheduler`/run queue, signals/links/monitors.
- `beam_bifs` → `BifRegistry` + `BifFn` + the registry generated from the `bif.tab` surface.
- `beam_io` → the poller/reactor (`mio`-backed) + `Port` + built-in-driver registry.
- `beam_rt` → the runtime **binary**: boot sequence, CLI flags, `system_info`, and the concrete
  `ExecutionContext` wiring engine + bifs + process; it owns the end-to-end vertical slice.

## Layering & dependency waves (design order — dependents design against a finished parent API)
Determined by the Cargo workspace DAG; a crate may only use crates in earlier waves.
| Wave | Crates | Depends on |
|---|---|---|
| 0 | `beam_alloc`, `beam_abi` | `beam_common`, `beam_term` |
| 1 | `beam_codec`, `beam_ets`, `beam_gc` | wave 0 |
| 2 | `beam_loader`, `beam_message`, `beam_dist` | wave 1 (`beam_codec`; `beam_message` also `beam_gc`, `beam_dist` also `beam_alloc`) |
| 3 | `beam_process` | `beam_gc`, `beam_message`, `beam_alloc` |
| 4 | `beam_engine` | `beam_loader` |
| 5 | `beam_bifs`, `beam_io` | `beam_process` (+ codec/ets/gc) |
| 6 | `beam_rt` | all |
`beam_common` + `beam_term` (Stage-0 foundations) are the frozen dependency sinks — **consume, do
not redesign**.

## Milestone-1 scope (what "implemented" means for *this* pass)
Full ERTS is a multi-year effort; every crate delivers a **real, tested, honestly-scoped core**, not
a full port. Coverage subsets are stated in each crate `CONTEXT.md`. Priority: the **vertical slice**
(`beam_loader` + `beam_engine` + `beam_rt` load and run a real `.beam`) is the key evidence and is
built first-class; other crates deliver their Stage-scoped core in parallel.
| Crate | Milestone-1 deliverable |
|---|---|
| `beam_alloc` | carrier allocator + size classes + process-heap bump allocator |
| `beam_abi` | layout-frozen NIF/driver struct + fn-table types, static size/offset asserts, transmute |
| `beam_codec` | ETF encode/decode of the core tag set + `VERSION_MAGIC 131` round-trips |
| `beam_dist` | `DFLAG_*`/`DOP_*` constants, handshake + control-message framing, `epmd` client |
| `beam_loader` | chunk parse + BEAM disassembly + lowering to the typed block IR |
| `beam_engine` | IR + direct-threaded interpreter over the generic-opcode subset + `ExecutionContext` |
| `beam_gc` | generational semi-space Cheney copy + MSO refcount |
| `beam_message` | `copy_struct` + send/receive envelope |
| `beam_process` | `Process`, run queue, scheduler loop, signals/links/monitors |
| `beam_bifs` | `BifRegistry` + core `erlang` BIFs (arith, comparison, type tests, term ops) + `io` floor |
| `beam_ets` | `set` + `ordered_set` core ops (`new/insert/lookup/delete/select`) |
| `beam_io` | `mio` poller/reactor + `Port` + one built-in driver |
| `beam_rt` | boot/CLI/`system_info` + end-to-end vertical-slice integration test |

## Term/heap foundation: maps · bitstrings · refc binaries (current state)
The shared blockers that capped maps, bitstrings and large-binary memory efficiency are implemented.
Frozen inter-crate contract — all term-layout writers are **heap-agnostic** (`beam_term` cannot allocate;
the caller reserves the words and passes `hp: *mut Eterm`):
- **Maps** (`beam_term::map`) — flat map ↔ HAMT/CHAMP at the ERTS threshold `MAP_SMALL_MAP_LIMIT = 32`
  (`erl_map.c`). `write_map` (dedup last-wins, canonical order, flat map ≤32 else hash map),
  `map_words_capacity(n)` for sizing, `map_size`/`map_get`/`map_pairs`(key-ordered)/`map_put`/`map_remove`.
  Node interiors are the **ERTS layout** — head-array `[hdr][size][16 slots]`, head-bitmap `2+n`, inner
  node-bitmap `1+n`, one term per slot, flat-map/collision leaves — chosen so `beam_gc::copy::move_size`
  and its Cheney scan already size and walk it (proven by `beam_gc/tests/map_copy.rs`). `compare`/`hashing`/
  `size` are structural over maps.
- **Bitstrings** (`beam_term::bitstring`) — `heap_bits_size_words`, `bit_count_to_bytes`, `write_heap_bits`,
  `write_binary`, `write_sub_bits`/`sub_bits_parts`, public `bitstring_parts`/`bit_at`/`float_bits`/
  `float_value`; non-byte-aligned supported. (Replaces `beam_codec`'s private `binary.rs` shims.)
- **Refc binaries** — bit-exact on-heap layout in `beam_term::refc`: `BinRef` (3 words) + `ErlSubBits`
  (5 words), the 8-word `[BinRef][ErlSubBits]` pair with the root at the SubBits; `ERL_ONHEAP_BINARY_LIMIT
  = 64` bytes / `ERL_ONHEAP_BITS_LIMIT = 512` bits. Off-heap payload = `beam_alloc::RefcBinary` (refcounted,
  `AllocType::Binary`); **`BinRef.val` == `RefcBinary::data_ptr()` — the pointer to the first payload byte**
  (the internal header is `beam_alloc`'s concern). Reclamation: hold a `RefcBinary` handle in
  `beam_gc::OffHeap` and release it when the on-heap reference dies. Small binaries stay on-heap
  (`ErlHeapBits`).

## Handoffs (owned by other nodes — NOT yet adopted)
- `beam_codec`: its private `write_large_flatmap` / `MAP_SMALL_MAP_LIMIT` / `binary.rs` shims duplicate
  `beam_term::{map,bitstring}`; it currently decodes >32-key maps as oversized flat maps — switch to
  `map::write_map` (builder) + `map::map_pairs` (encode).
- `beam_engine` / `beam_bifs`: `maps:*` on large maps via `map::{map_get,map_size,map_pairs,map_put,map_remove}`.
- `beam_process`: register off-heap refc binaries (`RefcBinary`) in `beam_gc::OffHeap` and supply the `sweep`
  liveness predicate (liveness of an off-heap entry is the caller's concern; the collector never walks
  off-heap payloads).

## Constraints
- **Keep the workspace green**: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --workspace`
  must pass after every wave. Per-crate gate:
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p <crate>`.
- **Build env:** sandbox `$HOME` is read-only — always run cargo via the wrapper
  `erts_rust/scripts/cargo.sh` (sets a writable `CARGO_HOME`). The cargo workspace manifest is
  `erts_rust/Cargo.toml`, NOT the repo root, so from the repo root ALWAYS pass
  `--manifest-path erts_rust/Cargo.toml` (a bare `cargo -p <crate>` from the root fails: "could not
  find Cargo.toml"). Never `cd`. Never edit `../tools/`.
- Every crate ships a `CONTEXT.md` (Intent, API Surface, Constraints, Test Strategy, Routing Table)
  **before** dependents are designed against it.

## Test Strategy
- Per-crate: in-module `#[cfg(test)]` unit tests + `tests/` integration tests for cross-module seams;
  a documented test strategy section in each crate `CONTEXT.md`.
- The **compatibility gate** is the sibling `../tools/differential/` harness (run a module under real
  ERTS and under Rust ERTS, compare observable results) — no behavior change without a case.
- The vertical slice adds a `.beam` fixture (produced with the system `erlc`) + an integration test
  asserting the printed result.

## Routing Table
- `beam_common/` → foundation: `BeamError`/`Result`, version constants, config/feature flags, logging, `unsafe` convention (frozen, Stage 0)
- `beam_term/` → frozen tagged term representation, immediates, boxed terms, atom table, literal areas (frozen, Stage 0)
- `beam_alloc/` → allocators (`erts_alloc` replacement): carriers, size classes, process heap/bump allocator
- `beam_abi/` → NIF C ABI (`enif_*`) + driver C ABI (`driver_*`/`erl_drv_*`), layout-frozen structs
- `beam_codec/` → External Term Format (ETF) encode/decode, `VERSION_MAGIC 131`
- `beam_dist/` → distribution protocol v6 (`DFLAG_*`/`DOP_*`), handshake, `epmd`
- `beam_loader/` → `.beam` parsing, loader, `ops.tab` transforms, typed block-structured IR
- `beam_engine/` → typed IR execution, direct-threaded interpreter, BeamAsm + Cranelift JIT tiers, `ExecutionContext` seam
- `beam_gc/` → per-process generational copying GC, MSO sweep, heap sizing (Stage 4)
- `beam_message/` → copy engine (`copy_struct`) + messaging (send/receive, fragments)
- `beam_process/` → processes, scheduler, run queues, thread progress, signals/links/monitors
- `beam_bifs/` → BIF registry (`bif.tab` surface) + implementations
- `beam_ets/` → ETS tables (set/ordered_set/bag/duplicate_bag), B-tree/CHAMP interiors
- `beam_io/` → poller/reactor (epoll/kqueue/io_uring), ports, built-in drivers
- `beam_rt/` → runtime binary: boot/preloaded modules, CLI flags, `system_info`, glue, vertical slice
- `../tools/differential/` → differential-test harness vs real ERTS (sibling — read-only, escalate writes to `..`)
- `../tools/bench/` → benchmark driver for external targets + system-ERTS baseline (sibling — read-only, escalate writes to `..`)
- `../../erts/emulator/beam/` → legacy C core: ground-truth behavior/ABI to reproduce (read-only reference)

## Status (Milestone 1)
- **All 15 crates architected, implemented and tested.** `cargo test --manifest-path
  erts_rust/Cargo.toml --workspace` is GREEN (69 test suites, 0 failures); build is warning-free.
  `beam_common`/`beam_term` were already implemented; every other crate followed its CONTEXT.md.
- **Vertical slice WORKS end-to-end**: `beam_loader` + `beam_engine` + `beam_rt` load a real `.beam`
  (fixture `beam_loader/tests/fixtures/slice.beam`, built with the system `erlc`) and execute it:
  `beam_rt run <slice.beam> slice:add/2 2 3` → `5`; `slice:fib/1 10` → `55`. The interpreter's
  supported opcode/operand subset is documented honestly in `beam_engine/CONTEXT.md` (the `Generic`
  fallback returns a typed error, never a wrong result).
- **Additive `beam_term` foundation extensions** (the term-layout LCA; no frozen constant/ABI
  changed): `From<TermError> for BeamError` (error interop); `compare`/`hashing`/`size` utilities
  (term ordering, structural hash, `size_object`); `thing` — heap-agnostic off-heap term
  materialization (float/big/flatmap/external pid/port/ref), which `beam_codec::term` now forwards
  to (de-duplication). Rationale: `beam_term` is a dependency sink, so writers take a caller-provided
  `*mut Eterm`; allocation stays in `beam_alloc`.
- **Dependency edges added** beyond the scaffold: `beam_message → beam_gc`, `beam_dist → beam_alloc`.

## Known Structural Debt (candidates for the next pass)
- `beam_message::OffHeapTable` is an indexed read view needed by the copy boundary, while
  `beam_gc::OffHeap` is push-only; the view should be absorbed into `beam_gc` (Prevent-Duplication).
- `beam_codec::decode.rs` keeps a private `MAP_SMALL_MAP_LIMIT` decode-policy constant (not a layout
  duplicate) — fine, but a future home for map-policy constants may be `beam_term`.
- Repo-root owned (outside this node): a stray tracked 2 MB `erl_crash.dump`; owed `../../CHANGE.md`
  entries for the `beam_term` extensions and the `beam_abi` frozen-header seam work.

## Design Decisions
- **Error interop is owned upstream, not duplicated.** `beam_term` already declares (but does not
  yet use) its `beam_common` dependency. The `From<beam_term::TermError> for beam_common::BeamError`
  conversion is added **in `beam_term`**, so all 13 dependents get `?`-interop for free instead of
  each writing a mapping (Prevent-Duplication). Purely additive; it is the "reconcile once
  `beam_common` lands" step `beam_term`'s own `CONTEXT.md` prescribes. (Needs a `../../CHANGE.md`
  note — flagged to the parent node, which owns the repo root.)
- **Engine depends only on `beam_loader`** (as scaffolded). It therefore defines an
  `ExecutionContext` trait for BIF dispatch / allocation / process state; `beam_rt` supplies the
  concrete implementation over `beam_bifs`/`beam_process`. This keeps the Stage-1 engine independent
  of the Stage-5 runtime core (acyclic DAG) and makes the engine testable with a minimal context.
- **Cranelift is an optional tier.** `beam_engine` must keep the interpreter working with the
  Cranelift backend behind a feature/stub so the build and the vertical slice do not depend on
  Cranelift codegen — the hot tier is measurement-gated (Part II §F), never on the critical path.
- **`beam_term::heap::Heap` is a test arena; `beam_alloc` is the production allocator.** Term-
  constructing crates use the `beam_alloc` interface; `beam_term::heap` stays for unit tests only.

## Notes for Agents
- Read `../CONTEXT.md` (workspace contracts) and the two `../../ERTS_RUST_REWRITE_*.md` design docs
  before designing a crate; cite the relevant Part I/II section in the crate `CONTEXT.md`.
- Legacy C ground truth for a crate lives in `../../erts/emulator/beam/` (e.g. `erl_alloc*.c`,
  `erl_term.h`, `external.c`, `dist.c`, `copy.c`, `erl_message.c`, `erl_gc.c`, `erl_db_*.c`,
  `beam_load.c`, `beam_file.c`, `emu/ops.tab`, `jit/asm_load.c`, `erl_nif.c`, `erl_driver.h`,
  `erl_bif_*.c`, `bif.tab`, `atom.names`). **Read it; never modify it.**
- The system Erlang toolchain (OTP 29 / erts-17.1) is on `PATH` — `erlc`/`erl`/`escript` are usable
  for producing `.beam` fixtures and as the differential oracle.

## See Also
- `../CONTEXT.md` → workspace root: the binding cross-crate contracts and routing table.
- `../../ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and `../../ERTS_RUST_REWRITE_DEEPDIVE.md`
  (Part II) → design source of truth, incl. the Stage 0–6 plan (§7/§H).
- `../../CHANGE.md` → log of major structural/data-structure/algorithmic changes (repo-root owned).
- `../../erts/emulator/beam/CONTEXT.md` → the legacy core being ported.
