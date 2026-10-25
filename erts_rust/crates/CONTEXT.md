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
- `beam_common` → `next_ref_id() -> u64` (`src/refid.rs`): **the ONE local reference-id space** — a
  process-wide monotone `AtomicU64`, first draw `0`. Every locally minted reference *term* draws its
  `id` word from it: `erlang:make_ref/0`, the NIF `make_ref`, `erlang:monitor/2,3`, `alias/1`, timer and
  `seq_trace` references, `beam_process::RefAllocator` (a ZST handle over it — so `Runtime::refs` and
  `ProcessTable::refs` are one space), and NIF resource terms (`enif_make_resource`, narrowed to
  `MAX_SMALL` by `beam_abi`). **No crate may keep a private reference-id counter.** A reference's
  identity is the `id` word alone (`node`/`serial` are `0` in M1), so an overlapping space would make a
  **dead** NIF resource term word-indistinguishable from a live plain reference and an identity-matching
  reachability prune would retain it — pinned by `beam_rt/tests/nif_resource_nested.rs` (dropped resource
  legs answer `{10,0}` / `{200,0}`, the oracle's exact figures). `beam_process::MonitorRef::from_id` is the one
  **explicit-id, non-mint** constructor, for a *derived* id (the unnamed-ETS tid, whose id must be the
  table handle so the tid decodes back through `beam_bifs::access::ref_thing_id`); it draws nothing and
  its uniqueness is the caller's claim. Reference ids decoded from another node are outside this space.
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
  every method **defaulted** to a typed `unsupported`). `ExecutionContext` also carries the
  **defaulted case-lowering seams**: the in-machine module-context switch (`enter_module`), the
  local-fun resolver (`resolve_local_fun`), the runtime-atom name/VALUE accessors (`atom_name` +
  `atoms<'m>(&'m self, module: &'m Module) -> &'m AtomTable`), the reduction/`reduce` hook,
  `apply_fun`/`apply_target`, `resolve_external_call` and `current_module`. **`atoms` is the
  atom-namespace seam every guard/`select_val`/map-key comparison site reads** (trait default =
  `&module.atom_table`, the module's load-time snapshot), so a host that owns ONE live shared
  namespace (all of `beam_rt`, via `RuntimeContext` **and** the scheduled path's `BudgetContext`)
  makes atom ordering name-exact and identical to the comparison BIFs', instead of silently falling
  back to raw atom-index order when a name is missing from the snapshot
  (`beam_term/src/compare.rs::cmp_atoms`). The engine therefore depends only on
  `beam_loader`/`beam_alloc` and the concrete context is supplied by `beam_rt` (keeps the
  Stage-1/Stage-5 DAG acyclic). A receive that finds no message suspends through the engine's block
  outcome (`RunOutcome { Returned, Wait }` + `run_export_outcome`/`run_label_outcome`/`resume_outcome`;
  the plain `run_export` folds a suspend into the typed `error::blocked()`), which `beam_rt` maps onto
  `beam_process`'s scheduler.
- `beam_engine` (off-heap half) → the machine's reservoir of `RefcBinary` payload handles
  (`Machine::refc_binaries`) plus its two host seams: `Machine::off_heap_overhead_bytes` (Σ
  `RefcBinary::reserved_bytes`, the backing store `beam_gc::BinVheap` accounts in) and
  `Machine::retain_refc_binaries(predicate)` (prune by reachability; a retained payload's `data_ptr`
  is unchanged and the `writable_payloads` registry prunes in lockstep) — `src/machine/offheap.rs`.
  The predicate IS the caller's soundness assertion (a false reject frees memory a live `BinRef.val`
  names); a false accept is only a leak.
- `beam_process` (off-heap half) → `Process::set_off_heap_binary_bytes`/`off_heap_binary_bytes` (the
  host-asserted payload figure `max_heap_size`'s `include_shared_binaries` charges),
  `Process::owned_off_heap()` (the `HeapFootprint::of` handle), the chain-summed `heap_used_words()`,
  and the unlimited-setting early return in `record_max_heap_verdict` (no O(off-heap slots) walk per
  collection when no limit is set).
- `beam_ets` → `Table` handle + set/ordered_set/hash ops.
- `beam_gc` → per-process copying collector entry (`gc`/`collect`) + `OffHeap` refcount interface
  (`OffHeapEntry::{Mso, Binary}`); re-exports `beam_alloc::RefCount`.
- `beam_message` → `copy_struct` (term movement across heaps) + the `Message` envelope + the `Queue`
  FIFO with a resumable `QueueCursor` scan position (`cursor`/`message_at`/`advance`/`remove_at`) that
  the receive seam drives.
- `beam_process` → `Process`/`ProcessId`, `Scheduler`/run queue/`ProcessRunner`, signals/links/monitors;
  **resume-on-send** (`Process::deliver -> DeliverOutcome` + `ProcessTable::drain_woken` /
  `Scheduler::resume_woken`) wakes a receiver blocked in `receive`.
- `beam_rt` (off-heap half) → one `beam_gc::BinVheap` per process (`ProcessEngine.off_heap`,
  `src/context/gc/bin_vheap.rs`) and exactly ONE reclamation point: the runner's park arm
  (`src/runner/run.rs`), after the rooted collection — reachable payloads from
  `for_each_reachable_binary_payload`, then `Machine::retain_refc_binaries`, then
  `Process::set_off_heap_binary_bytes(machine.off_heap_overhead_bytes())`.
  The trigger is O(1) per slice (the overhead figure is re-derived only when the reservoir's handle
  count moves); a declined prune retains the whole reservoir (a leak, never a dangle).
- `beam_deflate` → zlib 1.3.2 in safe Rust: the one-shot `compress`/`decompress`/`decompress_bounded`/
  `decompress_bounded_used`, the streaming `Deflate`/`Inflate` types whose one-call pump
  (`deflate(input, output, flush) -> {status, consumed, produced}`) reproduces zlib's
  `avail_in`/`avail_out` accounting, `Flush`/`Strategy`/`Method`/`Status` (incl. `NeedDict(adler)`),
  `DeflateConfig`/`GzHeader`, and `checksum::{crc32, adler32, crc32_combine, adler32_combine}`.
  Bytes only — no terms, no host state; byte-identical to the oracle for levels `-1`/`0..=9`, all five
  strategies, windowBits `±8..=±15` and `16+9..=16+15`, memLevel `1..=9`, all six flush modes (a
  combination it does not cover is a **typed** `BeamError`, never a conforming approximation).
- `beam_bifs` → `BifRegistry` + `BifFn` + the registry generated from the `bif.tab` surface, plus
  **fifteen** optional defaulted host seams (`OutputSink`/`SendHook`/`AtomInterner`/`FunCaller`/
  `PortHook`/`DictHook`/`PersistentTerm`/`NifLoader`/`RecordSource`/`LocalNode`/`LocalFunIdentity`/
  `ModuleInfo`/`ProcessHook`/`MemoryHook`/`RuntimeHooks`). It also exposes the **local-call predicate**
  `serves_local_call(module, function, arity) -> bool` (`module == "zlib"` + the 19 `(name, arity)`
  rows of `zlib.erl`'s non-exported NIF stubs) — the seam `beam_rt`'s `resolve_local_call` consults
  for a module's own unexported `*_nif` stubs, alongside `prim_tty`'s. A `zlib` stub is reached **two**
  ways and both must be served: as a local `call`, and as a local `make_fun3` closure (`zlib.erl`
  stores `fun deflate_nif/4` in a `#zlib_opts{}` record), which `beam_rt::context::apply` declines
  through the same predicate and dispatches via `call_bif`.
- `beam_io` → the poller/reactor (`mio`-backed) + `Port`/`PortRegistry` + port-task scheduling + built-in drivers (`fd`/`efile`, and the OS-child `spawn` driver with its `Driver::take_exit_status` seam); plus the two additive seams a host needs to *reach* a built-in driver: `pub trait Driver: core::any::Any` with the one downcast path `dyn Driver::downcast_ref::<T>()` (safe, no `unsafe` fat-pointer work), surfaced as `Port::driver_as`/`PortRegistry::driver_as` — what lets `beam_rt` recover the concrete `TcpInetDriver` a port boxes and call `TcpInetDriver::from_accepted(fd, listener)` (the C's `tcp_inet_copy`) instead of replaying the option block — and `ControlReplyShape` (`Null`/`Buffer`) with `Driver::control_reply_shape`/`PortRegistry::port_control_reply`/`control_reply_shape`, because the C's `*rbuf == NULL` is `[]` for **every** flag word (`io.c:4134`, before the `PORT_CONTROL_FLAG_BINARY` split).
- `beam_rt` → the runtime **binary**: boot sequence, CLI flags, `system_info`, and the concrete
  `ExecutionContext` wiring engine + bifs + process — including the `Mailbox` receive seam over the
  process mailbox, a single-threaded `ProcessRunner`/scheduler loop, and the `beam_bifs::PortHook`
  provider over a spawn driver; it owns the end-to-end vertical slice.

## Cross-crate laws (BINDING — each was learned the hard way; do not re-open)

### Term lifetime and rooting
- A raw `Eterm` stored in a long-lived slot **roots nothing**. Any term that can outlive one instruction — a raise reason, a pending reply, a parked value, a cache entry, a stashed counter sample — must be copied onto a scanned heap or explicitly stabilized before its owner's heap can be relocated.
- A pointer-stable, never-scanned store (the runtime arena, `alloc_words`) is the wrong home for a container whose children live on a scanned young heap: the copier gives such a container `Target::Foreign` and never descends, so the GC leaves the container verbatim while relocating the children out from under it.
- When a heap-growth or move is *exempted* (`relocation_exempt` / `has_unpublished_roots() == true`), the correct fallback is **stabilize, never leave dangling** — a door that cannot copy must refuse, and the refusing path must first make its roots publishable.
- A lease (an `Rc` handle over a shared store) held across an allocation is a deep copy or a dangling hazard, never a cheap borrow.
- Per-site inventory and the "every site that can relocate a process heap" list: `beam_rt/src/runtime/CONTEXT.md`, `beam_rt/src/runtime/roots/CONTEXT.md`, `beam_rt/src/context/gc/CONTEXT.md`.

### The aliasing contract on both C ABI envs (`beam_abi`)
- An env handed to C takes `&self` accessors with interior mutability; **never** hold a Rust `&`/`&mut` to a host across a call into foreign (NIF/driver) code — `noalias` lets LLVM drop the foreign store, and only in `--release`.
- Therefore `NifEnv` is shared (`&NifEnv` at every signature, `Cell`/`Rc<RefCell<…>>` for per-call state), `NifHost` is `&self`-receiver only, and the aliasing guard `beam_abi/src/nif/host_aliasing_tests.rs` must be run in **both** profiles — the dev profile is `opt-level = 1`, so it is not a proxy for release.
- `NifHost: AsAnyHost` **cannot** be adopted: the runtime's per-call host borrows the runtime and so is not `'static`. `NifEnv::new_typed` + `host_as`/`with_host_as` is the sanctioned path for recovering a concrete host.

### Driver ports: the drive-window rule
- A port must not hold an atom-table lease outside a **drive window**, or the next intern deep-copies the whole table.
- `PortRegistry::open_driver_atom_view`/`close_driver_atom_view` bracket every call into driver code, depth-counted so a nested close can never release early; between windows a port's atom view is `None`, and `beam_abi`'s `NifEntry::host` is the one durable `Rc<AtomTable>` holder.
- The commit step tests `has_pending_driver_atoms()` **before** reaching `atoms_mut()`, so the transient view is dropped first and the shared table takes its append path.

### One MD5, in `beam_common`
- `beam_common::md5` (`digest`, incremental `Md5`) is the workspace's single RFC 1321 core, shared by `beam_loader`'s `.beam` chunk digests and `beam_bifs`' `erlang:md5/1`.
- No consumer may reintroduce a copy: a module's chunk digest is its loader identity, so two implementations that drift sample-by-sample would produce modules the runtime and the oracle disagree about.

### The baseline JIT's inline set and deopt idempotency
- 16 bitstring shapes are inlined natively under `beam_engine/src/jit/baseline/emit/bitstr_inline/`: 15 match-context shapes (`bs_get_position`, `bs_set_position`, `bs_test_tail2`, `bs_skip_bits2`, `bs_start_match3`, `bs_start_match4`, `bs_get_integer2`, a fully static `bs_match`, `bs_match_string`, the UTF-8/16/32 skip/get pairs) plus the `bs_create_bin` private-append construction shape.
- A shape that would **allocate or root inside the instruction** is deliberately DECLINED: its predicate answers `None`, the entry emits zero bytes, and `emit/block.rs` falls through to the family helper. No inline arm may take an `ExecutionContext`.
- **Deopt idempotency** is what makes the set safe: `jit_step_one` re-executes the *whole* instruction after a deopt, so no guard that can reach `fallback` may run against an already-mutated machine, and no arm may commit partial state before its last fallback.
- Every shape carries a deopt pin (`src/jit/baseline/bitstr_shapes_deopt_tests.rs`, asserting tier agreement plus a once-only side effect). Detail: `beam_engine/src/jit/baseline/emit/bitstr_inline/CONTEXT.md`.

## Layering & dependency waves (design order — dependents design against a finished parent API)
Determined by the Cargo workspace DAG; a crate may only use crates in earlier waves.
| Wave | Crates | Depends on |
|---|---|---|
| 0 | `beam_alloc`, `beam_abi`, `beam_deflate` | `beam_common`, `beam_term` (`beam_deflate`: `beam_common` only — it owns bytes, not terms) |
| 1 | `beam_codec`, `beam_ets`, `beam_gc` | wave 0 (`beam_codec` also `beam_deflate`) |
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
| `beam_codec` | ETF encode/decode of the core tag set + `VERSION_MAGIC 131` round-trips; the `COMPRESSED` (`80`) tag is **byte-identical to the oracle** over `beam_deflate` (wrapper `[131][80][body_len:4 BE][zlib stream]`, emitted iff `stream_len <= body_len - 5`, `body_len < 5` always plain, level 0 never compresses) |
| `beam_deflate` | zlib 1.3.2 deflate + inflate in safe Rust: one-shot and streaming (all six flush modes, per-call `avail_in`/`avail_out`), the five match functions + `configuration_table` (levels `-1`/`0..=9`), the `inftrees`/`inffast` inflate with zlib's status codes, `crc32`/`adler32` + `combine`, gzip/zlib/raw framings |
| `beam_dist` | `DFLAG_*`/`DOP_*` constants, handshake + control-message framing, `epmd` client |
| `beam_loader` | chunk parse + BEAM disassembly + lowering to the typed block IR |
| `beam_engine` | IR + direct-threaded interpreter + the native baseline JIT tier (default-OFF Cranelift hot tier) + `ExecutionContext` + the receive/message family over a `Mailbox` seam |
| `beam_gc` | generational Cheney copying + immix mature generation (measurement-gated) + MSO refcount |
| `beam_message` | `copy_struct` + send/receive envelope + the `QueueCursor` scan position |
| `beam_process` | `Process`, run queue, scheduler loop, resume-on-send, signals/links/monitors |
| `beam_bifs` | `BifRegistry` + the whole declared `bif.tab` surface (556 BIFs; registry 667 keys = 556 declared + 111 non-table keys, 25 of them `zlib`) + the host seams |
| `beam_ets` | `set` (CHAMP) + `ordered_set` (K-ary B+-tree) + `bag`/`duplicate_bag` (chained) over `new/insert/lookup/delete/select` |
| `beam_io` | `mio` poller/reactor + `Port`/`PortRegistry` + port-task scheduling + built-in `fd`/`efile` **and OS-child `spawn`** drivers |
| `beam_rt` | boot/CLI/`system_info` + the executor runner (send/receive) + the `PortHook` provider + end-to-end vertical-slice integration test |

## Term/heap foundation: maps · bitstrings · refc binaries (current state)
Frozen inter-crate contract — all term-layout writers are **heap-agnostic** (`beam_term` cannot allocate;
the caller reserves the words and passes `hp: *mut Eterm`). Per-item detail in `beam_term/CONTEXT.md`.
- **Maps** (`beam_term::map`) — flat map ↔ HAMT/CHAMP at `MAP_SMALL_MAP_LIMIT = 32` (`erl_map.c`): `write_map`
  (dedup last-wins, canonical order), `map_words_capacity(n)`, `map_size`/`map_get`/`map_pairs`(key-ordered)/
  `map_put`/`map_remove`; `compare`/`hashing`/`size` are structural over maps.
- **Bitstrings** (`beam_term::bitstring`) — `heap_bits_size_words`, `bit_count_to_bytes`, `write_heap_bits`,
  `write_binary`, `write_sub_bits`/`sub_bits_parts`, public `bitstring_parts`/`bit_at`/`float_bits`/
  `float_value`; non-byte-aligned supported.
- **Refc binaries** (`beam_term::refc`) — bit-exact on-heap `BinRef` (3 words) + `ErlSubBits` (5 words) pair,
  root at the SubBits; `ERL_ONHEAP_BINARY_LIMIT = 64` bytes / `ERL_ONHEAP_BITS_LIMIT = 512` bits; off-heap
  payload = `beam_alloc::RefcBinary` (refcounted, `AllocType::Binary`); **`BinRef.val` ==
  `RefcBinary::data_ptr()`**. Small binaries stay on-heap (`ErlHeapBits`).
- **Funs / exports** (`beam_term::fun`) — the on-heap `ErlFunThing` interior as heap-agnostic writers/readers.
  Header word = `FUN_SUBTAG | arity<<8 | num_free<<16 | external<<24 | imported<<25`; three kinds — `Local`
  (descriptor `[uniq:u64][index:u32]`), `External` (`[module][function][arity]`, an `EXPORT_EXT` literal),
  `Imported` (10-word descriptor: module, 16-byte MD5, index, old_index, old_uniq, creator pid — a
  `NEW_FUN_EXT` literal). `env` follows the descriptor; block = `1 + desc_words(kind) + num_free`, with
  `desc_words` = 2 / 3 / 10. The C's out-of-line `ErlFunEntry`/`Export` `entry` pointer is **inlined into the
  term** (the documented M1 substitution). `beam_gc` sizes/scans copies through `fun_block_words`/
  `fun_env_offset`; `compare`/`hashing` order/hash funs structurally per the C (`utils.c:2149-2210`).

## Handoffs (owned by other nodes — NOT yet adopted)
- **Funs materialize, are callable and ETF-encode.** `beam_codec` decodes/encodes `EXPORT_EXT`/`NEW_FUN_EXT`
  and encodes a `Local` (`make_fun3`) fun only with a caller-supplied `FunIdentity`
  (`Encoder::with_fun_identity`), else the typed `error::unsupported_local_fun`. `beam_rt` installs the
  `beam_bifs` `LocalFunIdentity` seam, so a **compiled module's** local fun encodes byte-faithfully (only the
  `-eval` `erl_eval` template closure stays unclosable). Application is `ExecutionContext::apply_fun` ←
  `beam_rt::context::FunBridge`. Detail: `beam_codec/CONTEXT.md`.
- `beam_codec` forwards HAMT maps (`beam_term::map::map_pairs_storage_order`) and `src/term/{binary,record}.rs`
  to `beam_term`; only its private oversized-*flat*-map writer stays crate-local. `beam_engine`/`beam_bifs`
  reach `maps:*` on large maps through `beam_term::map`.
- `beam_bifs`'s `ets:info/1` bridge emits the oracle's full 15-field order; `owner` renders as `undefined`.
- Off-heap refc-binary liveness is the owning process's: `beam_process` sweeps its owned payloads by
  reachability at the end of a rooted collection (plus the caller's declared-dead assertions); a process with
  unpublished roots declines and retains.
- **No live `code_server` process.** `code:*`/`code_server` requests are served by `beam_rt`'s native floor
  (`src/native/`); the `{call,From,Request}` → reply wire a future live server must reproduce is documented in
  `beam_rt/CONTEXT.md`. The three `boot/*` differential cases are shape-only and already agree.

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
- **Measurement pins must express an invariant, not a magic constant.** An exact instruction/word/reduction
  count is legitimate only where the quantity is genuinely deterministic; where it is load-, geometry- or
  cadence-dependent, assert the *relation* the number was standing in for (a band, a ratio between two
  measured quantities, or an in-code derivation such as `slices == ceil(reductions / SLICE_BUDGET)`), so the
  suite cannot go red on a busy host. A pin that has to be re-typed after every legitimate scheduling change
  is a bad test, and "loosening it until it cannot fail" is not the fix either.
- Host load is not a result: read `/proc/loadavg` before and after a measurement run and prefer paired,
  interleaved sampling over absolute wall time (this host is shared).
- **Current state: every crate suite is green** (unit + integration + the oracle-cross-checked suites), and
  the workspace builds warning-free. Per-crate counts live in each crate's own `CONTEXT.md` — do not pin an
  aggregate total here, and check `git log` for the latest figure rather than trusting a copied one.

## Routing Table
- `beam_common/` → foundation: `BeamError`/`Result`, version constants, config/feature flags, logging, `unsafe` convention (frozen, Stage 0)
- `beam_term/` → frozen tagged term representation, immediates, boxed terms, atom table, literal areas; `map` (HAMT builder), `bitstring`, `refc`, `fun` (local/export/imported fun interior) (frozen, Stage 0)
- `beam_alloc/` → allocators (`erts_alloc` replacement): carriers, size classes, process heap/bump allocator; `RefCount`, `RefcBinary` (off-heap refcounted binary); `tests/` (the allocator integration suite)
- `beam_abi/` → NIF C ABI (`enif_*`) + driver C ABI (`driver_*`/`erl_drv_*`), layout-frozen structs; the **working host API** (`nif/enif.rs` — the `enif_*` function implementations over the `NifHost` seam), the **dynamic loader** (`nif/loader.rs` — `dlopen`/`dlsym` of a NIF shared object + `ErlNifEntry` validation + `NifLibrary::call`), `nif/host.rs` (`NifHost`/`NifEnv`), and the three additional seams: `nif/enif/schedule.rs` (`enif_schedule_nif`'s `Outcome` + iterative constant-stack continuation trampoline, `NifHost::nif_yield_point`), `nif/thread_env.rs` (`ThreadEnvHostFactory` / `install_thread_env_factory` — the off-call-path `enif_alloc_env`), and `nif/enif/{resources_stop.rs,stop.rs}` (`call_resource_stop` / the pending-stop table behind `enif_select`'s STOP verdicts) (`nif/host.rs` is `&self`-receiver only, and `nif/host_aliasing_tests.rs` is the optimised-profile aliasing guard)
- `beam_deflate/` → zlib 1.3.2 in safe Rust (bytes only, `beam_common` its sole dependency): `deflate/` (window + hash chains, the five match functions, Huffman construction, the flush driver), `inflate/` (bit reader, `inftrees`/`inffast`, header/trailer state machine, dictionary protocol), `checksum.rs` (`crc32`/`adler32` + `combine`); the one shared encoder behind both `beam_codec`'s ETF `COMPRESSED` and `beam_bifs`'s `zlib` BIFs
- `beam_codec/` → External Term Format (ETF) encode/decode, `VERSION_MAGIC 131`, the `COMPRESSED` wrapper over `beam_deflate`, the public `ref_wire` reference-id ↔ wire-words mapping (the C's `erts_set_ref_numbers`), and the decode-side local-node demotion (`Decoder::with_local_node`: a pid/port/ref naming this node decodes internal)
- `beam_dist/` → distribution protocol v6 (`DFLAG_*`/`DOP_*`), handshake, `epmd`, and the defaulted inbound decode-side identity (`control::LocalIdentity` + `decode_control_with_identity`, `Connection`/`ConnectionDriver::set_local_identity`)
- `beam_loader/` → `.beam` parsing, loader, `ops.tab` transforms, typed block-structured IR
- `beam_engine/` → typed IR execution, direct-threaded interpreter, BeamAsm + Cranelift JIT tiers, `ExecutionContext` + `Mailbox` (receive/message) seams, the block/suspend outcome; companion docs `TEST_NOTES.md` (measurement discipline) and `HANDOFF_bs_init_writable.md`
- `beam_gc/` → per-process generational copying GC, `OffHeap` (MSO + refc binaries) sweep, heap sizing (Stage 4); copies fun blocks by the `beam_term::fun` descriptor layout
- `beam_message/` → copy engine (`copy_struct`) + messaging (`Message`/`Queue` + the `QueueCursor` scan position)
- `beam_process/` → processes, scheduler, run queues, thread progress, signals/links/monitors, resume-on-send (`DeliverOutcome`), and the remote (`DistSignalRouter`/`DistSignal`, seven variants incl. the death-time `MonitorExit`) + inbound route-free receipt halves of the distribution signal family; sub-nodes `src/smp/` and `tests/`
- `beam_bifs/` → BIF registry (`bif.tab` surface) + implementations + the fifteen host seams (`OutputSink`/`SendHook`/`AtomInterner`/`FunCaller`/`PortHook`/`DictHook`/`PersistentTerm`/`NifLoader`/`RecordSource`/`LocalNode`/`LocalFunIdentity`/`ModuleInfo`/`ProcessHook`/`MemoryHook`/`RuntimeHooks`); sub-nodes `src/zlib/` (the six one-shots + the 19 stream NIF keys over `beam_deflate`), and the `serves_local_call` predicate `beam_rt` consults for a module's own unexported `*_nif` stubs
- `beam_ets/` → ETS tables (set/ordered_set/bag/duplicate_bag): `set` defaults to a CHAMP interior, `ordered_set` to a K-ary B+-tree, `bag`/`duplicate_bag` to a chained bucket index; `ETS_SURFACE.md` (the ETS API-surface reference)
- `beam_io/` → poller/reactor (epoll/kqueue/io_uring), ports, port-task scheduling, built-in drivers (`fd`/`efile` + the OS-child `spawn` driver)
- `beam_rt/` → runtime binary: boot/preloaded modules, CLI flags, `system_info`, glue, vertical slice, the executor runner (send/receive over the process mailbox + scheduler), the port subsystem (drives `beam_io::PortRegistry` + delivers port data to the owning process), the `PortHook` provider, the distribution halves (`src/dist/` accepting, `src/dist_client/` connecting, one `DistLinks` table over both) and their two-live-VM suites (`tests/dist_two_vm{,_rust,_wire}.rs` + the `tests/support/{dist,pair,fixture}.rs` harness); sub-nodes `src/boot/`, `src/cli/`, `src/info/`, `src/native/`, `src/port/`
- `../tools/differential/` → differential-test harness vs real ERTS (sibling — read-only, escalate writes to `..`)
- `../tools/bench/` → benchmark driver for external targets + system-ERTS baseline (sibling — read-only, escalate writes to `..`)
- `../../erts/emulator/beam/` → legacy C core: ground-truth behavior/ABI to reproduce (read-only reference)

## Status
- **All 15 crates architected, implemented and tested.** The workspace build is warning-free and the test suite
  is green — re-run `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --workspace
  --no-fail-fast` for the current count. CI: `.github/workflows/erts_rust.yml` builds, tests, runs a
  non-blocking clippy and runs `../tools/differential/gate.sh` (exit-3 "capability gaps, no mismatch" is
  non-blocking).
- **Real-module load coverage: all 1366 `.beam` across 34 application directories**, no allowlist, zero
  failures (`beam_loader/tests/real_modules.rs`, `beam_engine/tests/stdlib_sweep.rs`).
- **Engine breadth:** oracle-cross-checked execution well beyond stdlib/kernel/compiler
  (`beam_engine/tests/apps_lists.rs`); `tests/census.rs` reaches 1366/1366 modules / 2,888,257 instructions
  with zero unmodelled opcodes (`beam_engine/CONTEXT.md` lists the unused `Instr` variants).
- **Vertical slice works end-to-end:** `beam_rt run <slice.beam> slice:add/2 2 3` → `5`; `slice:fib/1 10` →
  `55`. The supported opcode subset is documented in `beam_engine/CONTEXT.md` (the `Generic` fallback is a
  typed error, never a wrong result).
- **Receive/message path works end-to-end.** The whole receive family (`send`/`loop_rec`/`loop_rec_end`/
  `wait`/`wait_timeout`/`timeout`/`remove_message`/`recv_marker_*`) runs over the `Mailbox` seam (`TestKind`
  matching), `beam_process` wakes a receiver blocked in `receive` on send, and a port-data wake reaches the
  owner through the same resume-on-send. Ports are consolidated onto `beam_io`: one long-lived `Poller` +
  `PortRegistry`, `SpawnDriver`, the port-task round in the runtime's main loop, `PortNotification`s turned
  into `{Port,{data,…}}`/`{Port,{exit_status,…}}` for the owning process. Detail: `beam_io/CONTEXT.md`,
  `beam_rt/CONTEXT.md`.
- **A.4 driver/port surface:** the `driver_output` term shape (`{Port,{data,…}}` + `port_info(P,input)`); the
  static `{spawn_driver,Name}` namespace is the C's `driver_tab` (`beam_io::STATIC_SPAWN_DRIVERS` =
  `tcp_inet`/`udp_inet`/`ram_file_drv`, `fd`/`efile` refused as `badarg`); `port_control/3` reaches the
  numeric `INET_REQ_*`/`TCP_REQ_*`/`UDP_REQ_*`/`PACKET_REQ_*` opcodes as well as the named verbs, pinned by
  `beam_rt`'s `inet_matrix_tests` at 160 oracle-cross-checked shapes; `port_connect/2`,
  `port_set_data/2`/`port_get_data/1`, port registration, `'EXIT'`/`'DOWN'` on close, `port_info/2`'s
  `queue_size`/`memory`, `Driver::has_control` gating and `{packet,N}` on the output path all agree;
  `driver_async` uses a real `AsyncPool` (`+A N` workers, `+A 0` the inline branch) drained by the poll loop,
  `async_free` exactly once. Residuals: Known Structural Debt; detail in `beam_abi/CONTEXT.md`.
- **The compatibility gate (`../tools/differential/gate.sh`)** is the authority; a full-corpus sweep of
  `differential run <case-id>` is gate-equivalent per case (`compare.rs::verdict_with` folds in the
  oracle-vs-golden drift check). The live corpus size and per-contract split belong to that node —
  read `../tools/differential/CONTEXT.md` rather than pinning a case count here, because a pinned count
  goes stale the moment the corpus grows. The required verdict is **zero mismatches**; the one recorded
  capability gap is `wire/new_fun_ext_encode` (A.5, unclosable by design — see Known Structural Debt).
  One A.4 case
  (`driver_extra/driver_pushqv_with_a_partial_skip_enqueues_the_tail`) has an oracle-side crash flake (a
  corpus-harness defect, not a divergence).
- **Term/heap foundation complete** (see the section above): HAMT/CHAMP map builder, public
  bitstring/heap-bits APIs, off-heap refc binaries — oracle cross-checked (`beam_term/tests/oracle.rs`) with
  GC copy-correctness evidence for HAMT maps + refc binaries (`beam_gc/tests/{map_copy,refc_binary_copy}.rs`).
  Detail: `beam_term/CONTEXT.md`, `beam_gc/CONTEXT.md`.
- **`beam_term` foundation extensions** (no frozen constant/ABI changed): `From<TermError> for BeamError`;
  `compare`/`hashing`/`size`; `thing` (heap-agnostic off-heap term materialization, forwarded to by
  `beam_codec::term`). Writers take a caller-provided `*mut Eterm`; allocation stays in `beam_alloc`.
- **Stage-1 execution tiers shipped:** a direct-threaded interpreter plus a native x86-64 baseline tier
  (`beam_engine/src/jit/`, block linker + cross-run block cache + linked local tail calls), the **default for
  every `beam_rt` process entry**, compiled lazily per function; `BEAM_RT_TIER=interpreter` is the only kill
  switch. Tier-equivalence gate `beam_engine/tests/jit_real_module.rs`: 736/736 oracle-checked comparisons
  agree. An optional default-OFF Cranelift hot tier sits behind the `jit-cranelift` feature. Detail:
  `beam_engine/CONTEXT.md`.
- **BIF surface complete:** **556/556** declared `bif.tab` BIFs; registry **667 keys** = 556 declared + 111
  stdlib fast paths (25 of them `zlib`), re-derived from code and pinned by `beam_bifs/tests/bif_tab.rs`. A
  registered-but-unadopted BIF answers the typed `unsupported_bif` until a host hook is installed. Per-seam and
  per-family detail in `beam_bifs/CONTEXT.md`.
- **Data-structure and runtime defaults:** `beam_ets` — `set` → CHAMP, `ordered_set` → K-ary B+-tree
  (arithmetic key order, `1 == 1.0`), `bag`/`duplicate_bag` → chained buckets. `beam_gc` — generational Cheney
  copying + an immix mature generation auto-selected at/above `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS =
  2048`, with an on-demand growth seam (`Collector::ensure_capacity`, `HeapPolicy::unbounded()`,
  `GcConfig::growable()`). `beam_dist` — all 30 `Dop` variants round-trip byte-exact (30 golden vectors vs the
  oracle) plus live interop for the `'N'` handshake, `DOP_REG_SEND`, `DOP_SEND_SENDER`,
  `DOP_MONITOR_P`→`_EXIT`, `DOP_EXIT2`, `DOP_UNLINK_ID`/`_ACK` and fragmentation both ways, the defaulted
  decode-side `control::LocalIdentity` and the public `ref_wire` mapping; `epmd` `DUMP`/`KILL`/`STOP`.
  `beam_rt` — boot/CLI/env, `system_info`/`process_info`, a native BIF floor, observability modules
  (`src/trace.rs`, `src/seq_trace.rs`, `src/crash_dump.rs`, `src/perf_map.rs` — `send` events only, empty
  perf map), inbound distribution on by default, `DEFAULT_HEAP_WORDS = 1<<24` (`error:system_limit` on
  exhaustion), an allocation-free registered-BIF dispatch hot path. Per-crate detail in the owning
  `CONTEXT.md`; measured figures in `../tools/bench/RESULTS.md`.
  Typed A.5 gaps: the distributed-spawn family (`DOP_SPAWN_REQUEST`/`DOP_SPAWN_REPLY`),
  `Delivery::Other(_)`, the ack-owing `DOP_UNLINK_ID` half (`id != 0`), a remote **named** monitor
  `{Name, Node}` and `DOP_PAYLOAD_MONITOR_P_EXIT` (the plain 5-tuple is always emitted).
- **Distribution signals (contract A.5) are live in both directions, with two-live-VM evidence.** Outbound,
  `beam_process`'s link/unlink/monitor/exit/terminal entry points route real control through the
  `DistSignalRouter` installed on the process table: `DOP_LINK`, `DOP_UNLINK_ID`, `DOP_EXIT` (death-time link
  exit), `DOP_EXIT2` (`erlang:exit/2` on a remote pid), `DOP_MONITOR_P`, `DOP_DEMONITOR_P` and
  `DOP_MONITOR_P_EXIT` (a dying watched process telling each remote watcher,
  `TUPLE5(DOP_MONITOR_P_EXIT, Watched, Watcher, Ref, Reason)` — watched first, the C's `dist.c:1497-1522`;
  the reference is packed by the shared `beam_codec::ref_id_to_numbers`). Inbound, every family reaches the
  local process through `beam_process`'s route-free receipt primitives, so an inbound delivery emits no
  outbound `DOP_*`; `beam_dist`'s `LocalIdentity` is installed per drain on all three decode paths, so an
  inbound pid/port/reference for this node is the internal term and `Pid =:= self()` holds for a peer that
  reflects a pid back. Evidence: `crates/beam_rt/tests/dist_two_vm.rs` (rust ↔ real `erl`, **24 cases**:
  link/unlink, monitor/demonitor with the peer-minted ref bound, `exit/2` kill vs `normal`, aliases both
  ways, identity round trip; plus the inbound half — a peer's link landing in the `links` item and clearing
  on unlink, the peer's monitor of our process getting a `'DOWN'` carrying the boxed reason, the peer's death
  killing a non-trapping link or being trapped with its real reason, the full `DOP_EXIT2` trapping/`kill`
  matrix, and three peer-node-loss `noconnection` cases: a non-trapping link dies, a trapping link gets
  `'EXIT'` with `noconnection`, a monitor gets `'DOWN'` with `noconnection`), `tests/dist_two_rust.rs` (two
  `beam_rt` VMs, **12 cases** — the same inbound families over two `beam_rt` VMs), `tests/dist_two_vm_wire.rs`
  (1 hermetic byte-level case pinning opcode 21, the watched-first argument order and the negotiated `DFLAG`
  set). All are `#[ignore]`d (two VMs each, ~86 s in total for 24 + 12 + 1 cases): run them with
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --test dist_two_vm
  --test dist_two_rust --test dist_two_vm_wire -- --ignored --test-threads=1`. A `beam_rt` node cannot render
  a remote identifier (`pid_to_list/1`, `~p` of an external pid/ref is a typed gap), so the rust↔rust legs
  assert term identity rather than text; every fixture keeps a wake source armed (a bare `receive` makes
  `run_scheduled` report its honest `blocked`, exit 3).
- **Outbound ordering invariant:** every message send on a link first drains that link's pending control
  queue (`DistClient::flush_queued_controls`, invoked at the top of `send_reg_send`/`send_message_pid`/
  `send_alias`), so a `link`/`unlink`/`exit`/`exit2`/`monitor`/`demonitor`/`monitor-exit` created earlier is
  never overtaken by a message written after it on the same link. ERTS writes a control message synchronously
  when the signal is created, under the link lock (`../../erts/emulator/beam/dist.c`'s
  `erts_dsig_send_*` family); this router can only queue, so the guarantee is re-established at the write. A
  message never reaching the wire while an earlier control is still queued is the property the contract needs.
- **The off-heap refc-binary reclamation loop is closed end to end** (trigger + reachability sweep + a host
  seam) across four crates: `beam_gc` owns the algorithm and accounting model (`BinVheap`,
  `for_each_reachable_binary_payload`, `HeapFootprint`), `beam_engine` exposes the machine's reservoir,
  `beam_process` publishes/adopts the figure, `beam_rt` drives it at the runner's park arm — so a
  no-retention off-heap-building loop is flat in N. Detail: `beam_rt/src/runner/CONTEXT.md`,
  `beam_gc/CONTEXT.md`, `beam_rt/tests/off_heap_reclaim.rs`.
- **Measured on the unmodified EMQX `emqx_base62:encode/1`:** with the prune enabled `erlang:memory(binary)`
  is flat (~272 KB → ~315 KB across N, `VmHWM` ~120 MB at every N) where the unpruned configuration is a
  linear 1 072 B/call; ns/call ratio post/pre 0.885, post/oracle 27.4×. A **capacity** fix, not a throughput
  one. Figures and caveats: `../tools/bench/RESULTS.md`.
- **Dependency edges beyond the scaffold:** `beam_message → beam_gc`, `beam_dist → beam_alloc`,
  `beam_engine → beam_alloc` (directionally legal); `RefCount` lives in `beam_alloc` and is re-exported by
  `beam_gc`.
- **Term-carrying structured raise reasons** (coordinated across `beam_common`/`beam_engine`/`beam_bifs`):
  `Raise` carries an opaque `live_reason: Option<LiveReason>` slot whose payload convention is the live reason
  term as an `Eterm`; `beam_engine` populates it on every raise, `beam_bifs` attaches the true reason term
  (`{badmap,M}`, `{badkey,K}`, `{case_clause,V}`, `{badrecord,T}`, `{badfield,{{M,N},K}}`, any non-atom
  `error/1,2`/`exit/1`/`throw/1`/`nif_error/1,2` argument), and `beam_rt::runner::exit_reason` renders it as
  the uncaught reason (an atom reason stays `ExitReason::Atom` with no slot). Per-node detail:
  `beam_common/CONTEXT.md`, `beam_bifs/CONTEXT.md`.
- **Failed head-clause/guard terminates via `func_info`; a missing module/function is bare `undef`.** `Instr::FuncInfo`
  raises `error:function_clause` (the C's `i_func_info_IaaI`); the `undef` raise's stacktrace HEAD is the
  synthesized callee item `{TargetModule,TargetFunction,ArgList,[]}` cooked by `beam_engine` for an unresolved
  `call_ext*`/`apply/3` (and by `beam_rt/src/eval/` on the `-eval` route), riding the raise's own raw-trace word
  `x(2)` — no frozen `RootSet`/`StackFrame` seam changed (`beam_common::StackFrame` stays metadata-only). The
  oracle's trailing `erl_eval`/`init` frames are deliberately not fabricated.
- **Host seams and BIF-level oracle parity:** the process dictionary is wired end-to-end through the
  `DictHook` seam (`beam_rt::context::DictBridge`); `term_to_binary/2` uses `beam_codec`'s
  `MinorVersion::{V0,V1,V2}` byte-identically; the port BIFs (`port_info/1` = the 7-item set in oracle order,
  `port_info/2`, `port_command/3 [force]` ⇒ `error:notsup`, `open_port(_, [list])` ⇒ `badarg`) and the
  `-eval` map-literal key order agree with the oracle.
- **The native-record opcode family is modelled and executed.** All eight members lower to typed `Instr`
  variants and run in `beam_engine`'s `families/records/`, branching to the fail label on exactly the C's
  record-semantics failures. `put_record/6`'s create form (`Src == nil`) works for the running module's OWN
  records; a create from a **foreign** module's record id stays a typed `unsupported`.
- **The NIF subsystem (contract A.3) is live end-to-end, including the stateful `enif_*` family.** `beam_abi`
  implements the 176-slot `enif_*` host API over `NifHost`/`NifEnv` with no `beam_codec` dependency edge;
  resources are keyed by reference **identity** (a copied resource term still resolves); `enif_send`,
  `enif_monitor_process`, `enif_select` + the resource `stop` callback, `enif_schedule_nif` and
  `enif_alloc_env` are oracle-cross-checked against `cc`-built C fixtures (each test skips loudly without the
  toolchain); term-only resources are reclaimed at the runner's park arm with a complete-root predicate. The
  dynamic loader (`nif/loader.rs`) `dlopen`s a NIF and validates `ErlNifEntry`; `beam_bifs` adds
  `erlang:load_nif/2` and `beam_rt` owns `NifRegistry`/`NifBridge`/`NifHostImpl`, intercepting NIF calls at
  external `call_ext` dispatch. Per-function coverage and residuals: `beam_abi/CONTEXT.md`,
  `beam_bifs/CONTEXT.md`, `beam_rt/CONTEXT.md`.

## Contract A.9 (build/bootstrapping) — status
A.9's differential coverage is boot/version **observability** plus the in-process compiler route.
- **In-process compilation — all 755 oracle-compilable sources byte-identical** (772 `lib/*/src/*.erl`
  sources over 34 apps; 17 fail on the oracle itself). `code:get_path/0` returns the oracle's 36 entries,
  derived from the release layout by `beam_rt/src/codeserver/path.rs`. Harness: `beam_rt`'s `#[ignore]`d
  `selfhost_breadth` (shard-resumable). There is still **no self-hosted build**.
- **Boot path — completes.** `beam_rt` executes the real `bootstrap/bin/start.boot` (22 instructions) through
  the final `{progress,started}`; `beam_rt -noshell -boot bootstrap/bin/start` exits 0 with empty
  stdout/stderr. Pinned by `beam_rt/tests/boot.rs` + `src/boot/bootscript.rs`; detail in
  `beam_rt/src/boot/CONTEXT.md`. `-eval`/`-s`/`-run` drive the default boot script too (so a `-eval` process
  has the live `kernel_sup`/`timer_server` tree and a firing timer delivers its message on both routes); a
  spawn target on the runtime host floor (`RuntimeContext::host_serves`) is dispatched through the BIF-entry
  trampoline, and the `-eval` route's `receive … after T` really waits (`Runtime::wait_for_timer`).
- **Remaining A.9 gaps (fidelity, not boot-completion):**
  1. **No self-hosted build** — the Rust runtime does not compile/run itself (Stage 2 bootstrap, `../lib/compiler`,
     is not wired into `beam_rt`).
  2. **No live `code_server`/`erl_prim_loader`/`application_controller` process** on a plain runtime — `code:*`,
     `init:*` and `application:*` queries come from a native floor (`beam_rt/src/native/{code,init,application}.rs`);
     on the `-boot` path the real `application_controller:start/1` IS spawned and stays registered.
  3. **A live `logger` process IS registered** (`error_logger` has none).
  4. **The native `file_server_2` is not the real Erlang `file_server`** (a mailbox-drained
     `beam_process::Process`), but `{get_until,…}`/`{get_line,…}` ARE served for regular files.
  5. **`init:restart/0`, `init:boot/1`, `init:stop/0,1` and `init:get_arguments/0` are implemented** and
     oracle-cross-checked.
  Per-node detail: `beam_rt/CONTEXT.md`, `beam_rt/src/boot/CONTEXT.md`, `beam_rt/src/native/CONTEXT.md`.

## Known Structural Debt (candidates for the next pass)
- **A.4 port/driver residuals** (each a typed error, never a wrong result): (a) a **header-less** read of a
  built-in driver port (`tcp_inet`/`udp_inet` passive `recv`, still on `beam_io`'s flat
  `DriverContext::input()` sink) arrives as list elements where the C answers a binary on a `[binary]` port —
  a read carrying the C's `(hbuf, hlen, buf, len)` split does render it, and a line frame's derived header
  prefix stays out of the input counters; (b) a **raw-mode read of nothing** (`driver_output(NULL,0)`) is
  dropped where the oracle delivers `{Port,{data,[]}}`; (c) `INET_REQ_FDOPEN`'s adoption form, `ignorefd` and
  `tcp_sendfile` reproduce their refusals only. Detail: `beam_io/CONTEXT.md`.
- **A.3 residuals** (each a typed refusal or a deferred answer): (a) an off-call-path NIF thread reaches only
  `beam_rt`'s `ThreadEnvHostFactory` host, whose atom space is a private append-only name space (`atom_table`
  is `None`, so env-less `enif_compare`/`is_identical`/`hash` order a nested atom by raw index;
  `select`/monitors/liveness/`whereis_pid`/`make_ref`/`system_info`/ETF/`port_command` are unsupported there;
  a remote pid destination answers `false`; a send is queued at `enif_send` and delivered at the next drain);
  (b) the load/unload host runs with `services = None` and `upgrade` is not modelled (a second `load_nif/2` is
  the oracle's `reload` refusal), and a `dtor`-time send has no drain point; (c) a monitor of a **remote** pid
  answers `NotAlive`, and a copied `enif_make_resource_binary` term does not resolve through
  `enif_get_resource`; (d) `nif_yield_point` is an honest no-op (fairness, never correctness) and
  `dirty_scheduler_support` stays `0` while `enif_thread_type` still reports 2/3. Detail:
  `beam_abi/CONTEXT.md`.
- The map/bitstring/refc handoffs are only partly adopted: `beam_codec` forwards to `beam_term` but keeps a
  crate-local oversized-*flat*-map writer; off-heap refc liveness is the owning process's.
- `wire/new_fun_ext_encode` remains `unsupported` **in the gate**: its `-eval` lambda is an `erl_eval` template
  closure whose identity/`num_free`/`env` are erl_eval internals no seam can derive faithfully (only a full
  `erl_eval`/`erl_parse` port would; fabricating them is forbidden). A compiled module's local fun encodes
  byte-exactly through `LocalFunIdentity`.
- Repo-root owned (outside this node): `../../CHANGE.md` documents Milestones 1–3 and still owes entries for
  the `beam_term` `map`/`bitstring`/`refc` foundation, the `RefCount` move into `beam_alloc` + the
  `RefcBinary` off-heap seam + `beam_gc::OffHeap` owning refc binaries, and the runtime parity work
  (`func_info`→`function_clause`, the `DictHook` process dictionary, the `{minor_version}`
  `term_to_binary/2` adoption, sorted `-eval` map literals).

## Design Decisions
- **One reference-id space, owned by the LCA.** Reference ids are minted by `beam_common::next_ref_id`
  and by nothing else. `beam_common` hosts it because it is the lowest common ancestor: `beam_abi`
  (wave 0) cannot host a type that `beam_process` (wave 3) draws from, and `beam_process` must not
  depend on the NIF ABI crate. `beam_process::RefAllocator` survives as the typed handle
  (`new()`/`next() -> MonitorRef`) so dependents are unchanged; `beam_abi` narrows the shared id to
  `beam_term::MAX_SMALL` **checked** (an `as` cast would shift `1<<59` into the immediate's sign bit and
  alias two ids) and refuses rather than writing a wrong id. The space is for *mints* only: a
  fabricated/derived id (`MonitorRef::from_id`) is the caller's claim, and the one such id in the
  workspace — an unnamed ETS table's tid — comes from `beam_ets`'s table-handle band so it decodes back
  to its handle. Cross-producer collision-freedom is pinned end to end by
  `beam_rt/tests/ref_id_space.rs` (NIF resource terms + monitors/aliases + `make_ref`/timers,
  interleaved: pairwise distinct and strictly increasing) and per producer by
  `beam_abi::resources_tests` / `beam_process::tests/reference_ids.rs`.
- **Error interop is owned upstream, not duplicated.** The
  `From<beam_term::TermError> for beam_common::BeamError` conversion lives **in `beam_term`**, so all 13
  dependents get `?`-interop for free instead of each writing a mapping (Prevent-Duplication). It is the
  "reconcile once `beam_common` lands" step `beam_term`'s own `CONTEXT.md` prescribes. (Owes a
  `../../CHANGE.md` note — flagged to the parent node, which owns the repo root.)
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
