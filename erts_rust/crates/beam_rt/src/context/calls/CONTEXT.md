# `beam_rt::context::calls` — the registered-call path (dispatch · bridges · interners)

## Intent

The registered-call machinery of the concrete `ExecutionContext`: what happens between the engine handing
`call_bif` a `{module, function, arity}` and a `beam_bifs` implementation running.
It owns the dispatch memo and its hook roster, `PendingInterner` and caller-module tracking, the NIF dispatch
order, `call_registered`'s gated arms with the lent/held `TableRegistry` rule, the nine bridges and the `[safe]`
export gate.
The heap doors and the off-heap prunes are [`../gc/CONTEXT.md`](../gc/CONTEXT.md)'s; the BIF-result copy-out and
the term stores are [`../pool/CONTEXT.md`](../pool/CONTEXT.md)'s; the seam-level contract is
[`../CONTEXT.md`](../CONTEXT.md)'s and its mechanics companion [`../IMPLEMENTATION.md`](../IMPLEMENTATION.md).

## API Surface

### The dispatch memo · `DispatchHooks`
`dispatch(module, function, arity) -> DispatchAnswer` is the **only** producer of a registered call's dispatch answer; it is memoized in a per-context **direct-mapped 128-slot** `DispatchMemo` (the slot from `dispatch_slot` over the key's **name bytes** and the arity), whose slots are `Option<Box<DispatchEntry>>` — boxed so the memo costs 1 KiB of the context rather than 10 KiB, which a nested-run context (~100 Rust frames deep on the A.9 compile path) cannot afford.
`DispatchAnswer` is `{ bif: Option<BifFn>, hooks: DispatchHooks }`: `bif` is `registry.lookup(module, function, arity)` — the implementation the caller then **invokes directly** (no second lookup); `contains` **is** `lookup(…).is_some()` (`beam_bifs/src/registry.rs:189`, pinned equal by that crate's `contains_always_agrees_with_lookup`), so `call_registered` is reached only on `Some`.
`hooks` is one boolean per `uses_*` predicate — the eight mutually-exclusive arm selectors plus the build gates (`fun_identity`, `memory_hook`) — computed by `DispatchHooks::of`, the **only** place the `binary_search`es over `beam_bifs`' `&'static` `needs_*` rosters run; `call_registered` selects its arm and optional handles from `hooks.*` instead of re-running the predicates.
A **miss** computes and stores; a **slot collision** overwrites, and the evicted key recomputes on its next call — an eviction costs work, never a result.
`dispatch_slot` hashes the key's **bytes** (FNV-1a over `module`, a `0xFF` separator, `function`, then the arity) and avalanches with a fixed multiply, so same-length keys span distinct slots (`ets:insert/2` and `ets:lookup/2` do not collide); the constant is **chosen**, not derived.
A **warm** slot is overwritten **in place** (`DispatchEntry::retag`: `clear` + `push_str` on both `String`s), so a steady-state miss allocates nothing; only a still-`None` slot allocates (once per key ever seen on that context).
The slot count is **measured**, not chosen for taste: the A.9 in-process `compile:file/2` route reaches **217** distinct keys for `lib/stdlib/src/ordsets.erl` and **241** for `string.erl`, and at 8 slots that produced **29.9 % misses of which 71 % were collisions** (`string.erl`: 3 227 465 hits / 1 442 796 misses = 343 044 cold + 1 099 752 collision). At 128 slots the same run reports 4 159 136 hits / 511 125 misses = 372 181 cold + **138 944** collision (88.96 % hits) — a collision still only costs work, never a result, so the widening is behaviour-preserving by construction and is pinned by `dispatch.rs`'s `memo_reuse_tests` (a brute-forced colliding pair answers each key its own answer in alternation; the `String` buffers' `as_ptr()` identity proves the reuse) plus `tests.rs`'s churn suite (whose probe set is now wider than the memo and includes one `erlang:+/arity` sibling per arity, so a hit test weaker than the whole key is caught).
**Why a cached answer cannot go stale.** Both inputs are immutable for the life of the runtime: the `needs_*` rosters are `&'static` tables with pure membership predicates, and `registry` is an `Rc<BifRegistry>` built once at boot (nothing calls `Rc::make_mut`/`Rc::get_mut` on it); the one runtime-mutable input — the NIF registry `load_nif/2` grows — is deliberately **not** memoized, since `call_bif` runs `try_call_nif` first on **every** call, so a library loaded mid-slice wins immediately (`../../tests/nif.rs`).
The memo lives on the `RuntimeContext`, so it is exactly as long-lived as one run; a nested run builds a fresh context with an empty memo.
**Two prologue costs are gone.** (a) The **second registry lookup** — `call_bif_routed` hands the already-resolved `BifFn` to `call_registered`, and `invoke_registered` reproduces `BifRegistry::call`'s arity check so the error path is unchanged. (b) The **NIF probe** — `try_call_nif` short-circuits on `NifRegistry::is_empty()`.

### `PendingInterner` · caller-module tracking · NIF dispatch
`PendingInterner` keeps its `{base, names}` state **inline** (`InternerState::Inline(RefCell<…>)`) and allocates only when `share` promotes it into a shared `Rc<RefCell<…>>`: `new(base)` roots it at the shared table's length, `commit` interns the recorded names in order at their predicted indices, and `beam_bifs::AtomInterner` gives append-only prediction + in-call dedup (`binary_to_term`/`list_to_atom`/`binary_to_atom` are the BIF writers).
An ordinary registered/NIF call has a **single** writer, so it never shares and allocates nothing; `load_nif/2`'s two writers (the BIF and the C `load` callback via `enif_make_atom`) call `share()` and get two handles over one cell, so both agree on `base + <names recorded>` and the caller's single `commit` lands every name at its predicted index; `share` is the only way to obtain a second handle (`PendingInterner` implements no public `Clone`).
`run_export` **sets** `current_module` (restoring it afterwards) and `FunBridge::call_fun` **inherits** it; it reaches the BIF through `BifContext::with_current_module` (`None` ⇒ `unsupported_bif`, never a guess). `call_registered` clones the caller's module only in the NIF-loading, `send` and fun-caller arms (which need an owned `String`).
`try_call_nif` is the **first statement of `call_bif`**, so a NIF replaces the Erlang stub of the same `{module,function,arity}` exactly as the BEAM's dispatch does; its term lives in the pointer-stable pooled BIF heap and the per-call `PendingInterner` is committed **after the registry borrow is released**. `NifCallError::Raised(reason)` → `error::nif_raised`; `UnknownFunction` → bare `error::undef`; a miss is `Ok(None)`.
The per-call host gets a `RuntimeNifServices` provider built from **disjoint** field borrows of the runtime (`refs`/`references`/`processes`/`ports`; `processes` is the caller's mailbox table on a scheduler slice), so the host's `make_ref`/`whereis_pid` hooks **and** its `is_port_alive`/`port_command` port hooks (a `try_borrow` ⇒ the typed `None` on a live borrow) are live; the registry `Rc` is cloned and the heap taken **before** those borrows, so the call itself never borrows `self.runtime`.
**Interception is at external dispatch only**: a *local* `call` inside the NIF's own module never reaches `call_bif`.

### `call_registered` — the gated dispatch arms, the lent/held `TableRegistry`
`call_registered` builds **one** `BifContext` and installs on **every** arm the cheap handles — the atom interner (`PendingInterner`), the port provider (`RuntimePorts`) and the `Copy` `NodeIdentity` hook.
The `TableRegistry` is installed from **two** sources, in precedence order:
* **lent** — the one live `&mut TableRegistry` an outer `BifContext::call_fun` handed the [`FunBridge`](#the-nine-bridges) for one nested call; the outer `ets:foldl/3`/`ets:foldr/3` `take`s it out of its own `BifContext` before applying the fun, so the lent `&mut` is **exclusive by construction** and installing it cannot alias.
  It is installed for **every** nested call (not gated by the cell rule below), which carries it through a reentrant intermediate that installs no cell of its own (`ets:foldl` fun → `lists:map` fun → `ets:lookup`).
* **held** — otherwise the shared cell, held exactly when this call needs it: `install_tables = !reentrancy_capable(args) || tables_capable(module, function, arity)`.
  A reentrant call that itself reads the seam (the two `ets` fun-folds) holds it; one that does **not** read the seam installs nothing, leaving the `RefCell` free for the fun it applies to re-enter and reach `ets:*` (that is why `lists:map(fun(X) -> ets:lookup(T, X) end, L)` works).
`try_borrow_mut` (never `borrow_mut`) stays the rule: an unexpected outer borrow degrades to "no ETS", never a panic; `reentrancy_capable` also gates the result rewind (`safe_to_rewind`, `false` for the six heap-retaining arms and for any reentrant call).
The copy-out has **two** destinations, chosen by who is running (`calls.rs::rehome_container_result`): a scheduled process gets the result on its own **scanned** young heap (`move_result_into_process_heap`), while the **table-less** route (the `-eval` evaluator, `-s`/`-run` and every single-pass `run_*` entry) copies it onto the runtime's long-lived `Runtime::eval_heap` + `Runtime::eval_off_heap` store (`pool.rs::move_result_into_runtime_store`).
The process arm is **hinted**: the largest graph this context has been refused for is remembered
(`RuntimeContext::copy_out_hint`, learned from the copier's own shortfall) and a destination with
fewer free words than the hint (plus a margin) is grown — through the NO-RELOCATION
`Process::grow_nursery` — **before** the attempt, so a copy-bound loop does not pay a refusal's
five-traversal sizing walk per op; `hint == 0` (a context's first copy-out) is byte-for-byte
today's behaviour and the grow-and-retry fallback is unchanged (`calls.rs::hint_grow_gate`).
See [IMPLEMENTATION.md](../IMPLEMENTATION.md#the-learned-copy-out-hint--copy_out_hint--hint_grow_gate-the-copy-bound-fix).
The **move** is unconditional on a process route (`move_container = mailbox_table.is_some() || safe_to_rewind`): a container result left on the pooled BIF heap sits in a region no collector scans and a later call's rewind recycles, so Erlang code that stores it in a heap object would hold a dangling pointer (`compile:debug_info_chunk/1` storing `lists:filter/2`'s result inside a tuple is that shape).
On the **table-less** route the move stays gated on `safe_to_rewind`: there is no collector to make the result collectable and a non-rewindable call never rewinds, so an unconditional move would only duplicate it into the runtime-lifetime store.
A **reference-holding** graph (`calls.rs::graph_holds_reference`) is neither moved nor rewound — a reference's identity is its address (`Runtime::references` is a `HashMap<Eterm, _>` and `Timers::is_timer_ref` keys on the term word), so a copy would stop being the timer/monitor it names.
The walk behind that test is **cons-spine driven**: its inner loop advances the CDR in place while it is a readable cons and pushes only the children `walk_may_lead_further` admits, so a long list costs one heap step per cell instead of a push + pop + re-read (the `apply_fun`-heavy compile route calls it ~64 k times per `ordsets.erl` compile, and the restructure is what took its self time from 15.5 % to 5.9 % there).
Every readability guard (`is_literal`/`is_term_word`/`dereferenceable_pointer`, the null/mis-aligned refusal), the one-charge-per-dereferenced-object `WALK_VISIT_BUDGET` (`budget == 0 ⇒ true`), the visit/spill counters and every answer are unchanged — pinned by `cons_spine_tests` (a 5 000-cell spine answers `false` with exactly 5 000 visits, plus the improper-tail, reference-in-tail, reference-in-CAR and NULL-cell-`LIST`-word cases).
That list shape is also why the walk needed no new `unsafe` block and no new guard: the budget's charge is per dereferenced object, never per cell, so the soundness valve stays the readability test.
The **rewind** is the gated half: both arms deep-copy through a **fresh, empty, per-attempt** `beam_message::OffHeapTable` (so `copy_struct` takes its single-traversal `try_copy_fast` path), **adopt** its drained handles/binaries/nodes on success only, and rewind the pool to that call's `mark` only when `safe_to_rewind` holds **and** the copy succeeded (or the result is an immediate) — a refusal, an `Err` result (whose live reason term may point into this heap) and a non-rewindable call leave the result on the pool at the call's own offset and never rewind.
The store's growth by region replacement, the rewind's soundness argument and the measured counter deltas (and why they do not reduce first-touch faults): [`IMPLEMENTATION.md`](../IMPLEMENTATION.md#the-table-less-arm-and-its-store).
`tables_capable` (`dispatch.rs`) names the seam's consumers, derived from `beam_bifs` (read-only; `src/ets*.rs`): **every registered `ets:*` verb** (each resolves its handle through `resolve_handle`/`take_table_gated`, all reading `ctx.tables()`) plus the two `erts_internal` raw-walk aliases `ets_raw_first/1`/`ets_raw_next/2`.
Intersected with the fun-argument predicate this yields exactly the two fun-folds — which take the table out of the registry before applying the fun, so they must hold (or lend) it, else `ctx.tables()` is `None` and they raise the typed `unsupported_bif`.

#### The nine bridges
Exactly **one** bridge is selected, first-match, from the `DispatchHooks` the caller's `dispatch` memo computed — one boolean per `uses_*` predicate, so no predicate runs per call; each bridge owns a different runtime borrow, so they cannot co-exist.
* **`DictBridge`** (`DictHook`; `uses_dict`) — the process dictionary over the **running process's** `beam_process::Dict` (the pass table, else the runtime's): `put/2`, `get/1`, `get/0`, `erase/1`, `erase/0`, `get_keys/0,1`; a missing pid is `error::no_mailbox`.
* **`ProcessBridge`** (`ProcessHook`; `uses_process_hook`) — all 11 `ProcessHook` methods over the real `ProcessTable` + `Rc<RefCell<PortRegistry>>`: `whereis/1`, `register/2`, `unregister/1`, `registered/0`, `is_process_alive/1`, `processes/0`, `ports/0`, `process_info_keys/1`/`process_info_item/4`, `process_flag/2`, `group_leader/0` (`process_info/1,2` is intercepted by the floor first; the per-process `group_leader/2` pair is served by the floor).
* **`NifBridge`** (`NifLoader`; `uses_nif_loader`) — `erlang:load_nif/2`, checked **before** the filesystem; a reload of an already-loaded module is `NifLoadError::Reload`; a load failure is mapped variant-for-variant and **returned**, never raised.
  Its long-lived host is built with `services = None` (it moves into the runtime's `NifRegistry`) and `crate::info::schedulers_count(runtime)` as its `scheduler_threads`, so a `load`/`unload` callback's `system_info` stays honest and every other provider-backed hook stays the typed default there.
  The load host carries no provider, but its `send` is live through the callback-scoped pending-send queue (`NifHostImpl::with_load_sends`/`take_load_sends`) that `NifBridge::load_nif` drains when the `load` callback returns — delivered through `nif_send.rs::deliver` over the pass's own process table (`NifBridge::mailbox`, because the runtime's `processes` is a pass-long empty placeholder) and drained on the veto path too.
  The window closes permanently with that drain, so an `unload`/resource-`dtor` callback on the same host answers the ABI's `0`.
  A NIF **call** instead runs on a per-call host carrying a `nif.rs::RuntimeNifServices` provider over `{ &mut Runtime, Option<&mut ProcessTable>, caller }`, so all eight provider-backed hooks are live there: `make_ref`, `whereis_pid`, the two liveness answers, `is_port_alive`/`port_command`, `send` (delivered through `nif_send.rs::deliver`, i.e. this module's own `send_message`) and `local_fun_identity` (`FunIdentityBridge`, for `enif_term_to_binary`'s `NEW_FUN_EXT`).
* **`RecordBridge`** (`RecordSource`; `uses_record_source`) — `records:get_definition/2` from the loaded module's `Recs` chunk (`Module::record_by_name`), field atoms name-resolved; unloaded module / undeclared record ⇒ `Ok(None)` ⇒ the oracle's `badarg`; an unresolvable field atom is a typed `TermNotFound`.
* **`SendBridge`** (`SendHook`; `uses_send_hook`) — `erlang:send/2`/`'!'/2` delegating to `send_message` (the return value **is** the message term; a send to a pid with no process succeeds silently; a non-pid-like destination is `badarg`; a registered name is delivered; a **reference alias** goes to the alias **owner**; a **port** goes through `SendBridge::send_with_options` → `deliver_to_port` ⇒ `PortSend::{Write,Dropped}`, where a written/queued command and every silent drop answer the message (`send/2`) or `ok` (`send/3`) and `[nosuspend]` on a busy port answers `nosuspend`).
* **`ModuleInfoBridge`** (`ModuleInfo`; `uses_module_info`) — `erlang:get_module_info/1,2` from the loaded module's IR.
* **`PersistentTermBridge`** (`PersistentTerm`; `uses_persistent_term`) — the seven `persistent_term:*` BIFs over the runtime's shared store (a cheap `Rc<RefCell<..>>` clone with **no** runtime borrow), holding raw `Eterm` words (safe while the heaps stay pointer-stable; a value that must survive a heap move is stabilized through `Runtime::try_stabilize_escaped`).
* **`RuntimeHooksBridge`** (`RuntimeHooks`; `uses_runtime_hooks`) — serves **40** of the 143-key `beam_bifs::RUNTIME_HOOKS_BIFS` roster; the arms are the single `match (module, function, args.len())` closing `runtime_hooks.rs`'s `impl RuntimeHooks for RuntimeHooksBridge`, and the other **103** roster keys reach its `_` arm, answering `unsupported: <module>:<function>/<arity>` (the seam default with no provider is the bare `unsupported_bif`; a few of the 103 are intercepted earlier by the native floor, so they never get here). The served / fixed-oracle-absent / typed-gap split: [`IMPLEMENTATION.md`](../IMPLEMENTATION.md#closable-host-seam-keys-runtime_hooksrs).
* **`FunBridge`** (`FunCaller`; the fallback) — `apply_fun` over `&mut Runtime`, carrying a clone of the caller's `SharedSink`, the mailbox identity, the scheduler-pump permission and `current_module`.
  It builds the nested context in one private `nested<'b>(&'b mut self, lent_tables: Option<&'b mut TableRegistry>) -> RuntimeContext<'b>`, shared by both seam entry points so they cannot drift: `call_fun` passes `None`, while the overridden **`call_fun_with_tables`** passes the outer call's lent `&mut TableRegistry` verbatim.
`NodeIdentity` answers this runtime's **own** node — `Runtime::local_node()` (the live `node@host` atom on a `-name`/`-sname` node, else `nonode@nohost`), creation `0`, plus the real `local_node_hash` (see [the node hash](../IMPLEMENTATION.md#the-node-hash-a2--node_hashrs)) — emitting `NEW_PID_EXT` (`88`) for an internal pid and `V4_PORT_EXT` (`120`) for an internal port.
The **decode** direction installs the same identity (`Decoder::with_local_node`) and demotes a local-node `NEW_PID_EXT`/`V4_PORT_EXT` to the internal id, so `binary_to_term(term_to_binary(self())) =:= self()`.
`FunIdentityBridge` supplies the identity a **compiled module's** fun hands to the `LocalFunIdentity` seam, on the `uses_fun_identity` arm (`term_to_binary/1,2`, `external_size/1,2` and the introspection pair `fun_info/1,2`).
For a `make_fun3` (`FunKind::Local`) argument it derives the `NEW_FUN_EXT` (`112`) image from the code server's **cached** loaded-module snapshot (`CodeServer::fun_module_snapshot`, scanned **caller-first**) plus the node atom — `Ok(None)`, never an error, for a context with no calling module, an unreadable `.beam`, or a fun no loaded module defines; otherwise module atom + module MD5 + `old_index`/`old_uniq` from `FunT` + the `FunT` entry's `function` + the fixed `<0.0.0>` creator.
An `External` fun and a non-fun are the clean `Ok(None)`; a decoded `Imported` fun resolves only its `name` (see the `fun_identity.rs` routing entry).
`RuntimeMemory` (the `MemoryHook`) is built only on the `uses_memory_hook` arm (`erlang:memory/0,1`); ungated its `code_bytes` snapshot (a `CodeServer::loaded()` clone+sort plus a `stat::metadata` per module) would cost ~13 µs on **every** registered call.

### The `[safe]` export gate — `export_gate.rs` (the 16th `BifContext` seam)
`ExportGateBridge<'a> { code_server: &'a CodeServer, registry: &'a BifRegistry }` implements `beam_codec::ExportGate`; its `is_export_active(m,f,a)` is the shared `runtime_hooks.rs::is_active_export` helper (`erlang:function_exported/3` now delegates to it) **OR** `registry.contains(m,f,a)` — the registry disjunct exists because this runtime's code server holds only the 22 preloaded modules.
The arm is its **own** mutually-exclusive `DispatchHooks` entry — the 11th, `export_gate`, from `RuntimeContext::uses_export_gate` = `beam_bifs::needs_export_gate`, whose roster `beam_bifs::EXPORT_GATE_BIFS` is exactly `erlang:binary_to_term/2` — installed in `calls.rs` next to `uses_record_source` because it borrows `&CodeServer` + `&BifRegistry` immutably while the fun/send/dict arms hold `&mut Runtime`.
Oracle-verified: `[safe]` accepts `lists:map/2` and `erlang:system_info/1` and refuses with `badarg` `lists:map/3`, `lists:no_such_fun_xyz/1`, `erlang:no_such_bif_xyz/1` and an unloaded module's export; the flag-free form never consults the gate.
A deliberate widening to record: a registry key is accepted even when its module is not loaded here.
Evidence: [`../../../tests/safe_binary_to_term.rs`](../../../tests/safe_binary_to_term.rs) (4 tests) + 6 in-module unit tests.

## Known Issues

Every entry is a typed capability gap or a documented divergence, never a wrong result.

* **The "no collection can run while a BIF call is in flight" invariant is a conjunction of facts, not an enforced guard** — the BIF route (`call_bif` → `try_call_nif` → `try_native` → dispatch memo → `call_registered`) reaches no `context/gc.rs` door (`BifContext` exposes only the pooled pointer-stable `Heap`), and `gc_armed` is `take`n at each door's first statement (`execution.rs:472,486,510`), armed only by `BudgetContext` immediately before a forward.
  Both halves are structural: the latch is **unreadable** inside the dispatch (`call_registered`'s destructure discards it) and the machine is handed as `&self`, so no arm can consult it and the engine cannot mutate the register file mid-call.
  Two term holders ride that window with **no root set covering them**: the marshalled argument copy (a `[Eterm; 2]` stack array / `Machine::args_scratch`) and `InMachinePosition`'s five `Eterm`s published in `Runtime::in_machine` (`position.rs`), which `alloc_words` places on the process's **scanned young heap**; the roster `off_heap_binaries` beside them is plain `(usize, usize, usize)` data, so it holds no term and needs no root set.
  `Machine::pending_exception` is a third, deliberately **not** parked by `with_roots` and rooted explicitly only in the park window via `Registers::extra_roots` (`runner/run.rs:527-540`); the copy-out is **not** a collection site (`calls.rs:964-1006`: `grow_nursery` + one retry), and the one in-call collection that exists — a delivery's `Process::deliver` on the receiver — is refused for the running process by the sender/receiver relocation exemption (`send.rs:352-502`).

* **A scheduled slice's out-of-line nested run cannot pump the scheduler, so a callee that parks in a `receive` is the typed `blocked()` gap** — the call goes `call_bif` → `run_export_blocking` (a fresh `Machine` on the same pid under a `for_process*` context, so `may_pump_scheduler()` is `false`) and `pump_once` returns `Ok(None)` on its first line, folding the parked callee into the uncatchable `beam_engine::blocked()` whether or not its `receive` carries an `after T`.

* **`erlang:fun_info/1,2` reads the `LocalFunIdentity` seam, and its `name` item comes from two wiring sites here.** `bridges.rs::local_fun_identity` attaches the `FunT` entry's `function` to the derived `FunIdentity` for a compiled `make_fun3` fun (`with_function`; introspection-only, so the `term_to_binary` bytes are unchanged); `fun_identity.rs::imported_identity` resolves a decoded `NEW_FUN_EXT` fun's `name` from the loaded module named by its own `module` atom.
  Residual: an imported fun whose **defining module is not loaded** is the typed `unsupported_bif` gap where the oracle reports `{name,[]}` (C `mfa == NULL ⇒ NIL`) — never a fabricated name; pinned by `an_imported_funs_unloaded_defining_module_is_the_typed_gap_not_an_empty_name`.

* **The fun-image half of `callee_observes_its_call_site` is per-function** (see *API Surface*): it admits `compile:*` to the in-machine route on the **scheduled** path; a signal-driven collection there is declined by the runner's **park-window relocation exemption**, with the evidence owed to `beam_process`'s signal sites ([`../../../EPP_SCHEDULED_ROUTE_FINDINGS.md`](../../../EPP_SCHEDULED_ROUTE_FINDINGS.md)).

* **The `erlang:send/2`/`'!'/2` BIF path cannot see the sending machine's in-flight refc-binary handles**, so a term the *running* machine just built off-heap and forwards from a BIF callee **flattens** (a correct but unshared copy); only the compiled `Dest ! Msg` instruction carries them.
  `send_source` unions the three families a *delivery* can resolve (`extra`, the runtime's escaped-payload store, the sender's mailbox-retained families) but the BIF route passes `extra = &[]` (the in-flight `Machine` is out of `RunnerState.engines` for the whole slice, and neither `ExecutionContext::call_bif` nor `SendHook::send` carries a handle parameter); closing it needs a sibling change in `beam_engine`/`beam_bifs`.

* **An enqueue-time control signal refused by the receiver's full nursery is built into a heap fragment and the message arrives** — `beam_process`'s `copy_reason_onto`/`enqueue_down_signal`/`enqueue_exit_signal` pre-test `has_unpublished_roots()` and the host's `CollectionGate`, hand the raw `Alloc` refusal back with **no** collect, grow or retry, and turn the `{'DOWN',…}`/`{'EXIT',…}` into a fresh `HeapFragment`.
  The only refusal still surfaced is an **exit** signal to a **non-trapping** receiver (it carries the exit *policy*); pinned by [`../../../tests/eval_exempt_receiver_signal.rs`](../../../tests/eval_exempt_receiver_signal.rs).

## Test Strategy

The colocated units live in [`../tests.rs`](../tests.rs) and [`../test_support.rs`](../test_support.rs).
The copy-out and `ets`-alias suites are [`../pool/CONTEXT.md`](../pool/CONTEXT.md)'s; the heap-door, valve and
off-heap-prune suites are [`../gc/CONTEXT.md`](../gc/CONTEXT.md)'s.

* Colocated units in `tests.rs` + `test_support.rs`: the `uses_*` predicates (incl. their pairwise disjointness and the `uses_memory_hook`/`uses_fun_identity`/`uses_export_gate` gates), the `install_tables` rule (`tables_capable_names_exactly_the_table_registry_seam_consumers`), `RecordBridge` over a loaded module's `Recs`, `NodeIdentity`'s internal-pid `NEW_PID_EXT` encoding, `FunIdentityBridge`'s clean-miss contract, the nested-BIF seams, `PendingInterner` and the export gate's six cases.

  The **dispatch memo** has its own block: hit/miss against the *uncached* prologue over a probe set spanning a registered BIF, one hook-gated BIF per arm, stdlib/ETS registrations and names no registry owns; the memoized roster mirrored **field for field against the raw `beam_bifs::needs_*` predicates**; a **slot-collision** block; and a churn block over a probe set larger than the memo.

* **The lent-`TableRegistry` propagation** (`tests.rs`): a nested `ets:*` inside an `ets:foldl/3`/`ets:foldr/3` fun resolves the folded table, and the lent registry survives an intermediate that installs no cell of its own (`ets:foldl` fun → `lists:map` fun → `ets:lookup/2`), both oracle-cross-checked through `-eval`.
  Note `ets:foldl/3` hands its fun the stored **object**, so a lookup must be keyed on `element(1, X)` — keying on the object itself is `hd([])` ⇒ `badarg` on **both** runtimes.

* Per-module units in `process_bridge.rs`, `module_info.rs`, `persistent_term.rs`, `memory_hook.rs` and `../nif.rs`: the `ProcessHook` methods over a real `Runtime` (incl. the already-borrowed `ports` registry ⇒ typed gap), the `ModuleInfo` item decode, the persistent-term semantics, the `MemoryHook`'s nine keys in oracle order, and the `NifHostImpl` hooks.
* Integration suites (routed in [`../../tests/`](../../../tests/CONTEXT.md)): `proc_seams.rs`, `module_info.rs`, `persistent_term.rs`, `nif.rs`, `driver.rs`, the two trace suites, `pid_roundtrip.rs`, the process-control family and `position.rs` (the execution-position door's oracle comparison), each driven through `-eval` and cross-checked against the live oracle.

* `calls.rs`'s own unit suite is `calls/tests.rs`: its three copy-out retry-policy cases over `mod notes`, plus the
  `#[ignore]`d measurement harness `eval_route_pool_costs` (routed to [`../pool/CONTEXT.md`](../pool/CONTEXT.md)).
* Run the crate gate `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib --no-fail-fast`.

## Routing Table

Each entry is the module's one-line purpose; the mechanics companion is [`../IMPLEMENTATION.md`](../IMPLEMENTATION.md).

* `runtime_context.rs` → the `RuntimeContext` type: fields (incl. `lent_tables`), constructors/accessors (`new`/`with_output`/`for_process`(`_with_engines`)/`engines`/`pid`/`may_pump_scheduler`/`arm_gc`/`process_table`(`_mut`)/`owns_process_table`/`heap_available`/`eval_heap`/`enter_module`), the relocation-exemption windows and `resync_parked_roots`.

* `dispatch.rs` → the dispatch memo (`DISPATCH_MEMO_SLOTS`/`dispatch_slot`/`DispatchHooks`/`DispatchAnswer`/`DispatchMemo`/`DispatchEntry`), the eleven `uses_*` hook predicates and the two cell predicates `reentrancy_capable`/`tables_capable` — see [the dispatch memo](#the-dispatch-memo--dispatchhooks).

* `calls.rs` → `call_bif_routed` (the three dispatch routes), `call_registered` (the nine arms + the lent/held `TableRegistry` rule + the re-home/rewind), the re-home arms `move_result_into_process_heap`/`rehome_container_result`, the copy-out helpers `copy_result_fresh`/`copy_shortfall_words`, the learned-hint gate `hint_grow_gate` with its constants `COPY_OUT_HINT_MARGIN_MIN`/`COPY_OUT_HINT_MIN_WORDS`, plus the growth-policy constants `GROW_HEADROOM_FACTOR`/`GROW_MIN_WORDS` (+ the `#[cfg(test)] mod notes` counters), `try_call_nif`, `take_bif_frontier`, `open_gate_for_foreign_result`; suite in `calls/tests.rs`.

* `perf_counters.rs` → the `pub(crate)` **env-gated** per-call counters (`BEAM_RT_PERF_COUNTERS=1`; a `Counters` behind a `OnceLock`, one relaxed load per call site when unset) that this path's `dispatch`/`call_bif_routed`/`call_registered`/`graph_holds_reference` sites bump: dispatch hit/miss/cold/evict, the route split (`call_bif`/`nif_probe`/`nif_hit`/`native_hit`/`registered`/`external`), the ten arm counters, `tables_held`/`tables_lent`, the copy-out/rewind family, the walk's calls/visits and a `per_key` map whose 24 hottest `(module, function, arity)` keys print on `Drop`. No behaviour depends on it and nothing is emitted unless the variable is set; the measured reports it produced are quoted under [the dispatch memo](#the-dispatch-memo--dispatchhooks).
* `calls/tests.rs` → the `calls.rs` copy-out unit suite (the three retry-policy cases over `mod notes`) plus the `#[ignore]`d **measurement harness** `eval_route_pool_costs` (`BEAM_RT_EVAL_POOL_CHILD` child per row, minor faults from `/proc/self/stat`, the pool's first-touch high-water and the store's committed words; table in [`IMPLEMENTATION.md`](../IMPLEMENTATION.md#the-table-less-arm-and-its-store)).

  It also carries the one inherent helper `host_serves(module, function, arity)` (floor ∪ NIF-loaded module ∪ registered `beam_bifs` key) — the **single** source of truth for "the host serves this MFA", which `resolve_external_call` refuses on first and the runner's export-entry arm asks directly (`../runner/CONTEXT.md`), because that seam's `Ok(None)` is overloaded.

  `resolve_local_call` is **not** `host_serves`: it answers `crate::native::prim_tty::serves_local_call(module, function, arity)` else `beam_bifs::serves_local_call(module, function, arity)` (`execution.rs`).
  That is two fixed `(name, arity)` key sets — prim_tty's 20 `NIF_KEYS` stubs and `beam_bifs`' 19 `zlib` stream stubs — each beginning with a **module-name equality** check, so the seam reads no runtime state, allocates nothing and does no hash lookup.
  `host_serves`'s NIF arm is module-granular (`NifRegistry::is_loaded`, not `lookup`), so a mid-run `load_nif/2` **does** flip it for every key of that module — the one runtime-mutable input either seam has.

* `bridges.rs` → the nine bridges (incl. `FunBridge`'s `call_fun_with_tables` override + its private `nested`), `NodeIdentity` (with its real `local_node_hash`), `local_fun_identity`/`module_md5`; `SendBridge` is a thin delegator to the shared classifier.

* `node_hash.rs` → the node's `LOCAL_EXT` hash (`node_hash()`, the self-contained port of the C's Bob-Jenkins `block_hash`) — mechanics in [`IMPLEMENTATION.md`](../IMPLEMENTATION.md#the-node-hash-a2--node_hashrs).

* `position.rs` → the in-machine execution-position door: `InMachinePosition`, `in_machine_probe`/`dispatch_in_machine` + `machine_position`, the published-slot readers `in_machine_value`/`in_machine_off_heap_binaries` that `../native/memory` consults first, and `off_heap_binaries`' `(addr, size, refc)` roster (read only for the `binary` item).

* `alias.rs` → `alias_destination`, the shared **provenance-classified** reference-alias → (`owner`, `MonitorRef`) lookup behind all three send routes.

* `nif_send.rs` → `deliver(runtime, mailbox, from, to, msg) -> bool`, the arm behind `enif_send` (A.3): it classifies through the shared `send::classify_send_destination` and runs this module's own `send_message` on a nested `RuntimeContext` (the sender as mailbox owner, `may_pump_scheduler` always `false`), so the message crosses the BIF send's copy boundary and a parked receiver wakes through resume-on-send; a dead/unknown local pid, a rejected destination or a failed copy answers `false` (the C's `0`).

* `fun_identity.rs` → `FunIdentityBridge`: `local_fun_identity` classifies through `beam_codec::term::fun_kind_term` (`Local` → the caller-first snapshot scan, `Imported` → `imported_identity`, `External`/non-fun → the clean `Ok(None)`); both arms reuse `bridges.rs::descriptor_position`.

* `export_gate.rs` → `ExportGateBridge`, the `beam_codec::ExportGate` over the shared `runtime_hooks.rs::is_active_export` **OR** the registry — see [the `[safe]` export gate](#the-safe-export-gate--export_gaters-the-16th-bifcontext-seam).

* `gc.rs` → the heap-admission doors (`ensure_heap`/`ensure_heap_full` + `collect_or_grow`), the allocation seams for a *scheduled* process, the root hand-off pairs (`publish_roots`/`scatter_roots`, `publish_roots_full`/`scatter_roots_full`), the per-word classifier `root_is_unscanned` with its two-tier scanned test `word_is_scanned`/`outside_bounds`, and the forced-request handling both full-root doors share.


* Read-only siblings: [`../../mailbox.rs`](../../mailbox.rs) (the nine `Mailbox` seam methods), [`../runtime/`](../../runtime/CONTEXT.md) (the scheduler/runner that drives the session), [`../../native/CONTEXT.md`](../../native/CONTEXT.md) (the BIF floor `try_native` consults first), [`../gc/CONTEXT.md`](../gc/CONTEXT.md) (heap doors + off-heap prunes), [`../pool/CONTEXT.md`](../pool/CONTEXT.md) (the BIF-result copy-out), [`../../../../beam_bifs/CONTEXT.md`](../../../../beam_bifs/CONTEXT.md) (the registry and the fifteen host seams this path installs).

## See Also


* [`../CONTEXT.md`](../CONTEXT.md) → the seam-level contract and the pinned summaries.
* [`../IMPLEMENTATION.md`](../IMPLEMENTATION.md) → the mechanics companion (`Per-call cost of a registered call`, the pooled heap frontier, the root-set bounds).
* [`../../../CONTEXT.md`](../../../CONTEXT.md) → the crate-level CLI/exit-code/`system_info` surface.
* [`../../../M1_GAPS.md`](../../../M1_GAPS.md) · [`../../../TRACE_AND_PERF.md`](../../../TRACE_AND_PERF.md) · [`../../../PERF_DISPATCH.md`](../../../PERF_DISPATCH.md) → the gap index, the trace seam and the per-call measurement ledgers.
* [`../../../../beam_bifs/CONTEXT.md`](../../../../beam_bifs/CONTEXT.md) → the BIF registry and the fifteen host seams this path installs.

## The reference walk runs only where its answer is observable

`RuntimeContext::call_registered`'s result tail walks the result graph for a reference
(`RuntimeContext::graph_holds_reference`) **only when `move_container`** — i.e. on a process route
(`mailbox_table.is_some()`) or on a table-less call no escaping arm took (`safe_to_rewind`).
The walk's answer has exactly two consumers, both gated identically: this arm, which only sets
`rewind = false`, and the copy arm, which copies and then sets `rewind = safe_to_rewind`.
When `move_container` is false the copy arm cannot run and `rewind` is already `false`, so both
answers of the walk produce the same `result` and the same `rewind` — the walk is unobservable
there and is skipped.
A **table-less reentrant** call — any call carrying a fun argument (`lists:foldl/3`, `lists:map/2`,
`maps:map/2`, … where `!reentrant` is a conjunct of `safe_to_rewind`) — is therefore never walked:
that is the accumulator shape, a whole intermediate compiler AST, and it was the quadratic term in
the in-process `compile:file/2` route (contract A.9).
Measured on this change: walk visits fall 79.4 % on `lib/stdlib/src/string.erl` (408.5 M → 84.1 M),
82.2 % on `lib/compiler/src/v3_core.erl` (1.63 G → 290 M), 62.6 % on `maps.erl` and 60.3 % on
`ordsets.erl`, with every compiled artifact byte-identical to the oracle's; the remaining walk calls
(~87 % of the old count) are the non-reentrant ones, whose `[Fun|T]`-shaped results are legitimately
observable.

The walk's visit count is accumulated in a **local** `u64` and folded in once per walk through
`perf_counters::add_visits` at the walk's single exit (both early answers `break 'walk` into it), so
the diagnostic counter costs one `enabled()`/`borrow_mut` per walk rather than per visited object.
`#[cfg(test)] notes::note_walk_visit_fold` is called from the same statement, so the test-only
per-visit counter and the folded total are pinned to agree on every exit shape.

### Rejected experiments — do not retry these
* **Fusing reference detection with a per-visit pool-touch copy-skip** is a net loss: +2.4 %
  instructions on `maps.erl` and +6.8 % on `string.erl`, because the extra per-visit test is charged
  to a walk that still visits 10^8 objects.
* **A fused walk plus a live-region registry** carries the same net loss: the per-visit charge is the
  cost, not the lookup structure.
* **The structural fix** — making internal reference terms literal-area terms, so every copier shares
  them and no result ever needs the exemption — is a cross-crate change owned by
  `beam_term`/`beam_message`/`beam_gc`, not this node.
