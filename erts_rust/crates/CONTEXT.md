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
  process/stack state, trap/signal handling) plus a **`Mailbox` trait** — the receive/message seam
  (`send_message`/`receive_next`/`receive_skip`/`receive_remove`/`receive_reset`/`receive_marker_*`,
  every method **defaulted** to a typed `unsupported`) — so the engine depends only on
  `beam_loader`/`beam_alloc` and the concrete context is supplied by `beam_rt` (keeps the
  Stage-1/Stage-5 DAG acyclic). A receive that finds no message suspends through the engine's block
  outcome (`RunOutcome { Returned, Wait }` + `run_export_outcome`/`run_label_outcome`/`resume_outcome`;
  the plain `run_export` folds a suspend into the typed `error::blocked()`), which `beam_rt` maps onto
  `beam_process`'s scheduler.
- `beam_ets` → `Table` handle + set/ordered_set/hash ops.
- `beam_gc` → per-process copying collector entry (`gc`/`collect`) + `OffHeap` refcount interface
  (`OffHeapEntry::{Mso, Binary}`); re-exports `beam_alloc::RefCount`.
- `beam_message` → `copy_struct` (term movement across heaps) + the `Message` envelope + the `Queue`
  FIFO with a resumable `QueueCursor` scan position (`cursor`/`message_at`/`advance`/`remove_at`) that
  the receive seam drives.
- `beam_process` → `Process`/`ProcessId`, `Scheduler`/run queue/`ProcessRunner`, signals/links/monitors;
  **resume-on-send** (`Process::deliver -> DeliverOutcome` + `ProcessTable::drain_woken` /
  `Scheduler::resume_woken`) wakes a receiver blocked in `receive`.
- `beam_bifs` → `BifRegistry` + `BifFn` + the registry generated from the `bif.tab` surface, plus
  **fifteen** optional defaulted host seams (`OutputSink`/`SendHook`/`AtomInterner`/`FunCaller`/
  `PortHook`/`DictHook`/`PersistentTerm`/`NifLoader`/`RecordSource`/`LocalNode`/`LocalFunIdentity`/
  `ModuleInfo`/`ProcessHook`/`MemoryHook`/`RuntimeHooks`).
- `beam_io` → the poller/reactor (`mio`-backed) + `Port`/`PortRegistry` + port-task scheduling + built-in drivers (`fd`/`efile`, and the OS-child `spawn` driver with its `Driver::take_exit_status` seam).
- `beam_rt` → the runtime **binary**: boot sequence, CLI flags, `system_info`, and the concrete
  `ExecutionContext` wiring engine + bifs + process — including the `Mailbox` receive seam over the
  process mailbox, a single-threaded `ProcessRunner`/scheduler loop, and the `beam_bifs::PortHook`
  provider over a spawn driver; it owns the end-to-end vertical slice.

## Layering & dependency waves (design order — dependents design against a finished parent API)
Determined by the Cargo workspace DAG; a crate may only use crates in earlier waves.
| Wave | Crates | Depends on |
|---|---|---|
| 0 | `beam_alloc`, `beam_abi` | `beam_common`, `beam_term` |
| 1 | `beam_codec`, `beam_ets`, `beam_gc` | wave 0 |
| 2 | `beam_loader`, `beam_message`, `beam_dist` | wave 1 (`beam_codec`; `beam_message` also `beam_gc`, `beam_dist` also `beam_alloc`) |
| 3 | `beam_process` | `beam_gc`, `beam_message`, `beam_alloc` |
| 4 | `beam_engine` | `beam_loader`, `beam_alloc` |
| 5 | `beam_bifs`, `beam_io` | `beam_process` (+ codec/ets/gc) |
| 6 | `beam_rt` | all |
`beam_common` + `beam_term` (Stage-0 foundations) are the frozen dependency sinks — **consume, do
not redesign**.

## Implemented core (per-crate scope)
Full ERTS is a multi-year effort; every crate delivers a **real, tested, honestly-scoped core**, not
a full port. Coverage subsets are stated in each crate `CONTEXT.md`. Priority: the **vertical slice**
(`beam_loader` + `beam_engine` + `beam_rt` load and run a real `.beam`) is the key evidence and is
built first-class; other crates deliver their Stage-scoped core in parallel.
| Crate | Delivered core |
|---|---|
| `beam_alloc` | carrier allocator + size classes + process-heap bump allocator |
| `beam_abi` | layout-frozen NIF/driver struct + fn-table types, static size/offset asserts, transmute; the working `enif_*` host API (`NifHost` seam) + the `dlopen`-based NIF loader |
| `beam_codec` | ETF encode/decode of the core tag set + `VERSION_MAGIC 131` round-trips |
| `beam_dist` | `DFLAG_*`/`DOP_*` constants, handshake + control-message framing, `epmd` client |
| `beam_loader` | chunk parse + BEAM disassembly + lowering to the typed block IR |
| `beam_engine` | IR + direct-threaded interpreter + the native baseline JIT tier (default-OFF Cranelift hot tier) + `ExecutionContext` + the receive/message family over a `Mailbox` seam |
| `beam_gc` | generational Cheney copying + immix mature generation (measurement-gated) + MSO refcount |
| `beam_message` | `copy_struct` + send/receive envelope + the `QueueCursor` scan position |
| `beam_process` | `Process`, run queue, scheduler loop, resume-on-send, signals/links/monitors |
| `beam_bifs` | `BifRegistry` + the whole declared `bif.tab` surface (556 BIFs; registry 640 keys) + the host seams |
| `beam_ets` | `set` (CHAMP) + `ordered_set` (K-ary B+-tree) + `bag`/`duplicate_bag` (chained) over `new/insert/lookup/delete/select` |
| `beam_io` | `mio` poller/reactor + `Port`/`PortRegistry` + port-task scheduling + built-in `fd`/`efile` **and OS-child `spawn`** drivers |
| `beam_rt` | boot/CLI/`system_info` + the executor runner (send/receive) + the `PortHook` provider + end-to-end vertical-slice integration test |

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

- **Funs / exports** (`beam_term::fun`) — the on-heap `ErlFunThing` interior, as heap-agnostic
  writers/readers. Header word = `FUN_SUBTAG | arity<<8 | num_free<<16 | external<<24 | imported<<25`
  (the first three fields are C's `MAKE_FUN_HEADER` encoding). Three kinds: `Local` (descriptor
  `[uniq:u64][index:u32]` — what `beam_engine`'s `make_fun3` builds), `External` (`[module][function][arity]`,
  an `EXPORT_EXT` literal), `Imported` (10-word descriptor: module, 16-byte MD5, index, old_index,
  old_uniq, creator pid — a `NEW_FUN_EXT` literal). `env` follows the descriptor; block =
  `1 + desc_words(kind) + num_free`, with `desc_words` = 2 / 3 / 10. The C's out-of-line
  `ErlFunEntry`/`Export` `entry` pointer is **inlined into the term** — the same documented M1
  substitution as the external-pid `node` slot. `beam_gc` sizes/scans fun copies through
  `fun_block_words`/`fun_env_offset`, and `compare`/`hashing` order/hash funs structurally per the C
  (`utils.c:2149-2210`).

## Handoffs (owned by other nodes — NOT yet adopted)
- **Fun literals materialize and are callable; a compiled-module `Local` closure's ETF image is
  wired.** `beam_codec` decodes `EXPORT_EXT`/`NEW_FUN_EXT` into carriable terms readable through
  `fun_kind_term`/`fun_module`/`fun_function`/`fun_arity_term`/`fun_env`; fun application is
  `beam_engine::ExecutionContext::apply_fun`, implemented by `beam_rt` (`context::FunBridge`).
  `beam_codec` encodes a `Local` (`make_fun3`) fun only when the caller supplies a `FunIdentity`
  (module / 16-byte `uniq` / old_index / old_uniq / creator) via `Encoder::with_fun_identity`; without
  it the encode returns the typed `error::unsupported_local_fun` (the inlined descriptor carries no
  module/MD5). `beam_rt` installs the `beam_bifs` `LocalFunIdentity` seam, so a **compiled module's**
  local fun encodes byte-faithfully; only the `-eval` `erl_eval` template closure stays unclosable
  (see Known Structural Debt).
- `beam_codec`: HAMT (>32-key) maps encode via `beam_term::map::map_pairs_storage_order` (byte-identical
  to the oracle) and `src/term/{binary,record}.rs` are thin forwarders over `beam_term`. Only its private
  `write_large_flatmap` (an oversized *flat*-map writer with no LCA producer, since `map::write_map`
  switches to a HAMT past 32 pairs) stays crate-local.
- `beam_engine` / `beam_bifs`: `maps:*` on large maps via `map::{map_get,map_size,map_pairs,map_put,map_remove}`.
- `beam_bifs`: its `ets:info/1` bridge (`src/ets.rs`) emits the oracle's full 15-field order
  (hand-reproduced from public `beam_ets::TableInfo` fields — `TableInfo::entries` needs `&mut AtomTable`,
  which a BIF context lacks); `owner` is still rendered as `undefined` (no pid term yet).
- `beam_process` owns the off-heap refc-binary sweep: it registers owned `RefcBinary` payloads in
  `beam_gc::OffHeap` and sweeps them at the end of a rooted collection with a reachability half
  (`beam_gc::AnyCollector::for_each_reachable_object` over the roots the collection just gathered)
  plus the declared-dead half; a process with unpublished roots declines (retains everything).
- **A live `code_server` process is not yet on the receive path.** `beam_rt` serves `code:*` / `code_server`
  requests from its native floor (`src/native.rs`), not from a registered `code_server` process, because
  OTP's real `code_server.beam` needs host facilities outside `beam_rt` (a host timer for `after T`,
  `erlang:make_ref/0`, `spawn_link`, `persistent_term`/`erl_prim_loader`, named-ETS resolution). The
  `{call, From, Request}` → reply wire a future cross-crate live server must reproduce is documented in
  `beam_rt/CONTEXT.md`; the three `boot/*` differential cases are shape-only and already agree.

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
- `beam_term/` → frozen tagged term representation, immediates, boxed terms, atom table, literal areas; `map` (HAMT builder), `bitstring`, `refc`, `fun` (local/export/imported fun interior) (frozen, Stage 0)
- `beam_alloc/` → allocators (`erts_alloc` replacement): carriers, size classes, process heap/bump allocator; `RefCount`, `RefcBinary` (off-heap refcounted binary); `tests/` (the allocator integration suite)
- `beam_abi/` → NIF C ABI (`enif_*`) + driver C ABI (`driver_*`/`erl_drv_*`), layout-frozen structs; the **working host API** (`nif/enif.rs` — the `enif_*` function implementations over the `NifHost` seam), the **dynamic loader** (`nif/loader.rs` — `dlopen`/`dlsym` of a NIF shared object + `ErlNifEntry` validation + `NifLibrary::call`), and `nif/host.rs` (`NifHost`/`NifEnv`)
- `beam_codec/` → External Term Format (ETF) encode/decode, `VERSION_MAGIC 131`
- `beam_dist/` → distribution protocol v6 (`DFLAG_*`/`DOP_*`), handshake, `epmd`
- `beam_loader/` → `.beam` parsing, loader, `ops.tab` transforms, typed block-structured IR
- `beam_engine/` → typed IR execution, direct-threaded interpreter, BeamAsm + Cranelift JIT tiers, `ExecutionContext` + `Mailbox` (receive/message) seams, the block/suspend outcome; companion docs `TEST_NOTES.md` (measurement discipline) and `HANDOFF_bs_init_writable.md`
- `beam_gc/` → per-process generational copying GC, `OffHeap` (MSO + refc binaries) sweep, heap sizing (Stage 4); copies fun blocks by the `beam_term::fun` descriptor layout
- `beam_message/` → copy engine (`copy_struct`) + messaging (`Message`/`Queue` + the `QueueCursor` scan position)
- `beam_process/` → processes, scheduler, run queues, thread progress, signals/links/monitors, resume-on-send (`DeliverOutcome`); sub-nodes `src/smp/` and `tests/`
- `beam_bifs/` → BIF registry (`bif.tab` surface) + implementations + the fifteen host seams (`OutputSink`/`SendHook`/`AtomInterner`/`FunCaller`/`PortHook`/`DictHook`/`PersistentTerm`/`NifLoader`/`RecordSource`/`LocalNode`/`LocalFunIdentity`/`ModuleInfo`/`ProcessHook`/`MemoryHook`/`RuntimeHooks`)
- `beam_ets/` → ETS tables (set/ordered_set/bag/duplicate_bag): `set` defaults to a CHAMP interior, `ordered_set` to a K-ary B+-tree, `bag`/`duplicate_bag` to a chained bucket index; `ETS_SURFACE.md` (the ETS API-surface reference)
- `beam_io/` → poller/reactor (epoll/kqueue/io_uring), ports, port-task scheduling, built-in drivers (`fd`/`efile` + the OS-child `spawn` driver)
- `beam_rt/` → runtime binary: boot/preloaded modules, CLI flags, `system_info`, glue, vertical slice, the executor runner (send/receive over the process mailbox + scheduler), the port subsystem (drives `beam_io::PortRegistry` + delivers port data to the owning process), and the `PortHook` provider; sub-nodes `src/boot/`, `src/cli/`, `src/info/`, `src/native/`, `src/port/`
- `../tools/differential/` → differential-test harness vs real ERTS (sibling — read-only, escalate writes to `..`)
- `../tools/bench/` → benchmark driver for external targets + system-ERTS baseline (sibling — read-only, escalate writes to `..`)
- `../../erts/emulator/beam/` → legacy C core: ground-truth behavior/ABI to reproduce (read-only reference)

## Status
- **All 15 crates architected, implemented and tested.** `scripts/cargo.sh build --manifest-path
  erts_rust/Cargo.toml --workspace --all-targets` is warning-free and `scripts/cargo.sh test
  --manifest-path erts_rust/Cargo.toml --workspace --no-fail-fast` is green (**8756+ passed / 0
  failed**; the exact count moves as tests are added — re-run the command for the current figure).
  **CI exists:** `.github/workflows/erts_rust.yml` (triggered on any `erts_rust/**` change or PR)
  builds the workspace, runs this test command, runs a non-blocking clippy, and runs the differential
  gate — whose exit-3 "capability gaps, no mismatch" verdict is treated as non-blocking.
- **Real-module LOAD coverage is complete for the ENTIRE installed OTP:** `beam_loader`'s
  `tests/real_modules.rs` and `beam_engine`'s `tests/stdlib_sweep.rs` discover the lib root generically
  (`code:lib_dir()`, no hardcoded app names) and assert that **all 1366 `.beam` across 34 application
  directories** load, with no allowlist and **zero failures**.
- **Engine execution breadth reaches beyond stdlib/kernel/compiler:** `beam_engine`'s `tests/apps_lists.rs`
  runs oracle-cross-checked functions from parsetools (yecc/leex), syntax_tools, xmerl, inets, edoc, eunit,
  dialyzer, asn1 and snmp; `tests/census.rs` reaches **1366/1366 modules / 2,888,257 instructions with
  zero unmodelled opcodes** (the 13 unused `Instr` variants are the seven native-record variants — an
  OTP-29 feature this toolchain does not enable — plus `Deallocate` (every corpus `deallocate` is
  fused into `DeallocateReturn` by the loader), `BsTestTail2`, `ExecutableLine`, `DebugLine`,
  `SetTupleElement` (the corpus lowers no in-place tuple write: `setelement/3` builds a fresh tuple) and
  the `Generic` sentinel).
- **Vertical slice WORKS end-to-end**: `beam_loader` + `beam_engine` + `beam_rt` load a real `.beam`
  (fixture `beam_loader/tests/fixtures/slice.beam`, built with the system `erlc`) and execute it:
  `beam_rt run <slice.beam> slice:add/2 2 3` → `5`; `slice:fib/1 10` → `55`. The interpreter's
  supported opcode/operand subset is documented honestly in `beam_engine/CONTEXT.md` (the `Generic`
  fallback returns a typed error, never a wrong result).
- **Receive/message execution path WORKS end-to-end.** `beam_engine` executes the whole receive family
  (`send`/`loop_rec`/`loop_rec_end`/`wait`/`wait_timeout`/`timeout`/`remove_message`/`recv_marker_*`)
  over the `Mailbox` seam (the compiled `TestKind` machinery does the matching); `beam_process`
  scheduling wakes a receiver blocked in `receive` on send; `beam_rt` runs a real `.beam` fixture with
  `send`/`receive` through the real runtime glue and a single-threaded scheduler (`self() ! X, receive
  Y -> Y end` → `X`; a blocked receiver resumed by a sender → `42`). **The port subsystem is consolidated onto `beam_io`:** `beam_rt` owns one long-lived `beam_io::Poller`
  + `PortRegistry`, spawns OS children through `beam_io`'s `SpawnDriver`, runs the port-task round from its
  main loop, and turns the resulting `PortNotification`s into `{Port,{data,Data}}`/`{Port,{exit_status,…}}`
  messages **delivered to the owning process**, so a receiver blocked on port data is woken through
  `beam_process`'s resume-on-send rather than only by an explicit `send` (`beam_rt/tests/port_wake.rs`);
  `erlang:port_close/1` returns the oracle's `true`. The **differential gate reports 0 mismatches**:
  `summary: total=2497 agree=2495 mismatch=0 unsupported=2 skipped=0 allowed=2` (`../tools/differential/gate.sh`
  → exit 0); re-run `gate.sh` for the live per-contract split. The two recorded capability gaps (the
  `known-gaps.txt` ratchet allowlist) are A.9 `wire/new_fun_ext_encode` and
  `boot_extra/init_script_id_names_the_running_otp_release` (the `-eval`/`run` path loads no boot script).
  The one unclosable by design is
  `wire/new_fun_ext_encode` (A.5): its
  `-eval` `fun() -> ok end` is an `erl_eval` *template* closure (the arity-selected `-expr/6-fun-7-`
  slot) whose single free variable is erl_eval's internal `Info` tuple holding the clause in `erl_parse`
  abstract format — an identity/env no fun-identity seam can derive faithfully (only a full
  `erl_eval`/`erl_parse` port would; fabricating erl_eval internals is forbidden), so it is
  **unclosable by design**. A compiled module's local (`make_fun3`) funs encode byte-faithfully through
  the `beam_bifs`/`beam_rt` `LocalFunIdentity` seam (`term_to_binary/1` of such a closure).
- **Term/heap foundation complete** (see the section above): HAMT/CHAMP map builder, public
  bitstring/heap-bits APIs, off-heap refc binaries — with an `erl` oracle cross-check
  (`beam_term/tests/oracle.rs`) and GC copy-correctness evidence for HAMT maps + refc binaries
  (`beam_gc/tests/{map_copy,refc_binary_copy}.rs`).
- **Additive `beam_term` foundation extensions** (the term-layout LCA; no frozen constant/ABI
  changed): `From<TermError> for BeamError` (error interop); `compare`/`hashing`/`size` utilities
  (term ordering, structural hash, `size_object`); `thing` — heap-agnostic off-heap term
  materialization (float/big/flatmap/external pid/port/ref), which `beam_codec::term` forwards
  to (de-duplication). Writers take a caller-provided `*mut Eterm`; allocation stays in `beam_alloc`.
- **Stage-1 execution tiers shipped (`beam_engine`).** The typed-IR seam carries a direct-threaded interpreter plus a real native baseline tier (`src/jit/`): `src/jit/baseline/` lowers each IR basic block to an x86-64 `extern "C"` function (block ABI returns `(func<<32|pc)`, `HALT=-1`, `ERROR=-2`; `rbx` = x-register array base) with a driver loop that deopts to the interpreter's own `step_instruction` for anything it does not inline. A cross-run compiled-block cache (`src/jit/cache.rs`) and a block linker (`src/jit/baseline/emit/link.rs`) reuse and chain compiled blocks, and a local tail call (`call_last`/`call_only`) is linked natively. `tests/jit_real_module.rs` is the real-`erlc`-module tier-equivalence gate — **736/736** oracle-checked comparisons agree with zero divergences. An optional Cranelift hot tier (`src/jit/cranelift/`) sits behind the default-OFF `jit-cranelift` feature. Correctness (interpreter-identical results) is the deliverable. The native baseline tier is the **default for every `beam_rt` process entry**, compiled lazily **per function** (not whole-module at first entry); the only kill switch is the environment variable `BEAM_RT_TIER=interpreter`. No per-module switch-safety predicate gates the JIT — the engine's in-machine module-context switch (`Step::Enter`) is implemented and driver-mediated, so a module containing a cross-module `call_ext*`/`apply` runs natively too. Deeper inlining is later work.
- **BIF surface complete (`beam_bifs`).** The registry covers the whole declared `bif.tab` surface —
  **556 declared BIFs (registry 640 keys = 556 declared + 84 non-table keys: 79 stdlib fast paths +
  `erlang:memory/0,1` + the OTP-30 `pid_to_binary`/`ref_to_binary`/`port_to_binary`)** — via two defaulted seams (`RuntimeHooks::call`, `MemoryHook::memory`).
  Registered-but-unadopted BIFs return the typed `unsupported_bif` until a host hook is installed, so
  the runtime-coupled families are registered here while their behaviour remains a host gap.
- **Data-structure and runtime defaults.** `beam_ets`: `set` → CHAMP, `ordered_set` → K-ary B+-tree
  (sorted interior retained and selectable), `ordered_set` keys compared by arithmetic order
  (`1 == 1.0`). `beam_gc`: generational Cheney copying + an immix mature generation, auto-selected at/
  above `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS = 2048` (assertion-backed), with an additive
  on-demand growth seam (`Collector::ensure_capacity`, `HeapPolicy::unbounded()`, `GcConfig::growable()`).
  `beam_dist`: all 30 `Dop` variants round-trip byte-exact (30 golden vectors cross-checked against the
  oracle), with LIVE interop for the `'N'` handshake, `DOP_REG_SEND`, `DOP_SEND_SENDER`, client-originated
  `DOP_MONITOR_P` → `DOP_PAYLOAD_MONITOR_P_EXIT`, `DOP_EXIT2`, `DOP_UNLINK_ID`/`_ACK` and fragmentation
  both ways; `epmd` `DUMP`/`KILL`/`STOP` are implemented (node-up/down are not v6 opcodes). `beam_rt`:
  boot/CLI/env, `system_info`/`process_info`, a native BIF floor, and its own observability modules
  (`src/trace.rs`/`src/seq_trace.rs`/`src/crash_dump.rs`/`src/perf_map.rs` — only `send` trace events are
  emitted and the perf map is empty because no code ranges cross the crate boundary yet); inbound
  distribution runs by default (`allow_inbound_delivery`, `true` unless turned off) and routes the
  `Send`/`RegSend`/`AliasSend`/plain-`AltactSigSend` families into the local signal path plus the
  link/exit/unlink/monitor/demonitor/monitor-down/group-leader families through `beam_process`'s
  route-free receipt primitives, with the distributed-spawn family and `Delivery::Other(_)` still
  typed `Unsupported`; `DEFAULT_HEAP_WORDS = 1<<24`, with
  heap exhaustion translated to `error:system_limit`; the parked root set publishes only the live
  float-register prefix (`Machine::fr_mark()`), the pooled BIF heap is rewound to each call's own mark
  (`Heap::reset_to(mark)`), and `process_info(P, binary)` is served from the target's own
  off-heap-binary roster (pid-less `Runtime::eval_off_heap` payloads stay unattributed). The
  registered-BIF dispatch hot path is allocation-free (the shared `AtomTable` is only committed when a
  call actually interns a name; the `term_to_binary` fun-identity bridge reuses the code server's
  generation-cached module snapshot); dispatch/throughput figures and their measurement caveats live
  in `../tools/bench/RESULTS.md`.
- **Dependency edges beyond the scaffold:** `beam_message → beam_gc`, `beam_dist → beam_alloc`,
  **`beam_engine → beam_alloc`** (wave 0 → wave 4, directionally legal — for the off-heap refc binaries
  `bs_init_writable`/`append` hold); `RefCount` moved down into `beam_alloc` (the LCA) and re-exported
  by `beam_gc`.
- **Term-carrying structured raise reasons (coordinated across three crates).** `beam_common::Raise`
  gained an **opaque live-reason slot** — `pub live_reason: Option<LiveReason>`, `Raise::with_live_reason`/
  `raise.live_reason()`, `LiveReason::new<T: Any + Send + Sync + 'static>` + `downcast_ref<T>()` — whose
  payload convention is the **live reason term as a `beam_term::Eterm`**. `ExitReason` is unchanged
  (`ExitReason::Structured` stays a unit variant, so `as_atom()` is still `None`; the slot is invisible to
  `Debug`/`PartialEq`). `beam_engine` populates it on **every** raise (`Machine::throw_with_trace` attaches
  the term on an uncaught raise; `Machine::raise_bif_error` lands a BIF raise that carries a term in a
  live catch frame, matching C's `handle_error` → `next_catch`). `beam_bifs` attaches the true reason term
  instead of collapsing to a bare `badarg`: `{badmap,M}`, `{badkey,K}`, `{case_clause,V}`,
  `{badrecord,T}`, `{badfield,{{M,N},K}}`, and any **non-atom** `erlang:error/1,2`/`exit/1`/`throw/1`/
  `nif_error/1,2` argument (an atom reason stays `ExitReason::Atom` with no slot). The one remaining
  degradation is a tag atom that cannot be interned (no `AtomInterner` hook) ⇒ plain `badarg`.
  `beam_rt::runner::exit_reason` recovers the live reason term from the slot and renders it as the
  uncaught-exception reason.
- **Failed head-clause/guard terminates via `func_info`; a missing module/function is bare `undef`.**
  `beam_engine`'s `Instr::FuncInfo` (`src/interpreter.rs`) raises class `error` with the **bare atom
  `function_clause`** (resolved from the running module's atom table; a module whose table lacks the
  atom yields a typed `unsupported`, never a substituted wrong atom), matching C `i_func_info_IaaI` —
  treating it as a no-op would let a failed head clause fall through to the entry label and loop.
  `beam_rt::boot::CORE_ATOMS` seeds `function_clause`/`exit`/`throw` so a fixture module lacking the
  atom still resolves it; a call to a missing module/function yields the bare atom `undef`.
- **Host seams and BIF-level oracle parity.** The process dictionary is wired end-to-end — `beam_bifs`
  adds a 7th optional `BifContext` seam `DictHook` (`put`/`get`/`erase`/`get_keys`/`get_keys_for_value`)
  backing `erlang:put/2`/`get/1`/`erase/1`/`get_keys/0,1`, and `beam_rt` supplies it
  (`context::DictBridge`). `term_to_binary/2` adopts `beam_codec`'s minor-version API
  (`MinorVersion::{V0,V1,V2}`, legacy `ATOM_EXT` 100 / ASCII `FLOAT_EXT` 99), byte-identical to the
  oracle. The port BIFs match the oracle: `port_info/1` = the 7-item set in oracle order, `port_info/2`
  = `{Item,Value}` (`badarg` for an unknown item), `port_command/3 [force]` ⇒ `error:notsup`,
  `open_port(_, [list])` ⇒ `badarg`. `-eval` map literals are stored term-order-sorted (the oracle's
  key order).
- **The OTP-29 native-record opcode family is fully modelled and executed.** `beam_loader` lowers all eight
  members (`is_any_native_record/2`, `is_native_record/4`, `get_record_elements/3`, `put_record/6`,
  `is_record_accessible/3`, `get_record_field/5`, `get_record_elements_id/4`, `update_record_id/7`) to
  typed `Instr` variants instead of `Instr::Generic`, and `beam_engine`'s records family
  (`families/records/{mod,raw,tests}.rs`) executes them — branching to the fail label on exactly the
  record-semantics failures the C branches on, and returning a typed `unsupported` only for a genuine
  capability gap. `put_record/6`'s **create** form (`Src == nil`) IS implemented for the running module's
  OWN records (`families/records/mod.rs` — the `create_native_record`/`create_local_native_record` route,
  built from the module's own `Recs` definition); the only remaining gap is a create from a **foreign**
  module's record id (`{Module,Name}` naming another module), which stays a typed `unsupported`.
- **The NIF subsystem (contract A.3) works end-to-end.** `beam_abi` implements a real `enif_*` host API
  (`src/nif/enif.rs`) over the host seam `NifHost`/`NifEnv` (`src/nif/host.rs`), which supplies term
  allocation/`intern_atom`/`atom_name`/`self_pid` and optional `term_to_binary`/`binary_to_term`/`make_ref`
  hooks — so `beam_abi` gains **no** dependency edge to `beam_codec` (the wave DAG stays acyclic). Coverage
  spans allocation (`enif_alloc`/`free`/`alloc_binary`/`release_binary`/`make_new_binary`), accessors
  (`enif_get_int/long/uint/ulong/int64/uint64/double/atom(+_len)/string/tuple/list_cell/list_length/
  map_size/map_value/binary`, `enif_is_*`), constructors (`enif_make_int/long/uint/ulong/int64/uint64/
  double/atom(+_len)/tuple(+_from_array)/list(_cell/_reverse/_from_array)/binary/string(+_len)/ref/pid/
  badarg/map_put/map_remove`), `enif_raise_exception`, `enif_compare`, `enif_has_pending_exception`,
  `enif_clear_env`. The **dynamic loader** (`src/nif/loader.rs`) uses `libc`'s `dlopen`/`dlsym`/`dlclose`
  (no new dependency) to read `nif_init`'s `ErlNifEntry`, validate `ERL_NIF_MAJOR_VERSION`/
  `ERL_NIF_MINOR_VERSION`, run the entry's `load` callback against a host `ErlNifEntry*`, and expose
  `NifLibrary::call`. `beam_bifs` adds the `erlang:load_nif/2` BIF + the optional `NifLoader`/`current_module`
  `BifContext` seams; `beam_rt` owns the `NifRegistry`/`NifBridge`/`NifHostImpl` and intercepts NIF calls at
  external `call_ext` dispatch. A C fixture (`beam_abi/tests/fixtures/test_nif.c`, built with the system
  `cc`; the test SKIPS LOUDLY if `cc`/`erl_nif.h`/`erlc` are absent) is loaded and called from interpreted
  code in `beam_rt/tests/nif.rs`; a version mismatch / missing `nif_init` yields a typed error, never a crash.
  `enif_*` C-variadic functions (make_tuple/list, `set_option`, the `*printf` family) are left `None` because
  stable Rust cannot define a C-variadic `fn` (rust-lang/rust#44930); the `_from_array` variants cover
  tuple/list building. `term_to_binary`/`binary_to_term`/`make_ref` remain defaulted `None` in `NifHostImpl`.

## Contract A.9 (build/bootstrapping) — status
A.9 is the shallowest contract: its only differential coverage is boot/version **observability** — there
is **no self-hosted build** (the Rust runtime does not compile and run itself).
- **Boot path — completes.** `beam_rt`'s boot (`src/boot/`, the boot node: `beam_rt/src/boot/CONTEXT.md`)
  reads and executes the real `bootstrap/bin/start.boot` (OTP 30 / erts-17.1, 22 instructions) **to
  completion**, ending at the final `{progress,started}` (index 21). The former stop at instruction 16
  (`{apply,{application,load,[{application,stdlib,…}]}}`, `exit noproc`) is closed: the `{kernelProcess,…}`
  servers survive their short-lived directive process via the unlink in `beam_rt/src/boot/drive.rs`, and
  the final `{apply,{c,erlangrc,[]}}` (index 20) is unblocked by the runtime-native `file_server_2` service
  (`beam_rt/src/native/file_server.rs`). Pinned by
  `beam_rt/tests/boot.rs::the_real_start_boot_runs_to_completion`,
  `beam_rt/src/boot/bootscript.rs::the_real_start_boot_completes_through_execute_reporting`, and the CLI
  test `beam_rt/tests/boot.rs::the_cli_runs_the_real_boot_script_to_completion` (exit 0, empty
  stdout/stderr). The differential case `boot/proc_lib_start_link_blocks_the_boot_path` now **agrees** (it
  is no longer in `tools/differential/known-gaps.txt`).
- **Remaining A.9 gaps (fidelity, not boot-completion):**
  1. **No self-hosted build.** The Rust runtime does not compile/run itself; the compiler bootstrap
     (Stage 2, `../lib/compiler`) is not wired into `beam_rt`.
  2. **No live `code_server`/`erl_prim_loader`/`application_controller` process** on a plain
     runtime. `code:*`, `init:*` and the `application:*` queries are served by a native floor
     (`beam_rt/src/native/{code,init,application}.rs`). On the `-boot` path the real
     `application_controller:start/1` IS spawned and stays registered (instruction 14).
  3. **A live `logger` process is registered.** The real `start.boot` leaves `logger_server` registered
     as `logger`, so a booted runtime has a live `logger` — `is_pid(erlang:whereis(logger))` is `true`,
     pinned by `crates/beam_rt/tests/boot.rs::the_cli_boot_registers_a_live_logger_process`.
     `error_logger` still has no process (only `error_logger:warning_map/0` via the `RuntimeHooks`
     bridge).
  4. **The native `file_server_2` is not the real Erlang `file_server`** — a mailbox-drained
     `beam_process::Process` (`beam_rt/src/native/file_server.rs`). The `{get_until,…}` and
     `{get_line,…}` families ARE served for regular files.
  5. **`init:restart/0` and `init:boot/1` are implemented** (`crates/beam_rt/src/native/init.rs`):
     `restart_0` performs a synchronous in-place reboot answering `ok`, and `boot_1` runs the script
     named by the boot flag; both are oracle-cross-checked. `init:stop/0,1` and `init:get_arguments/0`
     (including the automatic `root`/`bindir`/`progname`/`home`) are implemented and
     oracle-cross-checked.

## Known Structural Debt (candidates for the next pass)
- The map/bitstring/refc handoffs above are only partly adopted: `beam_codec` forwards to `beam_term`
  (`src/term/{binary,record}.rs`) but keeps a crate-local oversized-*flat*-map writer. Off-heap refc
  liveness is the process's: `beam_process` sweeps its owned payloads by reachability at the end of a
  rooted collection (plus the caller's declared-dead assertions).
- `wire/new_fun_ext_encode` remains `unsupported` *in the gate*: its `-eval` lambda is an `erl_eval`
  template closure whose identity/`num_free`/`env` are erl_eval internals no seam can derive faithfully.
  `beam_rt` installs `LocalFunIdentity`, so a **compiled module's** local fun encodes byte-exactly.
- Repo-root owned (outside this node): `../../CHANGE.md` documents most of the above (Milestones 1–3
  cover the frozen term layout, `copy_struct`, the fun/export literal layout + the `beam_message` fun
  sizing, the receive/message execution path, the NIF/driver symbol tables, and the `AtomTable`-clone
  fix). Entries still owed there by the repo root: the `beam_term` `map`/`bitstring`/`refc` foundation,
  the `RefCount` move into `beam_alloc` + the `RefcBinary` off-heap seam + `beam_gc::OffHeap` owning
  refc binaries, and the runtime parity work (`func_info`→`function_clause`, the `DictHook` process
  dictionary, the `{minor_version}` `term_to_binary/2` adoption, sorted `-eval` map literals).
## Design Decisions
- **Error interop is owned upstream, not duplicated.** `beam_term` already declares (but does not
  yet use) its `beam_common` dependency. The `From<beam_term::TermError> for beam_common::BeamError`
  conversion is added **in `beam_term`**, so all 13 dependents get `?`-interop for free instead of
  each writing a mapping (Prevent-Duplication). Purely additive; it is the "reconcile once
  `beam_common` lands" step `beam_term`'s own `CONTEXT.md` prescribes. (Needs a `../../CHANGE.md`
  note — flagged to the parent node, which owns the repo root.)
- **Engine depends only on `beam_loader`/`beam_alloc`.** It defines `ExecutionContext` (BIF dispatch /
  allocation / process state) and `Mailbox` (receive/message) as traits; `beam_rt` supplies the concrete
  implementations over `beam_bifs`/`beam_process`. This keeps the Stage-1 engine independent of the
  Stage-5 runtime core (acyclic DAG) and makes the engine testable with a minimal context. A receive
  that must block is reported by the engine's `RunOutcome` (not an error), so the runtime decides what
  "block" means without the engine depending on the scheduler.
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
- **Differential-corpus coverage:** the gate pins the native-record opcode family (`records` group), the
  `func_info` clause-failure path and bare-atom `undef` (`errors` group), the structured raise reasons
  (`{case_clause,_}`/`{badmap,_}`/`{badkey,_}`/`{bad_generator,_}`/`{try_clause,_}`/`{badrecord,_}`) and
  the port paths (`driver/open_port_*`, `port_info`, `port_command`). The two remaining shallow contracts
  are A.5 (the `wire` group covers the ETF codec only — a live handshake / `DOP_*` loopback needs two
  connected nodes and is not expressible through a single CLI process) and A.9 (boot/version
  observability only — see the A.9 status above).

## See Also
- `../CONTEXT.md` → workspace root: the binding cross-crate contracts and routing table.
- `../../ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and `../../ERTS_RUST_REWRITE_DEEPDIVE.md`
  (Part II) → design source of truth, incl. the Stage 0–6 plan (§7/§H).
- `../../CHANGE.md` → log of major structural/data-structure/algorithmic changes (repo-root owned).
- `../../erts/emulator/beam/CONTEXT.md` → the legacy core being ported.
