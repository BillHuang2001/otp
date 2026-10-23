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
`backtrace` is the **one** remaining gap (`UNSUPPORTED_ITEMS == ["backtrace"]`): it names the missing raw/binary stack-dump accessor — the engine builds only the cooked `current_stacktrace` list, never the C's `erts_stack_dump` binary.
`parent`, `label`, `priority_messages`, `magic_ref` and `garbage_collection_info` are **served**; the three carrying a decision or a caveat are the three sections below.

### The heap-size items are oracle figures, not this runtime's own
`heap_size` is the nursery's capacity, which is the process's own `min_heap_size` (`HeapPolicy::initial_heap_words`), so the default is the oracle's 233, `+hms 4000` answers 4185 and a `spawn_opt` `{min_heap_size,N}` is floored by the emulator's `+hms` and reported.
`total_heap_size` composes the C's figure — `heap_size` plus the old generation's capacity **only once a collection has created it** (`erl_bif_info.c:1023-1030`), so a fresh process reports `total_heap_size == heap_size`, exactly as the oracle does.
Both are pinned against the live oracle by `../../../tests/min_heap_size.rs`.
### Documented divergences (correct-but-different-by-construction)
`/1` emits 15 (registered) / 14 (unregistered) items where the oracle emits 17/16 when no execution position is readable.
`memory`, `reductions`, `group_leader` (`Runtime::group_leader_of`'s real pid — the root pid by default, set by `erlang:group_leader/2`, inherited on spawn), `binary`'s `Ptr`/`Refc`, `error_handler` (the atom) and `suspending` (`[]`) are this runtime's own values.
`status` is `running` for the caller (single-threaded), else the process's `ProcessState`.
`message_queue_data` is `on_heap` (the queue is heap-resident), with an **explicit** `process_flag(message_queue_data, off_heap)` accepted-and-ignored.
`trace` is the **live** tracee-flag **bitmask** integer (`0` while untraced), never `trace_info(P, flags)`'s atom list.

### `last_calls` — the `save_calls` ring
Three oracle-verified states: the bare atom `false` while no buffer is armed (`process_flag(save_calls, 0)` disarms and answers `false`), the empty list `[]` while armed and empty, else the ring rendered in **recorded order (oldest first)**. The C's `ERTS_PI_IX_LAST_CALLS` (`erl_bif_info.c:2183-2225`) walks the ring newest-first (`j = scb->cur - i - 1`) but `CONS`-prepends each term, so the answer it returns is chronological and the querying `{erlang,process_info,2}` is **last**. Entries are `{M,F,A}` triples plus the bare atoms `send`, `'receive'`, `timeout`.
Record sites: `{M,F,A}` from `RuntimeContext::resolve_external_call` (both execution tiers); `'receive'`/`timeout` from the mailbox seam (`Runtime::record_saved_receive` in `receive_remove`, `record_saved_pseudo` in `receive_reset`); `send` from `Runtime::record_saved_send`, called by the one send funnel `crate::context::send::send_message_with_off_heap` for every non-remote destination (and on its `badarg` arm, the C's final `else`) — it pushes the `{erlang,send,2}` call entry first unless the ring already ends with it, which absorbs the BIF route's own entry. The compiled `send` instruction mirrors `call_light_bif` for `erlang:send/2` (`jit/x86/instr_bif.cpp` `RET = imm(send_2)`), so the call entry is recorded at the instruction and only the pseudo entry comes from the funnel; the C's `save_calls(p, &exp_send)` happens once per local send whichever route reached `do_send`. The three remote arms (`RemotePid`/`RemoteAlias`/`RemoteName`) record nothing, going through `remote_send`, which never touches `save_calls`.
The ring lives on `Runtime` (`saved_calls(&pid)`, armed by `arm_saved_calls`) keyed by pid, so a parent reads an armed idle child's ring — `[{erlang,send,2}, send]` for the spawner's own `!` into it — and re-arming resets it.
This runtime records **every** `call_ext`, so its ring is a **superset** of the oracle's: the oracle drops calls by peephole (`lists:reverse/1`, `lists:sort/1`, `lists:append/2` with an unused result are frequently absent there, `lists:sum/1` present). Superset, never a wrong entry.

### `parent` — the recorded spawner, else the `init` stand-in
`parent` is the recorded spawner (`Runtime::set_parent`, applied by every spawn path: `spawn_in_pass`, the direct `spawn_export_child_with_policy` arm and the fun-spawn arm), so a grandchild names the **middle** process, as the oracle does.
A process this runtime did not see spawned answers `Runtime::init_pid`: a pid **drawn first** from the process table (`processes.alloc_pid()?`, before the root's `processes.spawn(system_spawn_policy(..))`), so it renders as the oracle's `<0.0.0>` and the root moves to `<0.1.0>`. Nothing runs behind it — absent from `processes/0`, no heap, never receives.
Why a stand-in rather than the root pid: on the oracle the `-eval` root's parent is `init` `<0.0.0>`, which is **not** itself, while here the `-eval` action already runs in a child of the root (`Runtime::as_action_process`). Answering the root's own pid would make `element(2, process_info(self(), parent)) =:= self()` true, which the oracle answers `false`; a distinct table identity satisfies the relation without fabricating a term. The stand-in is also what the `init` name is registered on, so `whereis(init)` **is** the root's `parent` — one pid for both roles, as on the oracle. It is a real, never-enqueued process (so `is_process_alive/1` and `process_info/2` answer for it), and its own `parent` item is the bare `undefined` because the C's `init` has no `p->parent`.

### `garbage_collection_info` — the eleven figures
The oracle's ordered eleven-pair proplist (integer values from `GcInfo`): `old_heap_block_size, heap_block_size, mbuf_size, recent_size, stack_size, old_heap_size, heap_size, bin_vheap_size, bin_vheap_block_size, bin_old_vheap_size, bin_old_vheap_block_size`. Three are honest `0`s — see Known Issues.

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

## Known Issues
- `garbage_collection_info` answers three honest `0`s: `mbuf_size` (this runtime's message queue is a
  heap-resident Rust FIFO, not an on-heap `mbuf` block), `recent_size` (the collector keeps no
  high-water mark), and `bin_old_vheap_size` (one bin vheap per process, no old generation for it).
  `bin_vheap_block_size`/`bin_old_vheap_block_size` are the heap policy's `min_bin_vheap_words`, the
  block size the off-heap reclaimer is installed with (`p->bin_vheap_sz`/`p->bin_old_vheap_sz` in the C).
- `last_calls`' ring is a **superset** of the oracle's (every `call_ext` is recorded; the oracle's
  peephole drops some) — see the section above.
- `backtrace` remains the only `process_info/2` item in `UNSUPPORTED_ITEMS`.
