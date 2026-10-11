# CHANGE.md — Major Modifications

This file records **major modifications** to the ERTS Rust rewrite: architectural shifts,
major data-structure refactorings, and algorithmic changes. It is a coarse, human-readable log
of *decisions and structural changes*, complementing `git log` (which holds every line-level change).

Format: one entry per major change — date, area, what changed and why. Newest first.

---

## 2026-10-10 — Milestone 6: GC root-set completion (the P0 fix), mailbox use-after-free, heap-sizing defect, bounded ETF decode, 3-word External fun descriptor, ETS navigation, IO framing + ABI fixes, boot-to-completion
Scope: a correctness-and-robustness milestone. The headline structural change is the **completion of
the frozen root-set contract** — the class of defect that produced an intermittent `SIGSEGV` — plus
two further use-after-free-class fixes in the message layer and a heap-sizing defect that made
mature capacity grow without bound. Also: the ETF decoder is now depth-bounded against hostile input,
the on-heap External-fun descriptor became 3 words (a real term-layout correction), ETS navigation
was re-derived from the oracle, and the `beam_rt` boot-script executor now drives the real
`bootstrap/bin/start.boot` to completion. Verified: `cargo build --workspace --all-targets`
warning-free, `cargo test --workspace --no-fail-fast` green (8200 passed / 0 failed / 5 ignored), and
the differential gate **`total=1839 agree=1837 mismatch=0 unsupported=2 skipped=0 allowed=2`** → exit 0.
- **Area:** `erts_rust/crates/beam_rt` (`context/mod.rs`), `erts_rust/crates/beam_process`, `erts_rust/crates/beam_gc`
- **Change (THE P0 FIX):** a GC collection could run with an **incomplete root set**.
  `RuntimeContext` implemented only the narrow `ExecutionContext::ensure_heap`, inheriting the
  trait default for `ensure_heap_full` (which discards the frame-register root) and
  `alloc_words_rooted` (which discards the caller's roots) — so a live `x[2]` could sit outside
  `x[0..live)` while a collection relocated the heap. Reproduced as an intermittent `SIGSEGV` in
  `beam_term::list::car` ← `lists`' `Instr::GetList` ← `beam_rt::runner::run`.
  **Structural fix:** the only doors a host may use to reach the heap-admission point are now
  `ensure_heap_full` and `alloc_words_rooted`, both rooted over the **whole machine root set**
  (`publish_roots_full`/`scatter_roots_full`); a shared `collect_or_grow(process, heap_need)` helper
  serves both. An **unscanned-root decline valve** (`root_is_unscanned`) makes the runtime decline to
  collect (grow instead) rather than relocate against a root set it cannot prove complete —
  correctness over collections. `Message::from` is now required to be an **immediate** at every
  producer (`beam_process/src/ptab.rs`: 4 on-heap producers fixed to the C's `am_system` / target-pid /
  node-atom conventions, matching `ASSERT(is_immed(ERL_MESSAGE_FROM(msg)))`).
  Consequence: the `erl_error:format_exception/3` repro runs to a byte-identical oracle result.
- **Area:** `erts_rust/crates/beam_process` (`src/process.rs`)
- **Change:** a **use-after-free** in the message queue: `run_rooted_collection` rebuilt queued
  messages with `Message::new(..).with_token(..).with_off_heap(..)`, silently dropping the message's
  `fragments`/`binaries`/`nodes` — a relocated message lost its off-heap handles. Rebuilt through
  `message.with_payload(payload)` so every field survives relocation.
- **Area:** `erts_rust/crates/beam_process` (`src/process.rs`), `erts_rust/crates/beam_message`
- **Change:** the process's persistent `off_heap` table had **dual duty** — `copy_reason_onto`
  registered a queued signal's reason objects into it while the send path drained that same table
  (`mailbox::enqueue_copied`), so a later delivery could steal a queued reason's handles. The send
  path now stages through a **fresh, empty** off-heap table.
- **Area:** `erts_rust/crates/beam_gc` (new `src/live.rs`)
- **Change:** the **heap-sizing defect** — mature capacity grew 233 → 6772 words over a steady-state
  loop because the sizing decision used an over-approximation of live data. Replaced with an **exact
  reachable-live counter** (`live_full`/`lines_for`, the same walk the collector performs), so capacity
  is a function of true live words and stays constant (`tests/steady_state.rs`,
  `tests/offheap_steady_state.rs`). `OffHeap::sweep` is now **slot-stable** (released slots are kept
  in place; the on-heap reference stores a slot index, so the old compacting `retain` was unsafe to
  call). `AnyCollector` became a complete strategy-agnostic drop-in for the concrete `Collector`
  (`should_collect`, `mature_capacity/used/available`, `mature_contains`); the GenImmix verdict is
  measured in `beam_gc/MEASURED.md` (Immix moves 35.7 % fewer words but needs a 3.79× larger mature
  footprint and a longer fullsweep pause) → **`MatureKind::Copying` stays the default**.
- **Area:** `erts_rust/crates/beam_codec` (`src/decode`, `src/compress.rs`)
- **Change:** the ETF decoder was **unbounded in recursion depth** — hostile input could exhaust the
  stack. Added `MAX_DECODE_DEPTH = 1500` plus a hostile-input corpus and a ~5000-term round-trip
  property test. Also: `COMPRESSED` now exposes the number of source bytes it **consumed on the wire**
  (`decode_term_with_local_node_used`, `compress::decompress_bounded_used`) — the `binary_to_term/2`
  `[used]` capability the distribution reader needs, previously emulated downstream by an O(n) binary
  search.
- **Area:** `erts_rust/crates/beam_term` (`src/fun.rs`), `erts_rust/crates/beam_codec`, `erts_rust/crates/beam_gc`
- **Change:** the on-heap **External fun descriptor is 3 words** (`[module][function][arity]`), not 2 —
  `desc_words` = 2 / 3 / 10 for `Local`/`External`/`Imported`. This is a real term-layout correction:
  the block size, the GC copy sizing (`fun_block_words`), and the codec's `EXPORT_EXT` handling all
  follow. Added `beam_term::write_internal_ref` / `INTERNAL_REF_WORDS` for the internal-reference form.
- **Area:** `erts_rust/crates/beam_ets` (`src/table/mod.rs`, `src/select`)
- **Change:** `select_replace/2` on a `duplicate_bag` rebuilt affected keys from checked-out `take`
  copies whose store storage was never freed — an unbounded leak. Added `Table::replace_key`, which
  validates + deep-copies first (rollback on rejection), frees the old key through the ordinary removal
  path, then installs the already-stored roots. Hash kinds were re-derived from the oracle: they have
  **no backward walk** (`last_key ≡ first_key`, `prev_key ≡ next_key`) — the previous "reverse walk"
  was a fabricated behaviour. `ordered_set` continuations are key-anchored (`Cursor::{Begin,Anchor,Position}`).
- **Area:** `erts_rust/crates/beam_io` (`src/drivers`)
- **Change:** 13 real defects fixed in the port/driver layer, notably `tcp_inet`'s `{packet,N}`
  framing (every framed send was mis-framed), `EINTR` retry, and the `AbiHost` port-queue family.
- **Area:** `erts_rust/crates/beam_process` (`src/signal.rs`, `src/ptab.rs`)
- **Change:** the signal-build **grow policy** — an oversized `{'EXIT',…}` reason was lost because the
  build buffer could not grow; `DistSignal::Exit`/`Exit2` split; best-effort termination cascade.
- **Area:** `erts_rust/crates/beam_abi`
- **Change:** the NIF/driver ABI's iolist, UTF-8 string/atom and bit-offset paths were wrong
  (`enif_inspect_iolist_as_binary`: a byte integer is a byte only in head/CAR position; `enif_get_string`
  UTF-8 truncation; `enif_inspect_binary` bit offset; `enif_make_sub_binary`). Added a safe
  `driver/async_job.rs` async runner (`AsyncJob`/`AsyncCallback`).
- **Area:** `erts_rust/crates/beam_engine` (`src/families/*`, `src/terms/update.rs`, `src/jit`)
- **Change:** the whole non-obsolete `genop.tab` opcode set is now modelled — the last one
  (`set_tuple_element/3`) landed as an in-place heap write with the baseline JIT inlined and
  literal-area-guarded. The whole-OTP corpus (1366 `.beam`, 2,888,257 instructions) lowers with
  **0 unmodelled opcodes**. A raise now carries a `raise_snapshot` stack trace; `badmap`/`badkey`
  sealed; native cross-module `call_ext` via an `ENTER = -3` block-ABI word.
- **Area:** `erts_rust/crates/beam_loader`
- **Change:** the loader's instruction roster is 134/193 typed; the literal pool, line/range tables
  and the `ops.tab` transforms are all lowered from the real `.beam` chunks.
- **Area:** `erts_rust/crates/beam_rt` (`src/boot`, `src/native`), `erts_rust/tools/differential`
- **Change:** the boot-script executor now drives the real `bootstrap/bin/start.boot` — **all 22
  directives** to `{progress,started}` (the `-boot` CLI exits 0 with empty stdout/stderr); the
  `{kernelProcess,…}` servers survive their directive process, and instruction 20's
  `{apply,{c,erlangrc,[]}}` is answered by the runtime-native `file_server_2` service. The native floor
  gained `init:boot/1`, `init:stop/0,1`, `init:restart/0`, `code:load_file/1` and the automatic
  `init:get_arguments/0` entries. The differential corpus grew **1633 → 1839** cases (A.3 +32, A.4 +16,
  A.5 +88, A.9 +70) and `gate.sh --strict` is green with a fresh 2-entry allowlist.
- **Area:** `erts/` (legacy C)
- **Change:** the in-tree dead-source checker was replaced by a **2-tier transitive-closure** analysis
  (`--root`, STRICT tier, `--write-report`) that covers the whole `erts/` tree, and 4 genuinely dead
  sources plus one dead `#ifdef LOADABLE` block were removed. Every remaining `.c` is referenced by an
  explicit object list, so the parity-gated removal plan and its 5-point per-file gate are documented
  in `erts/emulator/beam/CONTEXT.md` rather than acted on optimistically.
- **Area:** `lib/compiler` (Stage 2)
- **Change:** loop optimisation (with an arith-LICM), operation fusion (a new `has_map_field` +
  `maps:get/2` fusion) and FBIP (in-place `update_record` on a fresh, uniquely-owned source) are
  implemented, wired with derived kill switches, and proved sound by an equivalence harness
  (996 probe modules / ~156k compared calls, 0 mismatches). Two soundness holes were found and closed
  (`bsl`/`bsr` are not LICM-safe; a fresh *binary* is not a valid in-place `update_record` source).

## 2026-10-09 — Milestone 5: in-machine cross-module calls, ETS lifetime/concurrency fixes, per-object reclamation, slim IR + superinstruction, NIF/driver ABI completion, signal/ddll facilities, Stage-2 compiler passes
Scope: a broad correctness + performance milestone across the runtime. The headline structural
changes are (a) a **suspendable cross-module call seam** so a `receive` inside a callee parks and
resumes instead of failing, (b) a **storage-lifetime fix in all four ETS interiors** (a real
use-after-free), (c) a **slimmer execution IR plus a superinstruction pass**, (d) the **NIF/driver C
ABI brought to completion**, and (e) **loop-optimisation / operation-fusion passes in the real
compiler**. Verified: `cargo build --workspace --all-targets` warning-free, `cargo test --workspace
--no-fail-fast` all green (6646 passed / 0 failed), and the differential gate
**`total=1100 agree=1093 mismatch=0 unsupported=7 skipped=0`** — no case disagrees.

- **Area:** `erts_rust/crates/beam_rt` — the `ExecutionContext` glue + runner (over the seam `beam_engine` exposes)
- **Change:** `RuntimeContext` implements the engine's `ExecutionContext::resolve_external_call`, so a
  `call_ext*` callee runnable in this machine runs **in the caller's machine** — the engine stores the
  caller's return address and enters the callee's export label — instead of recursing through the
  synchronous `call_bif` → `run_export`. The override returns the *same* `Arc<Module>` the `CodeServer`
  holds (contexts dedupe by pointer identity) and the export label after `func_info`, loads an uncached
  callee on demand, and answers `Ok(None)` — preserving the synchronous path byte-for-byte — for every
  import it must not intercept. A **fun-bodied** process entry gets the same treatment.
  Consequence: `proc_lib:start/4` / `proc_lib:start_link/4` — and hence `gen_server`-class startup —
  park in `receive` and resume instead of folding into a `blocked` error.
- **Area:** `erts_rust/crates/beam_rt` (`runtime/ets_owner.rs`) + `erts_rust/crates/beam_process` (`ProcessExitObserver`)
- **Change:** The runtime installs a process-exit observer and two scheduler-pass **drains**: a dead
  process's ETS tables are released (dropped, or handed to the installed heir with the oracle's
  `{'ETS-TRANSFER', Tid, FromPid, Data}`), and its dangling incoming **port monitors** are swept. The
  observer only *records* obligations into its own cell (a death can be reached while a BIF holds a
  registry borrow); the drains discharge them with `try_borrow_mut` and defer — never panic, never drop
  — while that borrow is live.
- **Area:** `erts_rust/crates/beam_rt` (`context/gc.rs`, `runtime/gc.rs`)
- **Change:** The runtime now **drives `beam_gc`**: a scheduled process allocates on its own young heap
  and is collected at the engine's heap-admission point (`ensure_heap`), with the executing machine's
  `x[0..live)` plus the Erlang stack as the root set. Collection is armed per call by the slice's own
  budget context (a nested machine cannot clobber outer roots); a module with a float-family
  instruction is never collected over (`Machine::fr` is outside the root set); a failed collection halts
  that process's collection only; and an uncopiable store term trips a global valve that freezes every
  process heap. An `alloc_words`-only workload (no `test_heap` admission point) still falls back to the
  pointer-stable arena and is not collected — reaching it needs a mutable root-set hook on `Machine`.
- **Area:** `erts_rust/crates/beam_ets` — `src/table/{element,node,hash_interior,champ_interior}.rs`
- **Change (a live use-after-free):** `key_of(object, keypos)` returns a pointer **into** a table-owned
  `TermStore` object, but the chained-hash (`Entry.key`) and CHAMP (`Slot.key`) interiors stored that
  pointer as the index key, and the B+-tree stored **branch separators** aliasing a leaf's key element.
  On an upsert — or a `bag`/`duplicate_bag` `delete_object` that removed the anchor object — the stale
  term was not re-pointed, so `TermStore::free` reclaimed its runs and a later same-size insert reused
  them, and the stale term then read a *different, well-formed* term. Fixes: re-point the stored key on
  the replace arm; re-anchor a surviving entry via a keypos-aware `HashInterior::with_keypos`; derive
  the index key from the **table-owned copy**; and always re-materialise B+-tree separators from the
  rightmost live leaf (`NodeStore::subtree_max_key`).
- **Area:** `erts_rust/crates/beam_ets` — `WriteConcurrency` + `select_reverse` + `child_index`
- **Change:** `{write_concurrency, auto}` and `{debug_hash_fixed_number_of_locks, N}` are now modelled
  (`WriteConcurrency { Off, On, Auto, FixedLocks(u32) }`, with the bool API as its `Off`/`On`
  projection, so the change is source-compatible) and `decentralized_counters` is derived from it.
  `select_reverse/1,2,3` is implemented for all four kinds with direction-carrying continuations.
  `node::child_index` now **binary-searches** the strictly-ascending B+-tree separators instead of a
  linear `O(ORDER)` scan — measured `ordered_set` insert **0.32–0.69×**, hit **0.52–0.68×**, miss
  **0.26–0.31×** at n=10⁵. A guard-scoped traversal API (`TableRead::{iter_keys, iter_keys_rev,
  iter_objects, iter_objects_rev, foldl, foldr, tab2list_into}`, `TableTerm::tuple_elements`) makes an
  escaping borrowed term *unrepresentable* (three `compile_fail` doctests).
- **Area:** `erts_rust/crates/beam_loader` (`src/instr.rs`, `src/lower/fuse.rs`) + `erts_rust/crates/beam_engine`
- **Change:** the typed IR is made cheaper to dispatch with **no behavioural change**.
  `size_of::<Instr>()` **160 → 64 bytes**, `size_of::<Operand>()` **32 → 16**. The drivers were
  decode-time-only, never-stored payloads: `Operand::BigInteger/List/Allocation` are boxed
  (`Box<BigInt>` / `Box<Vec<..>>` — *not* `Box<[T]>`, whose fat pointer re-inflates `Operand` to 24),
  and nine operand-heavy **cold** `Instr` variants carry boxed payload bundles, while the hot variants
  (move/call/`gc_bif`/test/select and the whole receive family) stay inline. Compile-time
  `assert!(size_of::<Instr>() <= 64)` / `Operand <= 16` / `HeapNeed <= 24` guards prevent silent
  re-inflation. A new `beam_loader` **superinstruction pass** (`src/lower/fuse.rs`, run before blocks
  and labels are built) coalesces the adjacent `Deallocate(N); Return` emitted at every non-tail exit
  into `Instr::DeallocateReturn(N)`; both JIT tiers deopt it to the same `step_instruction`, and
  `Deallocate`/`Return` stay live for hand-built modules. Measured (release list walk, median-of-3):
  **45.01 → 41.05 ns/element (~1.10×)**; deterministic **8,000,006 → 7,000,006 instructions/run
  (−12.5 %)**, 54,479 pairs fused corpus-wide.
- **Area:** `erts_rust/crates/beam_abi` + `erts_rust/crates/beam_io` — the C ABI surfaces
- **Change:** the NIF ABI reaches full coverage and the driver ABI gains its remaining callback slots
  and `ErlDrvTerm` output encoding; a driver `.so` now loads, runs and unloads end-to-end. New
  `beam_io` facilities: a **safe OS-signal facility** (self-pipe + async-signal-safe trampoline;
  `Signal`/`Disposition`/`SignalFacility` with `set`/`disposition`/`read_fd`/`drain`) for
  `os:set_signal/2`, and a **`DynLib`** wrapper (`dlopen`/`dlsym`/`dlclose` + the full `erl_ddll`
  error-atom taxonomy) for the dynamic-driver loader. `Poller::poll` is now `EINTR`-tolerant (retry
  preserving the deadline) — required by the signal facility. Real bug fixed: a scheduled
  `driver_async` job's `free()` was silently dropped when the port died first; `AsyncCompletion` now
  owns the release exactly-once by construction, and `ERL_DRV_USE` is a single sticky merged
  obligation.
- **Area:** `lib/compiler` — Stage 2 (Part II §E), opt-in and OFF by default
- **Change:** three new compiler passes on the BEAM-SSA IR: **loop optimisation**
  (`beam_ssa_loop_opt` + `beam_ssa_loop_cfg`/`_reduce`/`_arith` — induction-variable strength
  reduction, canonicalisation, loop-invariant code motion of pure tag tests), **operation fusion**
  (`beam_ssa_fus` — map-lookup fusion and multiplicity fusion `(V*K1)*K2 → V*(K1*K2)` for type-proven
  integers), and **FBIP** (`beam_ssa_fbip` — a copying `update_record` becomes in-place when the source
  is provably fresh and every cycle through the use passes through the definition). All are wired
  through `beam_ssa_opt_budget.erl`, the single decision point, which now also carries a per-**function**
  effort model so a huge function cannot blow up compile time. Every pass is default-OFF, so the default
  compilation path is byte-identical (verified by Code-chunk md5). **Loop unrolling and loop-index
  narrowing are not implementable** in BEAM (no compile-time trip count for the only reachable CFG
  cycles; a single arbitrary-precision integer type means no narrower index to narrow into) — a
  documented negative result.
- **Area:** `erts_rust/tools/differential` — corpus, ratchet, honesty ledger
- **Change:** the corpus grew **794 → 1100 cases** with new groups for the four under-covered
  contracts (`nif_abi/` A.3, `driver_abi/` A.4, `wire_extra/` A.5, `boot_extra/` A.9). A **strict
  ratchet** (`--strict --allowlist known-gaps.txt`) makes an un-recorded capability gap *and* a stale
  allowlist entry hard failures (`gate.sh` exit 3), with `GAPS.md` + `divergences.md` as the honesty
  ledger. Every capability gap is recorded with its owning crate.

---
Scope: the runtime's **registered-BIF dispatch** — the single largest measured overhead — is
root-caused and fixed (**~10.6 µs → ~116 ns per call**), the runner gains **real pre-emption**
(`RunOutcome::Yield` on budget exhaustion) and runs scheduled entries on the **baseline JIT**, the
message **copy boundary** is hardened against three real bugs, the loader's chunk/attribute
understanding is completed, and the **legacy-C tree gains a mechanical, parity-gated removal process**.
Verified: `cargo build --workspace --all-targets` warning-free, `cargo test --workspace --no-fail-fast`
**all green / 0 failures**, and the differential gate **`total=723 agree=722 mismatch=0 unsupported=1
skipped=0`** (the single remaining `unsupported` is the by-design unclosable `erl_eval` template-closure
encoding, contract A.5 `wire/new_fun_ext_encode`).
- **Area:** `beam_rt` (Stage 5 — runtime dispatch; the headline perf fix)
- **Change:** the ~10.6 µs per-registered-BIF-call cost was a **per-call full `AtomTable` deep clone**
  plus, once that was gated, `RuntimeMemory::new` eagerly computing `code_bytes` by cloning and sorting
  every loaded module name *and* issuing one `std::fs::metadata` **per loaded module per call** — a
  structure evaluated even when nothing consumes it. The atom-interner clone is now skipped when the
  pending-interning set is empty, and the memory/fun-identity handles are built **lazily** behind
  `uses_memory_hook` / `uses_fun_identity` predicates derived from the loaded program's imports. Net:
  registered-BIF dispatch **~10.6 µs → ~116 ns/call**; `erlang:'-'/2` in a tail loop now runs natively at
  ~2.45 ns/iter. The engine-side share of a BIF call is ~11–12 ns (≤0.15 %), so the cost is host-side
  dispatch, not the IR seam. Added a two-point-slope perf harness (`examples/perf_bif.rs`,
  `tests/perf_bif.rs`) that subtracts the per-run setup floor so the number is a per-call slope, not a
  loop-average.
- **Area:** `beam_rt` (Stage 5 — scheduling, process control)
- **Change:** the runner now honours its reduction budget and returns **`RunOutcome::Yield`** — real
  pre-emption instead of run-to-completion — and a **scheduled-export entry executes on
  `Tier::BaselineJit`** (the block JIT is now on the runner's path, with `+JPperf` fed from
  `baseline_code_map`). Adopted `monitor/2,3`, `demonitor/1,2` (incl. `demonitor_flush`), `exit/2,3`,
  the full `process_info` item set, `is_alive/0`, `get_cookie/0`, `set_cookie/2`, `nodes/0,1`,
  `module_loaded/1`, `spawn_opt` heap-size knobs and priorities.
- **Area:** `beam_engine` (Stage 1 — execution, contract A.8)
- **Change:** the **execution-trace seam** landed behind `ExecutionContext::{trace_enabled, trace_event}`
  over a borrowed-name `TraceEvent { Call, ReturnFrom, ReturnTo }`; the gate is latched once per run into
  `Machine::trace_on`, so an untraced run pays ~0 ns and the engine never stringifies on the hot path.
  `tests/trace.rs` cross-checks the event stream against the live `erl` oracle on real fixtures. Also
  added `tests/bif_cost.rs` for the engine-side per-BIF-call attribution.
- **Area:** `beam_message` (Stage 0/5 — the copy boundary)
- **Change:** three real bugs fixed. (1) `is_shared` classified **every bare `HEADER` word** as a shared
  literal — now it requires a genuine term word, so a header word is copied rather than aliased.
  (2) A forged/cyclic term graph made the sizing walk **unbounded** (hang) — the copier now preflights
  through a **cycle-safe, bounded** `preflight_words` (`is_acyclic`), and the failure is a typed
  `Alloc(Message)`. (3) `Fragments::move_into` **half-stripped the chain** when a move failed part-way
  (some fragments released, the term's payload half-moved) — the chain is now released only after the
  whole chain copies. Added the public `is_message_heap_exhaustion(&BeamError) -> bool` classifier so a
  host can translate the copy engine's exhaustion without matching on `AllocKind`.
- **Area:** `beam_loader` (Stage 1 — module loading)
- **Change:** completed the loader's chunk/attribute understanding: a new `src/chunk/classify.rs`
  classifies 15 chunk ids with a whole-corpus checker, and `src/attributes.rs` decodes the `Attr` chunk
  **byte-identically to the oracle**. Corrected a stale claim — the on-PATH OTP-29 `erlc` *does* emit the
  native-record opcodes 186–191 and can be made to emit `executable_line`/`debug_line` and
  `bs_test_tail2`; 192/193 are OTP-30-only. Both framings of `LitT` were verified across 42 959
  literals. Load cost was measured (175.9 ns/instruction; 106 MB/s over 200 modules) and improved
  ~5–6 % with an **O(1) opcode-arity table** and a capped, pre-sized instruction vector.
- **Area:** `beam_abi` (Stage 0 — NIF/driver ABI), `beam_io` (Stage 5 — I/O)
- **Change:** the driver API reached **111/111** with the hidden refcount word modelled and a mechanical
  conformance test; a mis-modelled `ErlDrvBinary` public layout was corrected so a real `.so` compiled
  against the system headers keeps `ERTS_CHK_BIN_ALIGNMENT`. `driver_async` moved from
  thread-per-job to a **bounded process-global worker pool**; port streams gained `StreamOptions`;
  `stop_select` is honoured on deselect/teardown; `tcp_inet` latches peer close. A use-after-free in the
  process-global created-thread name registry — two inserters racing to own one name buffer — was fixed
  with first-wins registration.
- **Area:** `erts/emulator` (legacy C/C++ removal process)
- **Change:** the legacy-C tree now has a **mechanical, parity-gated removal process** instead of ad-hoc
  deletion: `erts/emulator/beam/check_dead_sources.py` (+ its committed `dead_source_report.txt`) proves
  a source file unreferenced (**746 checked / 721 referenced / 25 exempt / 0 deletable**), and
  `erts/emulator/beam/CONTEXT.md` records the **11-group parity matrix** (C module group → Rust owner →
  evidence → verdict) plus the revision rule a deletion needs (reference proof **and** a live
  zero-mismatch gate **and** no self-declared `unsupported` on the covering contract **and** a value
  oracle — a reference proof is *not* a build proof). One further file was deleted this pass
  (`erts/emulator/drivers/unix/bin_drv.c`, 227 lines, proven absent from `DRV_OBJS`), bringing the total
  to four (`multi_drv.c`, `sig_drv.c`, `vxworks_resolv.c`, `bin_drv.c`). The file's deep C-internals
  reference material was extracted into the `.agents/skills/beam-c-internals-reference.md` skill to keep
  the CONTEXT.md under the read limit.
## 2026-10-08 — Milestone 3: complete BIF surface, hot-path flattening, ordered-set B+-tree, on-demand heap growth, full ABI freeze

Scope: the runtime gains the **complete declared BIF surface**, a **flattened interpreter hot path**,
a **250–400× reduction in per-BIF-call overhead**, the planned **B+-tree `ordered_set` interior**, an
**on-demand heap-growth seam**, a **fully populated and mechanically checked C-ABI**, and a much wider
differential corpus. Verified: `cargo build --workspace --all-targets` warning-free, `cargo test
--workspace --no-fail-fast` **5924 tests / 0 failures**, and the differential gate **`total=723
agree=715 mismatch=0 unsupported=8 skipped=0`** (the 8 `unsupported` are honest, typed capability gaps;
one is the documented unclosable `erl_eval`-lambda encoding).

- **Area:** `beam_bifs` (Stage 3 — BIF surface)
- **Change:** The registry now implements **every entry of the real `erts/emulator/beam/bif.tab`** —
  **556 declared BIFs** (`bif` 476 + `ubif` 73 + `hbif` 7), up from 334 (222 missing). The 145 newly
  registered runtime-coupled keys (process control, timers, code loading, the ETS C interior,
  `erts_debug`/`erl_debugger`, `re`, `erl_ddll`, distribution) plus the 77 `erlang` keys and
  `erlang:memory/0,1` all bridge through two new **defaulted** `BifContext` seams — `RuntimeHooks::call`
  (one `(module, function, arity)` dispatcher) and `MemoryHook::memory()` — whose default is the typed
  `unsupported_bif` capability gap. Registration is data-driven per family through a `runtime_bif!`
  macro. With no host hook installed a call now raises the honest `unsupported` (CLI exit 3) instead of
  the bare `error:undef` (exit 1), so the gate can distinguish "not capable" from "not a BIF".
- **Why:** Contract A.7 pins the BIF surface to the real table; a partial roster made every unregistered
  key indistinguishable from a typo and masked genuine gaps.
- **Area:** `beam_rt` (Stage 5 — performance)
- **Change (algorithmic):** eliminated a **whole-`AtomTable` deep clone on every BIF call**.
  `RuntimeContext::call_registered` committed the names interned during a call through
  `CodeServer::atoms_mut()` (an `Rc::make_mut`) unconditionally; the table's strong count is always ≥ 2
  during a call, so the copy-on-write path always fired and deep-cloned the entire table (`Vec<String>`
  + `HashMap<String, Atom>`, thousands of entries after boot) even when nothing was interned. The commit
  is now skipped when the per-call interner recorded nothing, in both `call_registered` and
  `try_call_nif`. Measured marginal cost of one registered BIF call: **~89–102 µs → ~0.2–0.4 µs**
  (≈250–400×): a 200 000-iteration Erlang loop fell from 17.9 s to 0.225 s; a 1 000 000-iteration
  two-BIF-per-iteration loop from 201 s to 0.72 s. Behaviour is byte-identical (an empty commit was
  already a no-op).
- **Area:** `beam_engine` (Stage 1 — execution engine)
- **Change (algorithmic):** the per-instruction path no longer probes ordered maps. The
  `Label -> (function, code)` table became a dense `Vec<Option<(u32, u32)>>` (`LabelMap`, with a sparse
  fallback) built once per run instead of a `BTreeMap` re-probed per jump; each import's `call_bif`
  target is resolved once per run into a per-run `ImportTable` cached in the `LabelMap`. BIF arguments
  marshal through a fixed inline stack array and a reusable per-`Machine` `args_scratch` buffer, so the
  >8-arity path that used to mint a `Vec` per call is now **allocation-free** (pinned by a
  counting-allocator test). `Machine::reset` refills the register files in 128-byte chunks. Measured
  (`tests/engine_bench.rs`, ns per interpreted instruction): `gc_bif '+'/2` **30.7 → 16.3**, a real
  `erlc` recursion **14.9 → 7.3**, a label-heavy loop **167.7 → 8.6**, per-run setup floor
  **~431 → ~242 ns**. Reference semantics remain exactly `Machine::step_instruction`.
- **Area:** `beam_engine` (Stage 1 — JIT)
- **Change:** the baseline (BeamAsm-style) tier now **links local tail calls natively** — `call_last`/
  `call_only` to a labelled compiled block `jmp`s to that block's entry instead of round-tripping the
  driver, resolving the target before popping the frame and preserving the mandatory x-register spill;
  any target without a compiled block deopts to `jit_step_one`. Added additive outcome-aware tier
  entries (`run_export_outcome_with_tier` / `run_label_outcome_with_tier`) that propagate `RunOutcome`
  (including `Wait`) out of a tier run, carrying a suspend across the block ABI as a wait bit, and a
  `CodeRegion`/`code_map()` API exposing the real generated code ranges to the host (for the perf map).
  Tier equivalence is pinned by 736 oracle-checked comparisons over real OTP modules
  (`tests/jit_real_module.rs`) plus a feature-gated Cranelift suite with an empirical deopt proof.
- **Area:** `beam_gc` (Stage 4 — memory model)
- **Change (algorithmic):** added an **on-demand heap-growth seam** (`Collector::ensure_capacity`,
  mirrored on `GenImmixCollector`/`AnyCollector`, with `HeapPolicy::unbounded()` and
  `GcConfig::growable()`): one collection to reclaim, then growth of the empty nursery to the smallest
  curated size covering the request. A copying `collect_major` that could not fit its destination used
  to fail **mid-copy**, leaving the from-space half-forwarded; collections now pre-check capacity and
  return a typed `heap_exhausted` **before** copying, so a failed collection is a no-op on the
  from-space. The immix break-even rule `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS = 2048` is now
  assertion-backed. Per-object overhead measured at **0 words** for cons cells, 2-tuples, floats and
  flat maps; 140 allocate/collect cycles plateau at a 1.000× RSS factor.
- **Area:** `beam_ets` (Stage 3 — data structures)
- **Change (data structure refactoring):** `ordered_set` is now backed by the planned **K-ary B+-tree**
  (`ORDER = 32`, `MIN_OCCUPANCY = 16`): arena-allocated non-relocating `NodeId` nodes, a leaf chain for
  in-order iteration, borrow-or-merge rebalancing, and the separator invariant `keys[i] == max(child[i])`.
  At 10^5 keys the sorted-`Vec` interior it replaces is 3.9–6.3× slower on insert and 3.1–34.6× slower
  on delete (O(n) memmove). The sorted interior is retained as a selectable alternative and both produce
  identical observable traces. `ordered_set` keys are compared by the **arithmetic** order
  `erts_cmp(a, b, 0, 0)` (so `1` and `1.0` are the same key, matching `erl_db_tree.c`'s `cmp_key_eq`),
  and a same-arithmetic-key insert now also updates the stored key term (the key follows the object),
  matching the oracle's `first`/`last`/`next`. Per-object reclamation gained per-object free lists in a
  never-relocating arena; access-control enforcement and the 15-field `info/1` order are pinned.
- **Area:** `beam_dist` (Stage 5 — distribution, contract A.5)
- **Change:** the DOP codec is complete — **all 30 `Dop` variants** round-trip byte-identically, with 30
  golden vectors cross-checked against the real `erl` oracle, plus the full `DFLAG_*` visibility matrix
  (`publish_flag`/`publish_type`/`PeerVisibility`). Delivery classification covers the 16
  message-carrying opcodes with typed spawn/altact/payload-exit flags. A refusing acceptor now *notifies*
  the peer with the exact `not_allowed` frame before erroring (`dist_util.erl`), typed handshake status
  codes are shared between the connection and driver state machines (and the driver's handshake errors
  are terminal), and `epmd` gained `DUMP`/`KILL`/`STOP` with ground-truth-corrected replies (`STOPPED`,
  EOF-terminated dumps). Fragment reassembly/assembly is proven **live** in both directions above 64 KiB,
  as are the `'N'` handshake, `DOP_REG_SEND`, `DOP_SEND_SENDER`, a client-originated `DOP_MONITOR_P`
  answered with `DOP_PAYLOAD_MONITOR_P_EXIT`, `DOP_EXIT2` and `DOP_UNLINK_ID`/`_ACK`.
- **Area:** `beam_abi` (Stage 0 — C ABI, contracts A.3/A.4)
- **Change:** the driver API reached **111/111** LP64 symbols (the NIF API was already 176/176) by the
  three `driver_binary_*_refc` functions, which required a **hidden refcount word** allocated *before*
  the public `ErlDrvBinary` (so the frozen public layout and the C's `ERTS_CHK_BIN_ALIGNMENT` are
  preserved) — the C's `Binary2ErlDrvBinary` trick. The static layout freeze was extended to every public
  type in both headers (including the `ErlNifMapIterator` union interior) and to the full
  enum-discriminant/flag-bit/versioned-constant surface, and a **mechanical conformance test** now parses
  the real headers at test time and asserts symbol set equality, API-table field order, and a
  fully-populated table (proven non-vacuous by fault injection). A real compiled `.so` exporting both
  `driver_init` and `nif_init` is driven under the system ERTS and under this crate and the refcount
  sequences are asserted identical. A use-after-free in the process-global created-thread name registry
  (two inserters racing to own one name buffer) was fixed with first-wins registration.
- **Area:** `beam_io` (Stage 5 — I/O, contract A.4)
- **Change:** `driver_async` moved from one-thread-per-job to a **bounded process-global worker pool**
  (4 threads, handoff by channel, so the `!Send` `Runtime` never crosses a thread), and the registry now
  drives async completions itself so `driver_async` completes with no runtime change. Port streams gained
  `StreamOptions` (`stderr_to_stdout` via an exact `dup2(1,2)`, `eof` surfaced by a polling query rather
  than a new notification variant), `ERL_DRV_USE`/`stop_select` is honoured on deselect and teardown, and
  `tcp_inet` latches peer close to the C's `TCP_ADDF_DELAYED_CLOSE_RECV/_SEND` semantics. The two
  `open_port` divergences were closed: a `spawn_executable` directory now opens the port via a faithful
  fork/execve with the child reporting its errno (`{exit_status, 13}`), and an empty name is `badarg`.
- **Area:** `beam_process` (Stage 5 — processes)
- **Change:** the run queue became **four physically distinct per-priority FIFOs** with highest-first
  selection (a process born at `max` is dequeued ahead of normals), process-dictionary keys/values are
  compared **structurally** rather than by raw `Eterm` word, and the process-control surface was filled
  in: dead-pid monitors mint a reference and deliver an immediate `noproc` DOWN, port monitors and named
  monitors gained their own side tables, `demonitor(Ref, [flush])` removes a queued DOWN, and
  `register/2`/`unregister/1` follow the oracle's `badarg` rules. Reduction-quantum fairness is proven
  (2 processes share 10 slices as `[5,5]`; 4 as `[3,3,2,2]`, budget-invariant).
- **Area:** `beam_rt` (Stage 5 — observability, contract A.8)
- **Change:** adopted the new `MemoryHook`/`RuntimeHooks` seams. `erlang:memory/0,1` now return **real**
  runtime-measured figures (process heaps, atom table, live binary bytes, loaded `.beam` sizes, ETS)
  in the oracle's exact key order, and `start_timer/3,4`, `send_after/4`, `cancel_timer/2`,
  `function_exported/3`, `link/1` and `unlink/1` are real BIFs. The tracing subsystem emits the `send`
  and `receive` events (gated on sender/receiver respectively) plus the seq-trace event, and implements
  the probed **tracer-equals-tracee discard** rule. `erlang:halt/0,1,2` with a crash-dump writer and the
  `ERL_CRASH_DUMP` path, and a `+JPperf` perf-map writer, were added. The term-heap default rose to
  128 MiB (`1 << 24` words) with exhaustion translated to **`error:system_limit`** instead of a host
  `allocation failure`.
- **Area:** `beam_term` (Stage 0 — terms)
- **Change:** totality/fallibility split hardened — a non-term word is never dereferenced. Fallible APIs
  return `TermError::NotATerm`; total APIs give a defined answer (`class` ⇒ `NonValue`, `size_object`
  ⇒ 0, `number_as_f64` ⇒ `NaN`, `term_hash` ⇒ a tagged raw-word hash) and comparison orders a non-term
  after every valid term. The frozen layout is untouched.
- **Area:** `tools/differential`, `tools/bench`
- **Change:** the corpus grew from 667 to **723 cases** (708 → 715 agreeing), adding pinned cases for
  `erlang:memory/0,1` shape/key order, the newly registered BIF families, node identity, the
  `ordered_set` arithmetic-key rule, and the `+hmax`/`+hms` switch family — classified deliberately as
  agree or as an honest typed `unsupported`, never as a mismatch. The benchmark harness now separates
  the compute loop from the memory probe so an unimplemented capability degrades to an explicit
  `not-run (unsupported: …)` note instead of aborting a row, and the Rust-vs-oracle per-call gap was
  reproduced by hand and root-caused (above).

## 2026-10-07 — Milestone 2: real OTP boot, full C-ABI symbol surface, CHAMP/GenImmix, live distribution, Stage-2 passes

Scope: the Rust ERTS core advances from "vertical slice" to a runtime with a real boot sequence,
complete NIF/driver symbol tables, an adopted Stage-3 data structure, a wired Stage-4 collector
choice, a live distribution handshake, and a landed Stage-2 compiler-pass framework. Verified:
`cargo build --workspace --all-targets` warning-free, `cargo test --workspace` **5081 tests / 0
failures**, and the differential gate **`total=652 agree=651 mismatch=0 unsupported=1 skipped=0`**
(the single `unsupported` is the documented `erl_eval`-lambda encoding gap).

- **Area:** `beam_ets` (Stage 3 — data structures)
- **Change (data structure refactoring):** `set` tables now default to a **CHAMP** (compressed
  hash-array mapped prefix-tree) open-addressing interior (`champ_interior.rs`) instead of the
  chained-bucket index. 5 hash bits per level; each node compresses children into a
  `datamap`/`nodemap` bitmap pair with inline slots; a full 64-bit `term_hash` match falls back to a
  collision node; removals canonicalize by **inlining** single-entry sub-nodes (pulling a sub-node up
  would misroute every key, since a sub-node is indexed by the *next* 5 hash bits). The chained
  `HashInterior` stays the index for `bag`/`duplicate_bag` and is selectable for `set` via the
  crate-internal `SetInterior`. Measured (`interior_bench.rs`, smallint/atom/binary/tuple keys at
  1e1/1e3/1e5/1e6 entries): CHAMP inserts 2–10× faster, iterates up to 5× faster, stores 2–7× fewer
  index bytes/entry (28–63 vs 105–207 B), reaches lookup-hit parity on binary/tuple keys; it loses
  only on delete (1.6–2.8×) and small-int lookup-miss (up to ~2.1×). No observable `ets:*` change:
  `interior_parity.rs` pins identical traces for both interiors at `keypos` 1 and 2.
- **Area:** `beam_ets` (Stage 3 — concurrency and reclamation)
- **Change:** Tables became `Send + Sync` and shareable across processes via `SharedTable`
  (`Arc<RwLock<Table>>`; a poisoned lock is a typed `unsupported_ets` error, never a panic). One
  `write()` guard makes `insert_new`/`update_counter`/`select_replace` atomic (no lost updates).
  `ets:safe_fixtable/2` fixation is modelled as **deferred reclamation**
  (`Table::{set_fixation,is_fixed,deferred_objects}`): storage for removed objects is held in
  `deferred_frees` while fixed — `delete_all_objects` defers rather than resetting the arena — and
  released at unfix, so `ets:info(T,memory)` is held while fixed and falls at unfix, matching ERTS.
  `TermStore::copy_term` shares exactly the global empty-tuple literal (C `copy.c:130-140`).
- **Area:** `beam_gc` (Stage 4 — GC modernization)
- **Change (algorithmics):** the GenImmix mature-generation option gained a **measurement-derived
  automatic rule** (`selection.rs`): `IMMIX_BREAK_EVEN_WORDS = 8 * BLOCK_WORDS` (2048 words = 8 whole
  Immix blocks). `GcConfig` carries `MatureSelection::{Auto,Explicit(MatureKind)}` (default `Auto`);
  `AnyCollector::from_config` dispatches on the resolved strategy and exposes an inspectable
  `MatureChoice { kind, source, mature_words, break_even_words }`. `GcConfig::default()` still
  resolves to the copying path (the 233-word default mature generation is far below the break-even),
  so default collector behaviour is unchanged. Re-measured by a single-variable sweep
  (`immix_break_even.rs`, a purely long-lived corpus at 0.25…64 whole Immix blocks in two shapes
  under a forced fullsweep): below one whole block Immix pins nothing and is **strictly dominated**
  (5.3× moved volume and footprint, 2.0–2.7× pause); from one block up it pins the whole live set
  (moved words → 0) at a flat ~2.2× pause, and its worst-case benefit/cost ratio plateaus at 8
  blocks. `mature_selection.rs` proves the rule selects Copying below and Immix at/above the
  break-even, that an explicit override wins both ways, and that a threshold-crossing corpus yields
  identical collection-kind sequences and structurally identical reachable-object sets.
- **Area:** `beam_dist` (contract A.5 — distribution)
- **Change (protocol/layering):** the server-side handshake is now **proven live** against a real
  OTP-29 peer (`tests/live_accept.rs`): `'N'` flags/creation exchange, MD5 challenge, `'sok'` +
  `'N'` challenge + `'a'` ack, 2-byte→4-byte framing, and the no-`Complement` rule are byte-verified
  against `erts/emulator/beam/dist.c`. The root cause of the earlier failure was **not** the codec but
  the *advertised node name*: OTP's `dist_util.erl::recv_challenge_new/2` does
  `list_to_existing_atom(Ns)` and compares against the dialled node, so a server advertising a bare
  short name is aborted mid-handshake and the Rust side sees a short read ⇒ `badarg`. Inbound
  control messages now have a semantic seam: `delivery.rs` defines `Delivery`
  (`Send`/`RegSend`/`AliasSend`/`Link`/`Exit`/`UnlinkId`/`Monitor`/`Demonitor`/`MonitorExit`/
  `GroupLeader`/`Other`, field order taken verbatim from `dist.c::erts_dist_command`),
  `Delivery::classify`, and a **defaulted `InboundSink` trait** whose default returns the typed
  `DeliveryOutcome::Unsupported` — so the semantic routing lives in `beam_rt` without adding a
  dependency edge.
- **Area:** `beam_abi` (contracts A.3/A.4 — the frozen C-ABI symbol tables)
- **Change:** the NIF surface is now **176/176 `ErlNifApi` slots populated** and the driver surface
  **108/111**, split one-file-per-family under `src/nif/enif/` and `src/driver/drv/` (each family a
  `pub(super) const fn populate(api)` chained innermost→outermost). Added families: resource types +
  destructor, monitors, dirty schedulers, `enif_send`, `select`, stdout/`*printf`, `enif_alloc_env`,
  `ioq`, map iterators, thread/mutex/cond/rwlock/tsd (over a new in-crate `src/sync/` pthread
  facade), `driver_*` port-queue/PDL/monitor/thread/lifecycle families. The four C **variadic** entry
  points cannot be defined by stable Rust (`rust-lang/rust#44930`) and are provided by a real C shim
  (`csrc/nif_variadic.c`, compiled by `build.rs`), not a stub. New mechanisms: a **thread-local
  current-host stack** (`src/driver/current_host.rs`, `beam_abi::with_current_host`) so port-free
  slots can reach their host (this is exactly C's precondition for `enif_alloc_env`), and
  `ERTS_INVALID_ERL_DRV_PORT`. NIF `.so`s resolve `enif_*` out of the loading binary's `.dynsym`,
  which requires `-Wl,--export-dynamic` (kept in the relevant crates' link args and verified with
  `nm -D`). Fixed along the way: an `enif_map_iterator_destroy` null-deref (a missing
  `is_term_word` guard), the `enif_make_resource`/`enif_get_resource` round trip (the term was minted
  through a defaulted host hook and raised `badarg`; it is now minted in-crate as the frozen
  internal-reference block), and `enif_is_process_alive` on self (now answered from `self_pid`
  first, as C's `erts_proc_lookup` does).
- **Area:** `beam_io` (contract A.4 — driver host services)
- **Change:** a process-global driver registry `DriverTable` (`src/abi/table.rs`, the C's one
  `driver_list`): `add_entry` (runs `entry.init` first; non-zero ⇒ the `driver_init_failed` atom and
  nothing recorded), `add_loaded`, `remove_entry` (safe — pointer comparison only, never
  dereferenced), `lock_driver`, `lookup`, and `create_port(&mut PortRegistry, &mut Poller, …)` which
  releases the mutex before running `start` and keeps the `ErlDrvPort` handle stable for the port's
  whole life via `DriverPort::from_library`. `beam_io → beam_abi` (+ `beam_alloc`) is a legal
  wave-5→wave-0 edge.
- **Area:** `beam_bifs`
- **Change:** a real **`bif.tab` surface** (`src/bif_tab.rs`) parsing the actual grammar of
  `erts/emulator/beam/bif.tab` (`bif`/`ubif`/`hbif` + `module:name/arity` + an ignored trailing
  `C-name`, whole-line `#` comments, quoted operator names unquoted to the registry key, and
  `name/lo-hi` arity-range expansion). It is **validation only** — BIF dispatch is unchanged (the
  registry is a hand-written table of `fn` pointers). Measured census: declared **556** (`bif` 476 /
  `ubif` 73 / `hbif` 7), implemented **241**, missing **315** (enumerated, never fabricated);
  registry 320 = 241 declared + 79 stdlib fast paths. Also fixed the `lists:usort/2`
  **wrong-result** bug (the ascending split's `Y < Z` branch omitted `x = y`, duplicating an element:
  `usort(A =< B, [1,2,3])` returned `[1,1,3]`) and added an additive `LocalFunIdentity` seam so a
  compiled module's `make_fun3` closure encodes to a byte-exact `NEW_FUN_EXT`.
- **Area:** `beam_loader` + `beam_engine` (Stage 1)
- **Change:** the loader's coverage is now measured rather than estimated (`Module::records()`,
  `Recs` chunk exposure, literal-area decode incl. compressed form / `EXPORT_EXT` / `NEW_FUN_EXT` /
  bignums / >32-key maps): **1366/1366 installed `.beam` across 34 applications load with 0
  failures**, and the residual `Instr::Generic` census over 261 modules / 793 319 instructions is
  **empty (0 distinct opcodes)** — the loader lowers every compiler-emitted opcode 1:1. The
  "blocked" `Instr` variants are mostly synthetic-probe artifacts; the genuine residue is host-seam
  work owned by `beam_rt`/`beam_bifs`. New read-only execution-position accessors
  (`src/machine/location.rs`): `current_code`, `entry_call`, `current_function`, `initial_call`,
  `current_location`, `current_stacktrace` — written at the single choke point shared by the
  interpreter and the native tiers' deopt helper, so the address is exact at every suspension.
- **Area:** `beam_engine` (Stage 1 — the execution-engine seam)
- **Change (architecture):** the JIT behind the typed-IR seam gained a block register allocator
  (r12–r15 + rbp), a cross-run `CodeCache` keyed by `ModuleKey`, direct block linking, and
  comparison inlining (measured 2.4×–14.9× on the affected blocks). The engine's largest source file
  was split from 75513 to under 911 lines across 7 responsibility modules. Coverage census: `Instr`
  106 = 49 executed + 57 typed-blocked + **0 unclaimed** (`unclaimed == 0` asserted);
  `TestKind` 25/25. Correctness-first is preserved: the JIT deopts into `jit_step_one` rather than
  ever producing a wrong result.
- **Area:** `beam_rt` (contracts A.6/A.7/A.8 — the runtime binary)
- **Change (A.7 — boot):** `Runtime::boot()` no longer *names* the preloaded modules, it **loads
  them**: `src/boot/preloaded.rs` loads all 22 C-preloaded `.beam`s from the erts app `ebin`, and
  `src/boot/bootscript.rs` is a real `start.boot` reader + executor (`BootScript`/`Instruction` over
  `beam_codec::decode_term`) with `boot_from_script`, `load_code_module`, `add_code_path`,
  `register_boot_process`. `erts_internal` grew from 1 to 13 names and `prim_file` from 0 to 11
  entries, each pinned against the oracle. The real `start.boot` executes directives 1–15 and
  honestly stops at instruction 16 `{apply,{application,load,[…]}}` (no `application_controller`
  yet) with a typed error; a synthetic `preLoaded`+`module`+`apply` script completes end-to-end.
- **Change (A.6 — CLI/configuration):** the flag surface went from ~12 to full coverage
  (`+Q`/`+P`/`+K`/`+sbt`/`+A`/`+hms`/`+hmbs`/`+hmax*`/`+M*`/`+MM*`/`+MB*`, `-config`, `-setcookie`,
  `-boot`, `-emu_flavor`), and the real `ERL_*FLAGS` precedence chain is implemented
  (`[ERL_OTP<sysver>_FLAGS] [ERL_AFLAGS] [cmdline] [ERL_FLAGS] [ERL_ZFLAGS]`) with the C's
  `build_args_from_string` tokenizer. `+Q N` now **drives** `system_info(port_limit)`
  (`min(next_power_of_two(request), 134217727)`, default 65536) instead of pinning a constant, and
  `ERL_MAX_PORTS` is routed through the same channel.
- **Change (A.8 — introspection):** `system_info/1` went from 16 to 63 oracle-accepted keys and
  `process_info/1,2` from 0 to 27 items, with oracle-pinned value **and shape** equality for 38
  comparable keys; the family was split into `src/info/{system,limits,schedulers,memory,unsupported}.rs`.
- **Change (execution):** the `run` action now drives the **full scheduler**
  (`Runtime::run_scheduled`, bounded `RUN_DEADLINE`/`RUN_STEP_SLICES`) — run queue + ports +
  mailboxes + timers — so a blocking workload (an A.4 driver/port echo round-trip) completes instead
  of failing with `unsupported: … blocked`; the runner records the original `BeamError` per pid so
  the raised class and the exit-3 capability-gap contract are preserved. The scheduler/run-pass
  family was extracted from `src/runtime.rs` into `src/runtime/scheduler.rs` (runtime.rs 2250 →
  1241 lines).
- **Area:** `beam_rt` (distribution) + `beam_message`
- **Change:** the node now advertises the **full `node@host`** on the wire (a bare short advertised
  name is refused by a real peer), so a real `erl`'s `net_kernel:connect_node/1` returns `true`
  against a running `beam_rt` binary (proven in-process and through the real binary).
- **Change (correctness, `beam_message`):** `src/layout.rs` no longer sizes/walks fun blocks with the
  legacy `ERL_FUN_SIZE` (env at offset 2); it derives both from the frozen `beam_term::fun` layout
  (`env[0]` at `1 + desc_words(kind)`, block = `fun_words(kind, num_free)`), matching what
  `beam_gc` and `beam_term::size` already do. The legacy math made `copy_struct` mis-size a closure
  crossing a copy boundary (verified to abort with a null dereference in `beam_term/src/list.rs`).
- **Area:** `lib/compiler` (Stage 2 — compiler optimization)
- **Change (framework):** a per-pass **time budget + watchdog + eligibility/ordering hook**
  (`src/beam_ssa_opt_budget.erl`, options `ssa_opt_budget` / `ssa_opt_watchdog` / `ssa_opt_gates`).
  With the options off the pass funs are literally unwrapped, so default compilation is
  byte-identical. Budget truncation skips a **monotone suffix** of optional passes (less optimized,
  equally correct); a hard per-pass watchdog kills a runaway pass in a monitored child and **aborts**
  rather than silently skipping (silent skipping was measured unsound — a non-deterministic pass
  subset). Measured on `lib/stdlib/src/lists.erl`: default median 305 ms, `{ssa_opt_budget,5}`
  median 243 ms.
- **Change (passes):** `beam_ssa_loop_opt.erl` gained strength reduction (`X*2^n`→`bsl n`, `X*2`→
  `X+X`, `X div 2^n`→`bsr n` under integrality/non-negativity proofs) and induction-variable
  canonicalisation; `beam_ssa_fus.erl` gained multi-key map-key test-chain elimination (the
  redundant `has_map_fields`/`is_map_key` chain is collapsed and the multi-pair `get_map_elements`
  is still synthesised by `beam_block:opt_maps/2`); `beam_ssa_fbip.erl` was audited. All passes stay
  **opt-in/OFF** because global equivalence was not proven. FBIP widening to maps was investigated
  and found **infeasible compiler-only**: no map-update instruction in `ops.tab`/`genop.tab` (emu or
  JIT) carries a `copy|reuse|inplace` hint, `beam_ssa_codegen.erl:2398` emits copying
  `put_map_assoc` unconditionally, and `erl_map.c` always allocates — so the emitted map assembly is
  byte-identical with and without `+fbip_opt` (control: the same build still rewrites `update_record`
  `copy`→`inplace`, proving the flag path is live). It is an emulator+runtime feature, not a
  compiler-only widening, and is recorded as such rather than faked.
- **Area:** `tools/differential` (the compatibility gate)
- **Change:** the corpus grew 431 → **652** cases with a new A.4 **runtime** group (`driver_c.rs`,
  a real C driver `.so` driven through its port lifecycle — A.4 previously had no runtime case at
  all), A.3 resource/ioq/sync cases, A.6 flag/env cases, and A.8 `system_info`/`process_info` cases,
  all with oracle-derived goldens. A new `CaseKind::EvalEnv` + `runtime::run_with_env` expresses
  environment-variable cases **without** touching the existing `CaseKind::Eval` literals, and pins
  the `ERL_*FLAGS` precedence chain (including a single-valued switch proving a later source
  overrides an earlier one).
- **Area:** `tools/bench`
- **Change:** a new `foreign` workload measures the **Rust runtime** on real target-project code for
  the first time (Cowboy `cowboy_bstr` and EMQX `emqx_base62`, compiled once with the system `erlc`
  and run under both runtimes). Measured (startup-corrected ns/call, `--quick`): `to_lower` sys
  437.79 vs Rust 268 618.72; `emqx_base62:encode` sys 175.74 vs Rust 2 092 849.08; startup
  comparable (~130–160 ms both) and peak RSS 70.9 MB (sys) vs 28.8 MB (Rust). The gap is execution
  (today a tree-walking interpreter vs the JITed BEAM), reported honestly as a real result.
  `khepri` runs via `$BENCH_TRY_NIX` (`rebar3 eunit` 77 340 ms, peak RSS 357.6 MB); `plb` and `emqx`
  remain honest skips with the true reasons recorded.
- **Area:** legacy C removal
- **Change:** three genuinely dead C files removed (`git rm`, with `rg` evidence over every
  `Makefile*`/`*.mk`/`*.in`/`*.tab`/`*.am`/`configure*`/`otp_build`/`*.sh`/`*.c`/`*.h`):
  `erts/emulator/drivers/unix/multi_drv.c`, `erts/emulator/drivers/unix/sig_drv.c`,
  `erts/emulator/drivers/vxworks/vxworks_resolv.c`. `bin_drv.c` was **not** removed: the same sweep
  found live references in `erl_nif.c`, `dist.c`, `io.c`, `erl_bif_info.c`, and `erl_binary.h`, so
  the earlier "dead file" claim was wrong for it and the file stays.

---

## 2026-09-01 — Milestone 1: crate tree, frozen term layout, typed-IR engine, ETS/GC/messaging interiors

- **Area:** `erts_rust/crates/*`, `erts_rust/tools/*`
- **Change (foundation):** Froze the **term representation** in `beam_term` as a bit-exact port of
  `erts/emulator/beam/erl_term.h`: 2-bit primary tags, 60-bit small integers, 6-bit-shifted atoms,
  header-less 2-word cons cells, boxed sub-tags, and `TAG_LITERAL_PTR` literal pointers. `Eterm` is a
  `#[repr(transparent)]` newtype over `usize`, ABI-identical to a NIF's `ERL_NIF_TERM`. All term
  construction/inspection is centralized here (single source of truth for every other crate).
- **Area:** `beam_ets` (Stage 3 data structures)
- **Change (data structures):** ETS keeps the immutable **observable** `ets:*` semantics (key
  equality/ordering, duplicate-key winners, `size`, lookup/select results) while the **interior
  layouts are opened up** (Part II §B/§D). `set`/`bag` use a bucket hash index; `ordered_set` uses a
  key-sorted interior — the legacy **AVL tree is replaced** by a sorted structure in M1 and is slated
  to become a **K-ary B+-tree** (Part II §B.3). Object storage is a never-relocating table-owned bump
  arena (the copy boundary); deletion does not yet reclaim blocks (MSO planned).
- **Area:** `beam_gc` (Stage 4)
- **Change (algorithmics):** Implemented the per-process **generational semi-space Cheney copying**
  collector, preserving the **barrier-free** generational invariant (a free consequence of term
  immutability), with a refcounted **MSO** off-heap sweep. MMTk/Boehm are rejected; GenImmix is a
  measurement-gated future option (Part II §D).
- **Area:** `beam_message`
- **Change (algorithmics):** Implemented the `copy_struct` copy engine + send/receive envelope — the
  per-process-heap copy boundary (Part II §D). Messaging across heaps remains copy-based by design.
- **Area:** `beam_loader` + `beam_engine` (Stage 1 — the execution engine)
- **Change (architecture):** The loader lowers `.beam` to a **typed, block-structured IR**; backends
  consume only the IR. This is the pluggable **execution-engine seam** (Part II §A/§F, "Ruby-YJIT
  shaped"). A **direct-threaded interpreter** implements an honest generic-opcode subset; the
  **BeamAsm-style baseline JIT** and the **optional Cranelift hot tier** sit behind the same seam and
  are measurement-gated / off the critical path. Root-set contract kept: `x_regs[0..live)` + the whole
  stack, **no frame liveness maps**.
- **Area:** `beam_dist`, `beam_abi`
- **Change:** Distribution protocol v6 (`DFLAG_*`/`DOP_*`, handshake, control framing, `epmd` client)
  and the layout-frozen NIF (`enif_*`) / driver (`driver_*`/`erl_drv_*`) C-ABI surfaces, with
  static size/offset assertions and `Eterm ↔ ERL_NIF_TERM` transmutes.
- **Area:** `beam_rt`
- **Change:** The runtime binary (boot, CLI flags, `system_info`) wires the engine + BIFs + process
  context and executes a real `.beam` end-to-end (the vertical slice): `beam_rt run slice.beam
  slice:add/2 2 3` → `5`, `slice:fib/1 10` → `55`.
- **Area:** build system
- **Change:** Developer-iteration build configuration: `mold`/`lld` linker, `opt-level=1` dev profile
  with dependencies at `opt-level=2`, thin-LTO release, and a Nix flake pinning the toolchain plus the
  oracle/target runtimes. `scripts/cargo.sh` selects a writable `CARGO_HOME` for read-only-`$HOME`
  sandboxes.
- **Area:** `tools/differential`, `tools/bench`
- **Change:** The differential compatibility gate defines a **capability-gap contract** (runtime exits
  `3` with `beam_rt: unsupported:` for an unimplemented opcode/BIF/`-eval` form) distinct from a real
  runtime error, so an incapable runtime is reported `unsupported` (honest), never `mismatch`; new
  module-run cases give real oracle-vs-Rust agreement. The benchmark driver gains a **read-only
  target adapter** (build in a `$TMPDIR` copy of a read-only foreign repo) and a **dependency-free
  HTTP/1.1 load generator**, so it measures Cowboy, the Elixir/`erlc` compilers, and the system-ERTS
  baseline with real numbers.

## 2026-09-01 — Bootstrap: Rust ERTS rewrite workspace established

- **Area:** repository root / `erts_rust/`
- **Change:** Introduced a new Cargo workspace, `erts_rust/`, as the from-scratch Rust
  reimplementation of ERTS (the BEAM runtime). It supersedes the legacy C/C++ emulator for the
  new build path; the legacy tree under `erts/` is retained as the **differential-test oracle**
  and the `FLAVOR=emu` bootstrap fallback, to be **removed module-by-module** as Rust parity is
  proven by the differential harness.
- **Why:** The rewrite targets full drop-in compatibility with Erlang/OTP (`30.0-rc0` / `erts-17.1`),
  higher robustness (safe periphery), a modern codebase, and improved speed + memory. The staged
  decomposition (Stage 0–6) comes from `ERTS_RUST_REWRITE_EVALUATION.md` (Part I) and
  `ERTS_RUST_REWRITE_DEEPDIVE.md` (Part II).
- **Structure:** 15 library/binary crates + 2 tooling crates, decomposed along the BEAM subsystem
  seams documented in Part II §A/§H. Cross-crate contracts (frozen term representation, error
  strategy, IR seam, root-set contract, differential-test gate) are fixed at the workspace level in
  `erts_rust/CONTEXT.md`.
- **Tooling:** Nix flake (`erts_rust/flake.nix`) pins the dev environment; `.cargo/config.toml`
  configures a fast linker and fast dev build profiles for iteration speed.
