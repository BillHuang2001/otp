# `beam_rt/src/native/process/` — the process-control floor (`erlang:*` processes, monitors, spawn)

## Intent
The process-control families of the runtime-native floor: everything that reads or writes this runtime's own `beam_process` tables.
The bodies are one module per family — this node's two children `exit.rs` (the exit-signal floor) and `monitor.rs` (the monitor/demonitor/alias family), and the sibling parent file `../process.rs` (the spawn family, the process/port/registry queries, `whereis`/`register`/`unregister`, `processes/0`, `ports/0`, `pre_loaded/0`, `module_loaded/1`, `group_leader/0`, `make_ref/0` and the `monitor_nodes` forms of `process_flag/2`).
The node-facing and leader arms are siblings too: `../node.rs` (identity/cookie), `../net/` (`nodes/0,1`, the connector, `monitor_node/2`) and `../group_leader.rs`.
The dispatch is `../native.rs::try_native`'s (the one dispatch table — this node adds no second list).
Everything routes through `beam_process`'s tables; the per-family oracle contracts live in each module's rustdoc plus `../../../M1_GAPS.md` *Process-control families*.

## API Surface
### Monitors, aliases and references — `monitor.rs`
`monitor/2,3`, `demonitor/1,2`, `alias/0,1` and `unalias/1` route through `beam_process`'s tables, with the per-family oracle contracts in `monitor.rs`'s rustdoc.
A bare `make_ref/0`/timer reference removes nothing, a structural non-reference is the `badarg`, and `[flush]`/`[info]` answer the real "was a monitor removed" boolean.
`erlang:monitor(process, {Name, Node})` naming **another** node is a typed `unsupported` (`monitor.rs::named_node_target` resolves names on this node only), so a remote **name** monitor has no `DOWN`; a remote **pid** monitor works in both directions.
A reference on this node is not a local alias (the runtime keeps no local alias table), so an alias-directed send to a non-live reference takes the send path's own typed error.

### `exit/2,3` — `exit.rs`
`exit/3` takes only the bare `priority` atom.
A self-directed exit is raised directly, or delivered as `{'EXIT', self(), R}` to a trapping caller.

### The spawn family — `../process.rs`
`spawn_opt` validates **every** option before the child exists, and runs from a normal context **or inside a scheduler slice** (`RuntimeContext::spawn_in_pass` + `Runtime::absorb_pending_spawns`, `../../../tests/spawn_from_slice.rs`).
**`spawn_opt`'s `{max_heap_size, Size | Map}` carries the whole setting, not just its bound.** `SpawnOptions::max_heap` holds the `MaxHeapSetting` (bound plus `kill`/`error_logger`/`include_shared_binaries`), the bound also lands in the child's `HeapPolicy` (`SpawnOptions::policy`, an independent clamp), the C's cross-field rule is `violates_floor(parsed.policy().min_heap_words)` ⇒ `badarg` (`erl_process.c:12429-12431`), and `apply_spawn_options` installs the setting on the child's PCB (`Process::set_max_heap_size` + `Runtime::note_max_heap_set`), so `process_info(P, max_heap_size)` reports it rather than the birth default.

### `erlang:spawn_monitor/1,2,3,4` — an `erlang.erl` wrapper, not a `bif.tab` BIF
The four arities (`erlang.erl:10667`/`:10686`/`:10706`/`:10874`) are `erlang:spawn_opt(…, [monitor])` behind `erlang.erl`'s own guards (`is_function(F,0)` for `/1` and `/2`, plus `is_atom(Node)`, `is_atom(M)`, `is_atom(F)`, `is_list(A)`), so no registry entry — and nothing in the known-unimplemented list — claims them and the floor owns the whole family.
A guard violation is the oracle's `badarg`, and the `Node` forms on a node **other** than this runtime's own are the typed capability gap (the oracle routes that clause through a distributed `erlang:spawn_request/5`, A.5), never a locally fabricated child.
The monitor is installed by the private `attach_monitor` (shared with `spawn_opt`'s `monitor` option, so the `{Pid, Ref}` shape and the reference → monitor bookkeeping cannot drift), on record **before the child's first schedulable moment** (`../../../tests/spawn_monitor.rs`).

### The registry / `whereis` surface — `../process.rs`
A name is resolved in the **process** table first and the **port** registry second, so a registered port answers `whereis/1` and `registered/0`.
`whereis/1` answers `undefined` for an unknown name; `register/2`/`unregister/1` keep `beam_process`'s name table as the one store.
`group_leader/0` reads the calling process's leader and `erlang:group_leader/2` (with its `erts_internal:group_leader/2,3` forms) writes it through `Runtime::group_leaders` (`../group_leader.rs`).

### `erlang:process_flag(monitor_nodes | {monitor_nodes, Opts}, Bool)` (A.5/A.9)
Claimed **only** for the bare `monitor_nodes` atom and the `{monitor_nodes, Opts}` tuple the C's `process_flag/2` intercepts (`bif.c:2202-2227`) — the registry's `ProcessHook::process_flag` takes an `Atom` and cannot carry a tuple flag.
`erts_monitor_nodes` (`dist.c:6754-6868`)'s `On`-boolean rule, `[]`-or-map `Opts` grammar (`list`/`connection_id`/`node_type`/`nodedown_reason`; anything unrecognised `badarg`) and per-pid reference counts (`0`/`1`/`2`/`0`) are `../process.rs`'s rustdoc/unit tests, over the `Runtime::monitor_nodes` store.
Every ordinary atom flag (`trap_exit`, `async_dist`, `priority`, `save_calls`, …) still falls through to the registry and its `ProcessHook` bridge unchanged.

### `erlang:monitor_node/2` (A.5) — `../net/monitor.rs`
Served by `net/monitor.rs::monitor_node_2` (arm in `../native.rs` beside `alias/1`/`unalias/1`): the atom `true` for **every atom** node — this runtime's own node, `nonode@nohost`, a node with no link — and `badarg` **only** for a non-atom node or a flag other than the atoms `true`/`false` (measured on OTP 29.1.1 / erts-17.1; the "`Node == node()` is `badarg`" claim is measurably false and is not implemented).
The subscription is stored on `Runtime::node_monitors` — the C's `ERTS_MON_TYPE_NODE` chain, a **separate** store from `Runtime::monitor_nodes` above (`ERTS_MON_TYPE_NODES`) — as the C's idempotent set: a repeat `true` neither duplicates nor refcounts, `false` removes, and the per-entry `saw_connected` is seeded from `Runtime::dist_is_connected`, so subscribing to an already-connected node replays **no** `nodeup`.
The notifications are the plain 2-tuples `{nodeup, Node}` / `{nodedown, Node}` (never the `Info` 3-tuple of the `monitor_nodes` family), emitted by the scheduler pass's `Runtime::service_node_monitors` (owned by `../../runtime/scheduler.rs`, diffed by the pure `node_monitor_transitions`).
`erlang:monitor_node/3` stays a typed capability gap (`beam_bifs` registers it as a runtime BIF with no host hook → `unsupported: erlang:monitor_node/3`, CLI exit 3), so it is deliberately **not** listed in `../../error.rs`'s `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` (whose drift guard forbids curating a registry-owned key).

### `erlang:nodes/2`'s `connection_id` is a real per-link integer — `../net/nodes.rs::nodes_2`
A **peer** entry emits `Runtime::dist_connection_id(<the peer's name>)` — the local link identity `DistLinks::connection_id_for` mints on first request and keeps for that link's lifetime (a different integer per live link, a fresh one for a re-established link) — and only a `this` entry, or a peer whose link retired between the node set and the lookup, emits the C's `~0`, the atom `undefined`.
Nothing raises: the value is a positive integer or `undefined`, which is what makes `global:init/0`'s `nodes(visible, #{connection_id => true})` (via `net_kernel:monitor_nodes/2`) succeed on a live peer.

### `erlang:hibernate/0` — `../process.rs`
An OTP-30 instruction modelled as "continue": `process.rs::hibernate_0` answers `true` because the C's `i_hibernate` rewrite (`emu/ops.tab:896-898`, `:975`) does not happen here and `erts_hibernate` (`beam_common.c:1568-1602`) never blocks — so the caller's own following `receive` blocks, the only thing the OTP callers (`gen_server.erl`, `gen_statem.erl`) rely on.

### `erlang:garbage_collect/0,1,2` (A.7) — the on-demand collection
Bodies in `../erts_internal.rs`, arms in `../native.rs::try_native`; `erlang` is a floor namespace, so its `.beam` wrappers are un-enterable and the floor owns all three arities (`/0` `erlang.erl:3051-3053`, `/1` `:3055-3066`, `/2` the `get_gc_opts/2` grammar `:3073-3157`), with the per-argument oracle probe table in `../erts_internal.rs`'s rustdoc.
**No BIF may collect** — the root set a collection relocates is the **executing machine's** `x[0..live)` plus its Erlang stack, which a BIF running *inside* the engine does not own — so a servable call records a **request** via `Runtime::request_gc` and answers the oracle's `true`.
It is served only where that root set is owned: the engine's heap-admission point `../../context/gc.rs::ensure_heap`, which bypasses only the occupancy test and never the safety valves, and the runner's park arms `../../runner/run.rs` → `Runtime::serve_gc_request`.
A request is recorded **only** for a **scheduled slice** (a `-eval` or single-pass `run_*` context allocates on the pointer-stable runtime arena no collection moves, so it answers `true` and records nothing), so `Runtime::gc_stats().collections` moves only when a collection really ran, and a request the valves refuse (a pinned or halted pid, a **relocation-exempt** process, the global escape-store valve) is dropped honestly.
A request for a process that terminates first is pruned at the pass boundary (`../../runtime/scheduler.rs::prune_gc_requests`).
The argument contracts (`erl_process.c:11309-11324`): a **live local pid** ⇒ `true` plus a request for *that* pid, a well-formed internal pid the table does not hold ⇒ `false`, anything else — including an **external**/wire pid, which this runtime can never honour ⇒ `badarg`.
`/2`'s options are the 2-tuples `{async,_}`/`{type,_}` (a later overriding an earlier; default `major`; anything else `badarg`), the `{type,_}` value validated **before** any liveness decision (`garbage_collect(DeadPid, [{type, foo}])` is `badarg`, never `false`), and `{async, ReqId}` answering `async` for a live and a dead target alike.
**The async form's follow-up `{garbage_collect, ReqId, GCResult}` message is the one part this runtime cannot reproduce** — the oracle's request is a cross-process system task (`erts_internal:request_system_task/3`) with no system-task protocol here — so the return value is the oracle's exactly and the notification is never sent, never faked (`../../../tests/garbage_collect.rs`).

### Not served — process / scheduler control
`process_display/2` needs a process **stack dump** (no engine accessor).
`suspend_process/1,2` needs suspend/resume signalling.
`await_result/1`, `flush_monitor_messages/3` and `'@flush_monitor_messages_refopt'/0` are **pure Erlang** receive loops, not BIFs.
A remote `{Name, Node}` monitor and `erlang:monitor_node/3` are the typed gaps above; a monitor of an **internal port** or a **foreign reference** by `exit/2,3` is also a gap.
`spawn_opt`'s `{scheduler,N}`/`{async_dist,B}` options are gaps.
Each is a typed capability gap, never a wrong result; the `erlang`-namespace fallback classifies the curated ones (`process_display/2`, `suspend_process/1,2`, `spawn_request/1,2,3,4,5`) as exit-3 `unsupported` (`../../error.rs`).

## Constraints
- The crate-wide `#![deny(unsafe_code)]` covers this node; terms come from `beam_engine::terms`/`beam_codec::term`/`beam_term` only.
- Return-based errors: `Option<Result<Eterm>>` from the floor arm; a "value atom" the C *returns* is `Ok(atom)`; nothing panics on runtime data and nothing swallows.
- The floor holds **no** process state of its own — `beam_process`'s tables are the one store, and the file is the only place a per-family answer is rendered.
- A family that needs a facility this runtime lacks answers the typed `unsupported`, never a fabricated child, monitor or reference.

## Test Strategy
- The colocated suites live with `../process.rs`; this node's own children have none, so the family contracts are pinned end to end by the crate's integration suites: `../../../tests/spawn_from_slice.rs` (an in-slice `spawn_opt`), `../../../tests/spawn_monitor.rs` (the `{Pid,Ref}` record before the first schedulable moment), `../../../tests/garbage_collect.rs` (the `/0,1,2` argument matrix and the never-sent async notification) and `../../../tests/ref_id_space.rs` (monitors/aliases sharing the one reference-id space).
- `../../../tests/position.rs` pins the execution-position accessors the `process_info` family reads; every message/exit/`DOWN` shape is additionally exercised on both the single-pass and scheduled routes by the crate's `tests/`.
- Oracle values quoted in a test's doc comment are re-verified against the system `erl` (OTP 29.1.1 / erts-17.1); a typed-gap row pins the `unsupported` outcome with a `// documented divergence:` comment.

## Routing Table
- `monitor.rs` → `erlang:monitor/2,3` + `demonitor/1,2` (and the alias/reference forms), over `ProcessTable::{monitor,monitor_named,demonitor}`; the `named_node_target` typed gap.
- `exit.rs` → `erlang:exit/2,3` (the exit-signal floor), incl. the trapping-caller delivery shape.
- `../process.rs` → the sibling parent file: the spawn family (`spawn_opt`, `spawn_monitor`), the process/port/registry queries, `process_flag(monitor_nodes…)`, `make_ref/0`, `hibernate/0`.
- `../node.rs` → `is_alive/0`, `get_cookie/0`, `set_cookie/2`.
- `../net/` → `nodes/0,1` + the connector (`net_adm`/`net_kernel`/`erl_distribution`) + `monitor_node/2`.
- `../group_leader.rs` → `erlang:group_leader/2` and `erts_internal:group_leader/2,3` over `Runtime::group_leaders`.
- `../erts_internal.rs` → the `garbage_collect/1` body and the `erts_internal`-namespace process primitives ([`../erts_internal/CONTEXT.md`](../erts_internal/CONTEXT.md)).
- `../../runtime.rs` / `../../runtime/scheduler.rs` → `monitor_nodes`, `node_monitors`, `service_node_monitors`, `request_gc`/`serve_gc_request`/`prune_gc_requests`.
- `../../../../../../erts/emulator/beam/` → the C ground truth: `erl_process.c` (`garbage_collect`'s argument rules), `bif.c` (`process_flag/2`'s interception), `dist.c` (`erts_monitor_nodes`); `../../../../../../erts/emulator/beam/emu/ops.tab` (`i_hibernate`).

## See Also
- `../CONTEXT.md` → the floor's contract, the dispatch order and the constraints.
- `../memory/CONTEXT.md` → `process_info/1,2`, the read side of `max_heap_size`/`garbage_collection`.
- `../../../M1_GAPS.md` *Process-control families* / *Distribution and the node (A.5)* → the gap ledger.
