# `beam_rt/src/native/memory/` — `erlang:process_info/1,2` (contract A.8)

## Intent
The per-process introspection surface: `erlang:process_info/1,2` is a **runtime** operation, read out of this runtime's own `beam_process` table, so the floor serves it before the `beam_bifs` registry is consulted.
The dispatch is `../native.rs::try_native`'s (the one dispatch table — this node adds no second list); the body is `RuntimeContext::process_info`/`snapshot` in `mod.rs`.
`binary.rs` builds the `binary` item's off-heap refc-binary roster.
`tests.rs` is the colocated unit suite; `binary_e2e.rs` is the end-to-end case against the live oracle over a test-time-compiled `.beam`.
`mod.rs`'s module docs are the authoritative per-item table (every served item, each typed `unsupported` item's reason, the divergences, the oracle probes); the sections below are the load-bearing summary.

## API Surface
### Reply shapes (oracle OTP 29.1.1 / erts-17.1)
* `/2` with an item **atom** → `{Item, Value}`, **except** `registered_name`, whose value is the **bare** name atom (the bare `[]` when unregistered — the C's `always_wrap = !is_atom(opt)`, `erl_bif_info.c:1556`).
* `/2` with `{dictionary, Key}` → `{{dictionary, Key}, Value | undefined}`.
* `/2` with a **list** → `[{Item, Value}, …]` in the requested order, duplicates repeated, `registered_name` **wrapped** (`{registered_name, Name}` / `{registered_name, []}`).
* `/1` → the fixed `PI_1_KEYS` set (C `pi_1_keys[]`, `erl_bif_info.c:1008-1026`), in order `registered_name, current_function, initial_call, status, message_queue_len, links, dictionary, trap_exit, error_handler, priority, group_leader, total_heap_size, heap_size, stack_size, reductions, garbage_collection, suspending`.
  A **registered** process emits all 17 (`{registered_name, Name}` first), an **unregistered** one **omits** that key and emits the remaining 16 (the C's multi-item loop drops a bare `[]` value, `erl_bif_info.c:1175`).
  An item this runtime cannot answer is **omitted** by that same mechanism, which is how a process whose execution position this runtime cannot read loses the two engine-derived keys `pi_1_keys[]` asks for.

### Failure modes
* A non-atom / non-list / non-`{dictionary,_}` second argument ⇒ `error:badarg`.
* An unknown item name (also **inside** a list, or an improper list) ⇒ `error:badarg`, **even** for a non-live pid.
* A non-internal-pid first argument ⇒ `error:badarg`.
* A **valid** item with a live-but-absent pid ⇒ the atom `undefined` (all three forms; the list form of a dead pid is `undefined`, *not* `[]`).

### `binary` — `binary.rs`
The process's **live** off-heap refc binaries as `{Ptr, Size, Refc}` 3-tuples, `[]` when none.
The roster is the **deduped union** (by payload pointer) of the target's **engine reservoir** (`RunnerState::reservoir_at`, the machine's `refc_binaries`, the payloads the process's own compiled bitstring code built) and the process's own off-heap table (`Process::off_heap_binaries`, released slots skipped — the path for a host that registers a payload directly, which no production site does).
The reservoir is **pruned at the slice boundary** whenever the process's binary virtual heap triggers (`../../context/gc/bin_vheap.rs`), so a payload the process has released drops out of this answer instead of being listed to the end of its life.
The roster has **one read route per query shape**: another pid's from `RunnerState::reservoir_at`, the **executing** pid's from the in-machine BIF door (`RuntimeContext::in_machine_off_heap_binaries`, `../../context/position.rs`, because a pass holds the running machine *out* of `RunnerState` for the whole slice), so `process_info(self(), binary)` lists the payload the process built earlier in the very slice that asks.
Both routes are rendered by the **one** `binary_value` builder.
This is the per-process roster; `erlang:memory(binary)` is a different observable (`binary_alloc`'s live bytes).

### `links` and `monitored_by` — the local + inbound-wire union
ERTS keeps **every** link — local process/port and remote process/port — in the one link tree `ERTS_P_LINKS(prt)`, and `collect_one_link` (`erl_bif_info.c:575-593`) collects all of them except a link whose `unlinking` flag is set, including `ERTS_LNK_TYPE_DIST_PROC`/`ERTS_LNK_TYPE_DIST_PORT` whose `other.item` is the **external** pid/port.
The `am_links` arm (`erl_bif_info.c:3689-3707`) then builds a **bare list of pid/port terms** (no tuples), so a process a peer linked reports that peer.
This runtime keeps the two halves in two stores — the local half in `Process::links` (internal pid immediates), the inbound remote half in `ProcessTable::remote_links` as decomposed `RemotePid`s — and the `links` item is their concatenation (`RuntimeContext::links_value`, the `monitored_by` precedent in `monitored_by_value`).
A `RemotePid` is all-immediate (sysname **atom** term + number/serial/creation), so nothing points into another heap, but rendering one as an *external* pid needs the four-word `ExternalThing` block this crate may not write (`#![deny(unsafe_code)]`), so the remote entries are materialized on a scratch `beam_alloc::Heap` via `RemotePid::to_eterm` and the finished list copied out through `rehome_container_result` — the fast path (no remote links) stays `terms::make_list` on the reply heap.
**Order: local links first (insertion order), then the remote ones (recording order).** The oracle does **not** pin this item's order: the C emits the traversal's reverse (it conses onto the head) and `*_rbt_foreach`'s order is documented undefined (`erl_rbtree.h:164-169`), even though the tree's keys compare by `ml_cmp_keys` (internal pid < internal port < external pid < external port). A caller must treat the item as a set, which is what `lists:member/2` does.
`monitored_by` has the identical two-store shape (`Process::monitored_by` + `ProcessTable::remote_watchers`), and both items share the documented ordering caveat.
The pin is `tests.rs::the_links_item_unions_a_local_link_with_an_inbound_distribution_link` (shape only — a foreign pid cannot be printed by this runtime, and the oracle's `<Channel.Creation.Serial>` text is not pinnable across VMs) and the sibling `the_monitored_by_item_renders_a_local_and_a_wire_origin_watcher_apart`.

### `garbage_collection` and `max_heap_size`
`garbage_collection` is the 5-item proplist `[{max_heap_size,Map},{min_bin_vheap_size,N},{min_heap_size,N},{fullsweep_after,N},{minor_gcs,N}]` in `beam_gc::GARBAGE_COLLECTION_KEYS` order (a **map** for `max_heap_size`, unlike `system_info`'s bare integer).
`max_heap_size`'s value source is `Process::max_heap_size()` when the pid's setting was set explicitly (`Runtime::max_heap_was_set`), else the **birth default** `Runtime::birth_max_heap_setting(&policy)` — the `+hmax` bound (`HeapPolicy::max_heap_words`) with the `+hmax*` flag word — so an explicit `#{size => 0}` stays distinguishable from a never-set one.
Boot/system processes get `max_heap_words = 0` (`Runtime::system_spawn_policy`, selected by `Runtime::default_spawn_policy` inside the `Runtime::booting` window), so `+hmax` bounds user code and cannot cap the boot.
`minor_gcs` is `Collector::minor_since_major`, **not** a collection total (a fullsweep resets it to `0`); the monotone totals are `Runtime::gc_stats().collections` and `beam_gc::AnyCollector::stats()`.

### Execution-position items and `catchlevel`
`current_function`/`initial_call`/`current_location`/`current_stacktrace` and `catchlevel` come from the engine's read-only accessors via `RuntimeContext::position_value`/`catchlevel_value`, sourced in order from the runtime's **root execution frame** (`../../runtime/executing.rs`), the **in-machine BIF door**'s published slot (`../../context/position.rs`, the executing process's own query from compiled code), then `RunnerState::position_engine_at(pid)`.
A **fun-bodied** process with no resolved local target, a never-scheduled pid and a **direct/no-machine** caller report `Ok(None)`, so `/1` **omits** the item and `/2` answers the typed `unsupported`.
Residuals: the door's `initial_call` is the process's **entry MFA** where the oracle answers `{erlang,apply,2}` for some spawn shapes, and the slot is runtime-wide, so a **nested** fun-applied run reads the **outer** machine's position (`../../../tests/position.rs`).

### Honest typed `unsupported` items
Valid oracle names whose gap reason names the missing accessor/hook: `backtrace` (no raw/binary stack-dump accessor), `magic_ref` (the NIF resource API is unbound), `parent`/`label` (no such PCB field), `priority_messages` (no such `beam_process::ProcessFlags`), `garbage_collection_info` (`beam_gc::HeapInfo` measures no per-generation block sizes).

### The heap-size items are oracle figures, not this runtime's own
`heap_size` is `process.collector().heap_size_words()` (`mod.rs:1372`) — the **capacity** of the **whole nursery chain** (`beam_gc/src/nursery.rs:247`: the current space *plus* every space a growth retired), never the live occupancy and never one space alone.
`total_heap_size` (`mod.rs:1380-1381`) adds `old_heap_size_words()` — the mature generation's capacity **only once a collection has created it** (`beam_gc/src/collector.rs:666`, the C's `erl_bif_info.c:1023-1030`), so a fresh process reports `total_heap_size == heap_size` even though its collector already owns an empty mature space.
The **accounting** figure is `HeapObservable::total_heap_size_words()` = nursery-chain capacity + mature capacity (`beam_gc/src/collector.rs:659`); that, *not* the `process_info` item, is what `erlang:memory(processes)` and the `max_heap_size` door read (`beam_gc/src/heap_info.rs:128-136`).
`heap_block_size` is not an `erlang:process_info/2` item at all: it exists only inside the oracle's `garbage_collection_info` map (with `old_heap_block_size`/`mbuf_size`/`recent_size`/`bin_vheap_*`), which is this node's typed `unsupported` item (`mod.rs:358-362`).
`memory` is this runtime's own byte total — `(total_heap_size + stack_size)*8 + off_heap_bytes` (`mod.rs:1025-1026`), the last from `Process::off_heap().overhead_bytes()` (`mod.rs:1396`); `garbage_collection` is `HeapInfo::new` over those same two heap items plus the policy (`mod.rs:1168-1190`); `binary` is the off-heap refc roster (`binary.rs`), a different observable from `erlang:memory(binary)`.
Both heap items are pinned against the live oracle by `../../../tests/min_heap_size.rs`.

### What these figures do NOT see — the `erlang:memory` accounting boundary
`erlang:memory/0,1` is *not* served here: `beam_bifs` renders it (`beam_bifs/src/runtime/memory.rs:86,103`) over the `MemoryHook` seam, whose only producer is `crate::context::memory_hook`. The two surfaces answer different questions, so know the boundary before comparing them.
- `erlang:memory(processes)` = Σ over live processes of `total_heap_size_words()` × 8 (**capacity**: nursery chain + mature), `processes_used` = young-chain used + mature used (`../../context/memory_hook.rs:135,140-141`).
- `memory(binary)` = `binary_alloc` live bytes (**refc binaries only**), `memory(code)` = the **on-disk** `.beam` sizes of loaded modules, `memory(ets)` = 8 bytes per live table, `memory(atom)` = index slots + name text, `memory(system)` = those plus every registry allocator's `live_bytes` **excluding `Binary`/`Standard`/`EHeap`**, `total` = `processes + system` (`memory_hook.rs:160-191`).
- **Everything allocated through `std_alloc` is structurally excluded** (that exclusion is what keeps the process heaps from being counted twice): so the runtime's non-process `beam_alloc::Heap` regions — the pooled BIF heaps and their retired regions, `Runtime::eval_heap` + `eval_heap_retired`, machine/scratch/message-fragment heaps — appear in no key, and neither do the engine arena's `ReservedHeap` regions (each allocates its **whole** reservation up front and is never freed).
- Also outside every key: each process's **Erlang stack** (`Registers`'s `Stack` is a `Vec<Eterm>`, `beam_process/src/registers.rs:191-198`), the mailbox queues, MSO payload boxes (`beam_gc/src/mso.rs:71-79`, global allocator), the off-heap roster slot `Vec`s, every plain Rust allocation (loader IR, code-server maps, the compiler's own structures), and any carrier retention (`AllocStats::carrier_bytes` is not a `live_bytes` figure).
- So `process_info(P, memory)` matches no `erlang:memory` key: it adds `stack_size*8` and the per-process off-heap overhead bytes that `memory(processes)` omits, while `memory(binary)` counts a refc payload once globally rather than per owner — the same three-way split `beam_gc/src/footprint.rs:24-61` states for the crate's own accounting (whose module docs also list what it cannot see).
- Consequence: a `process_info(P, total_heap_size)` peak and a VM-wide `erlang:memory(total)` peak are not comparable in general — the first omits the mature capacity before the first collection and all off-heap bytes, the second omits every region above — so an RSS peak well beyond both is expected rather than a contradiction.
### Documented divergences (correct-but-different-by-construction)
`/1` emits 15 (registered) / 14 (unregistered) items where the oracle emits 17/16 when no execution position is readable.
`memory`, `reductions`, `group_leader` (`Runtime::group_leader_of`'s real pid — the root pid by default, set by `erlang:group_leader/2`, inherited on spawn), `binary`'s `Ptr`/`Refc`, `error_handler` (the atom) and `suspending` (`[]`) are this runtime's own values.
`status` is `running` for the caller (single-threaded), else the process's `ProcessState`.
`message_queue_data` is `on_heap` (the queue is heap-resident), with an **explicit** `process_flag(message_queue_data, off_heap)` accepted-and-ignored.
`trace` is the **live** tracee-flag **bitmask** integer (`0` while untraced), never `trace_info(P, flags)`'s atom list.
The one outright divergence is **`last_calls`** — hard-coded `false` while `Process::save_calls()` holds a real flag (`mod.rs:803`).

### `registered_name` is the queried process's
`registered_name` is the truth of the **queried** process (resolved by pid, never the caller's), which is what makes the `-eval`/`-s`/`-run` action routes agree with the oracle.
Those routes evaluate the action in a **freshly spawned, unregistered** child of the runtime's root process (`../../runtime.rs::as_action_process`), so the action's own `process_info(self(), registered_name)` answers the bare `[]` exactly as the oracle's `-eval` child of `init` does, while `process_info(init, registered_name)` still answers `init`.
The dispatch-level pin is `tests.rs::registered_name_is_the_queried_processes_name_not_the_callers` (the root pid is registered as `init`, so a test can `unregister` it to reach the live-unregistered case).

## Constraints
- The crate-wide `#![deny(unsafe_code)]` covers this node: terms are built and read only through `beam_engine::terms`, `beam_codec::term` and `beam_term`.
- `snapshot` extracts the item figures before the process-table borrow ends, and the reply is then built on the engine heap.
- Return-based errors throughout: the floor arm answers `Option<Result<Eterm>>`, a `badarg` is a raised `BeamError`, never a crate-local error enum, never `unwrap()` on runtime data, never a silent swallow.
- No item builder collects or blocks: every source is published read-only state (`RunnerState`, the in-machine door, the process table).

## Test Strategy
- `tests.rs` pins the reply shapes, the failure modes, the `unsupported` set and the `registered_name` wrapping rules through the crate's own dispatch (`../test_support.rs::call` ⇒ `call_bif` ⇒ `try_native`), so the *served surface* is pinned rather than an internal function.
- The `links` and `monitored_by` items are pinned **hermetically at the builder level** (`the_links_item_unions_a_local_link_with_an_inbound_distribution_link`, `the_monitored_by_item_renders_a_local_and_a_wire_origin_watcher_apart`): both halves of each union are installed through the real entry points (`ProcessTable::{link,record_remote_link,unlink_term}` / `monitor` + `monitor_from_remote_ref` + `demonitor_from_remote_ref`), every identity field of the external pid is asserted separately, and the two-step path is asserted equal to the fast path after the remote half is removed. **Shape only** — no foreign pid is printed.
- `binary_e2e.rs` adds the **end-to-end** case — the `binary` item of a real, test-time-compiled `.beam` spawned through the scheduler and parked holding an off-heap refc binary, compared structurally against the live oracle running the same fixture; its `oracle_eval_pa` is the only probe in this node that also passes `-pa <scratch dir>` (its fixture is spawned by `spawn/3`), bounded 30 s / 10 ms poll, and its fixture half compiles with `erlc` first and skips loudly without it (`build_fixture`).
- `../../../tests/process_info_binary.rs` pins the **self** query (`process_info(self(), binary)` from the slice that just built the payload, in both the atom and the list form, plus the empty and the other-pid rows) against the live oracle as canonical flat integer lists.
- `process_info(Pid, garbage_collection)` is **NEVER** compared against a live oracle: `garbage_collection_matches_the_oracle_for_a_live_process` asserts the runtime's own root-process value against a **hard-coded** `expected` array (233 / 46422 / 65535 / 0) — the name's "oracle" is the constants' documented provenance, not a live run.
- `erlang:memory/0,1` and `erlang:statistics(garbage_collection)` do not appear in this node; the only live-oracle GC-family probe is `the_oracle_backs_the_remaining_gaps_values`, which compares `garbage_collection_info`'s **item KEY NAMES** only (`element(1,…)`), never values.
- The live-oracle probes here — `tests.rs`'s `oracle_pi_1_keys` (**unbounded**) and its `oracle_eval` (bounded by `ORACLE_TIMEOUT = 30s`) — resolve `erl` by a **bare `$PATH` lookup only** and print an inline `eprintln!("SKIPPING …")` (no banner) when absent.
- Oracle values are quoted in each test's doc comment, re-verified against the system `erl` (OTP 29.1.1 / erts-17.1); where the runtime answers a typed capability gap the oracle answers a value, the test pins the `unsupported` outcome with a `// documented divergence:` comment.

## Routing Table
- `mod.rs` → the `process_info/1,2` dispatch and every item builder (`PI_1_KEYS`, `snapshot`, the `position_value`/`catchlevel_value` consumers, the shared `binary_value` builder) plus the module docs carrying the per-item table.
- `binary.rs` → the `binary` item's roster (the deduped reservoir union and its two read routes), documented in the module header.
- `tests.rs` → the colocated unit suite (reply shapes, failure modes, `unsupported`, `registered_name`).
- `binary_e2e.rs` → the end-to-end `binary`-item oracle comparison over a test-time-compiled `.beam`.
- `../native.rs` → the `try_native` arm (the one dispatch table) and the shared helpers.
- `../../runner/` → `RunnerState::{reservoir_at,position_engine_at}`.
- `../../context/position.rs` → the in-machine BIF door; `../../context/mod.rs` → `call_bif`.

## See Also
- `../CONTEXT.md` → the floor's contract, the dispatch order and the constraints.
- `../process/CONTEXT.md` → the process-control families (the `max_heap_size` setter side, `garbage_collect`).
- `../../../../../../erts/emulator/beam/erl_bif_info.c` → `pi_1_keys[]`, `process_info_2`'s forms and the `always_wrap`/nil-skip rules (read-only ground truth).
- `../../../M1_GAPS.md` *`process_info/1,2`* → the divergence ledger.
