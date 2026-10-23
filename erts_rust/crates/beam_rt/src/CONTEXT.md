# `beam_rt/src` — runtime source root

## Intent
The crate's implementation root: the `main.rs` shim, the `Runtime` façade (`runtime.rs`, `runtime/`),
the CLI (`cli.rs`, `cli/`), the concrete `ExecutionContext` glue (`context/`), the runtime-native BIF
floor (`native.rs`, `native/`), the A.4 port layer (`port.rs`, `port/`), the A.8 introspection surface
(`info.rs`, `info/`), the distribution node (`dist.rs`, `dist/`, `dist_client/`), the allocator
free-page release (`malloc.rs`) and the observability modules (`trace.rs`, `seq_trace.rs`,
`crash_dump.rs`, `perf_map.rs`, `debug_trace.rs`).
The crate-level API surface (CLI table, exit codes, `system_info` keys, boot status, gaps) lives in
`../CONTEXT.md`; the per-module detail lives in the child `CONTEXT.md`s below.

## Version / boot-script / system-root resolution (cross-cutting seam)
Binding facts for anyone touching A.6/A.7/A.9 (CLI, boot, introspection); verified against the code
and this host. These pieces are spread across `codeserver.rs`, `cli.rs`, `boot*.rs`, `info/system.rs`
and `native/{code,init}.rs`, so they are recorded here rather than in any one child node.

- **The version constants are compile-time.** `beam_common::{OTP_VERSION = "30.0-rc0",
  ERTS_VERSION = "17.1"}` (`../beam_common/src/version.rs:39,42`). `erlang:system_info(otp_release /
  version / system_version / compat_rel)`, the NIF `ErlDrvSysInfo` strings and the `-version` banner
  are all served from them — `info/system.rs:116,117,126-130,182`, `boot.rs:317-319` (banner, printed
  by `cli.rs:1596`), `nif.rs:476,480`.
- **The runtime has NO notion of its own root/release.** The code path/root is resolved **only from
  the filesystem**, preferring the `erl` on `$PATH` (`codeserver.rs::native_roots:660-683` — seed
  order: `erl`-on-`$PATH` ancestors, then `current_exe` ancestors, then `$ERL_TOP`, then
  `STANDARD_OTP_ROOTS`; each tried as itself and as `<seed>/lib/erlang`). The first root passing the
  `SYSTEM_APPS` probe (`stdlib`/`lists.beam` is the hard requirement) wins
  (`native_system_ebins:644-646`, `system_ebins_under:693-713`, `SYSTEM_APPS:825`). `bootstrap/` is
  never a candidate root. So with an `erl` on `$PATH`, the consumed
  root is ALWAYS that system install's root (`CodeServer::lib_root` at `codeserver.rs:335-340`), and
  that root then yields the whole release's ebin list through `codeserver/path.rs`
  (`release_ebins`/`user_lib_ebins`/`system_path`, the `code_server.erl` port), which reproduces the
  oracle's `code:get_path/0` exactly (`../../tests/code_path.rs`).
- **The default route loads THIS runtime's own release's boot script.** With no `-boot`,
  `cli.rs`'s `boot_script_to_drive` prefers `CodeServer::own_bootstrap_boot_script()` — the committed
  `bootstrap/bin/start.boot` of the source tree the binary runs from — so the default route drives the
  OTP-30 primary bootstrap (header `{"Erlang/OTP","30"}`) and `init:script_id/0` agrees with
  `erlang:system_info(otp_release)` (`"30"`). Only when no source tree is reachable (an installed
  deployment) does it fall back to `DEFAULT_BOOT_SCRIPT = "start"` → `resolve_boot_file` (candidate
  `<value>.boot`, else the `<Root>/bin/<value>.boot` string-concat fallback) → `<systemRoot>/bin/start.boot`.
  An **explicit** `-boot FILE` is untouched and still resolves exactly as `init`'s `get_boot/2` does.
- **`init:script_id/0`** (`native/init.rs`) reads `Runtime::boot_script_id`, set from the
  loaded `BootScript`'s `{Name,Vsn}` header at `runtime.rs` (`boot_from_script`) and
  `runtime.rs` (`boot_from_script_reporting`). With no loaded
  script it is the typed `unsupported: init:script_id/0 (no boot script has been loaded)` gap.
- **`CodeServer::lib_root()` stays the consumed SYSTEM install's `<lib>`** (it feeds
  `bindir`/`app_dir`/system-ebin resolution/`$ROOT` expansion), so it is never repointed at the source
  tree: `own_bootstrap_boot_script` is a **separate** discovery used only to pick the default script.
- **The runtime DOES know where its own source tree is at runtime**, without a build-time absolute path:
  `CodeServer::own_bootstrap_boot_script` walks `current_exe()`'s ancestors (bounded, exactly as
  `native_roots` does) and then `$ERL_TOP`, checking each for `bootstrap/bin/start.boot`. Tests still use
  an explicit repo path (`env!("CARGO_MANIFEST_DIR")`), which a relocated binary could not report
  truthfully. An explicit `-boot bootstrap/bin/start` stays **CWD-relative** (as on the oracle), so it
  only finds the committed script when the process CWD is the repo root.
- **Consumers of the resolved root** all yield the system install's paths (never this runtime's own
  tree): `codeserver.rs::app_dir:349-371` (`code:lib_dir/1,2`), `codeserver.rs::bindir:385-400`
  (`init:get_argument(bindir)` = `<root>/erts-<vsn>/bin`), `native/code.rs::code_lib_dir_0:426-434`
  (`code:lib_dir/0`), `native/code.rs::code_root_dir:650-661` (`code:root_dir/0`),
  `native/init.rs:74,108,215` (`init:get_argument(root|bindir)`, `init:get_arguments/0`),
  `cli.rs::boot_file_root:1868-1871`, `boot/bootscript.rs::expand_root:418-435` (the script's own
  `$ROOT` expansion).

## The `-eval` route's term storage and machine ownership (cross-cutting seam)
Binding facts for anyone touching `Action::Eval`, `Runtime::eval` or `crate::eval` (spread across
`runtime.rs`, `context/{pool,pump,calls}.rs` and `eval/mod.rs`, so recorded here).

- **No `Machine` spans a `-eval` expression.** `Runtime::eval` (`runtime.rs:2981`) builds one
  `RuntimeContext::new(&mut *runtime)` and hands it to `crate::eval::eval` (`eval/mod.rs:349`), whose
  `Evaluator` holds only `ctx: &'a mut RuntimeContext<'b>` (`eval/mod.rs:460-463`) — the tree-walker
  builds terms through `beam_engine::terms`/`beam_bifs`, never through an engine. The only
  `Machine`s on this route are the **per-nested-call, short-lived** ones in
  `context/pump.rs:268` (`run_export_blocking`) and `:356` (`run_label_blocking`), dropped at scope
  exit after `Runtime::adopt_escaped_binaries(machine.refc_binaries())` (`pump.rs:472`
  → `runtime.rs:1618`). So there is no long-lived `Machine::refc_binaries` reservoir to
  prune: the reservoir-equivalents are `Runtime` fields.
- **`RuntimeContext::new` sets `mailbox_table: None`** (`context/runtime_context.rs:176-191`), so on
  this route `ExecutionContext::alloc_words`/`alloc_words_rooted` (`context/execution.rs:498,509`)
  bypass the process heap entirely and go to `Runtime::alloc_words` → `Runtime::arena`
  (`runtime.rs:1414`, `arena: MinimalContext` at `:240`). Every tuple/list/map/float/bignum and every
  ≤64-byte binary the walker builds (`terms::make_*`) lives in that **pointer-stable** arena (no
  region of it ever moves, and nothing collects it); `gc_armed` is `false` (`runtime_context.rs:186`), so no collection ever runs there.
- **Four storage regions, all observable: one bounded, one depth-sized, two bounded by the region-reclaim pass.** `arena` (`:240`), `beam_engine`-owned, hands out words that stay at their address and is rewound above `src/runtime/reclaim.rs`'s mark once nothing reachable stands there; the pooled BIF heap
  `bif_heaps: Vec<Heap>` (`:222`, taken/returned by `take_bif_heap`/`put_bif_heap`, **depth-sized** —
  `bif_heap_words` for the outermost call, `NESTED_BIF_HEAP_SEED_WORDS` = 1 MiB for a nested one, self-tuned
  to `2 ×` an observed nested peak — and grown **geometrically** in `context/calls.rs take_bif_frontier`); `eval_heap: Heap`
  (`:271`) plus `eval_heap_retired: Vec<Heap>` (`:296`, filled by `grow_eval_heap`, whose growth is clamped
  at `MAX_EVAL_HEAP_WORDS` = 256 MiB — a retired region is **dropped whole** once the descent proves it
  whole-dead, and a partially live one is **evacuated** into a fresh region by the store move; both at the
  region-reclaim pass's two non-running sites, see `runtime/CONTEXT.md`); and
  `eval_off_heap: beam_message::OffHeapTable` (`:286`), the off-heap payload **owner** table.
  `Runtime::retention_stats() -> RetentionStats` (`runtime/retention.rs`) reads all four — arena
  demand words/calls, `bif_regions`/`bif_words`/`bif_peak_used_words`/`bif_max_capacity_words`/
  `bif_heaps_taken` (the live nesting depth)/`bif_nested_heap_words`/`bif_nested_peak_words`, `eval_heap_words`/`eval_heap_retired_regions`/
  `eval_heap_retired_words`, `eval_off_heap_handles` (MSO slots only — `OffHeapTable::len` excludes
  the table's refc binaries) — and `BEAM_RT_MEM_TRACE` prints it as one `[rt-mem] boot …` line after
  the boot and one `[rt-mem] exit …` line on the way out of `cli::run` (silent unless set).
  The two arena/pool fields are **capacity/reservation** figures, so they appear in `VmSize` and only their written pages in RSS; the store's two retired ones are not — a region is retired only once it is full, so `eval_heap_retired_words` is ~fully written and resident (`inet_parse`'s peak is 59 % retired store / 37 % arena written pages / 2.4 % glibc / 1 % other).- **A big binary literal is ON-heap.** `Evaluator::materialize_bits` (`eval/mod.rs:601-610`) sends a
  byte-aligned binary ≤64 bytes to `terms::make_heap_binary` (arena) and everything else —
  non-byte-aligned, or past `ERL_ONHEAP_BINARY_LIMIT` — to `beam_codec::term::make_bitstring` on
  `eval_heap`. `binary:copy/2` likewise builds **on-heap** on the pooled BIF heap
  (`beam_bifs/src/binary.rs:679-697`, refusing `system_limit` past the region's `available()*8`), and
  the container result is then re-homed onto `eval_heap` (`context/pool.rs:105
  move_result_into_runtime_store` ← `calls.rs:1395 rehome_container_result`, chosen because
  `mailbox_table.is_none()`).
- **Off-heap refc payloads on this route come only from a nested `Machine`** (`bs_init_writable`/
  `bs_create_bin` → `beam_engine/src/machine/mod.rs:791/834 refc_binaries`), and their handles are
  adopted into `eval_off_heap` before the machine drops. `beam_bifs` never builds one (no
  `OffHeapBinaries` host is installed anywhere in production — the handoff is still open per
  `beam_codec/CONTEXT.md`), and `stabilize_term` (`runtime.rs:1456`, the `-eval` single-pass form)
  **flattens** an unresolvable payload on-heap.
- **The runner's park prune cannot be reused here.**
  `context/gc/bin_vheap.rs:359 reclaim` needs `&mut ProcessTable` + a `Process` (for
  `collector().scanned_bounds()`) + `&mut Machine`; and its gate 6
  (`context/gc_walk.rs:381 reaches_an_unscanned_word`) uses `Criterion::PayloadPrune`
  (`gc_walk.rs:791-798`), which answers `hit = true` on the **first pointer-bearing word in
  `WordRegion::Foreign`** — and every eval-route root (arena / `eval_heap` / retired regions) is
  `Foreign` by construction (`context/gc.rs:382 word_region`, `WordRegion` at `:271`). So a prune
  against the walker's roots always declines, and
  `beam_gc::for_each_reachable_binary_payload` (`beam_gc/src/offheap.rs:107`, whose walk only enters
  a from-space word) can never see them either.
- **The only region-agnostic walker in this crate** is `RuntimeContext::graph_holds_reference`
  (`context/calls.rs:1221`) — a `#![allow(unsafe_code)]` `Walklist` walk over
  `beam_term::layout::for_each_child_slot` (`beam_term/src/layout.rs:303`) with the
  `dereferenceable_pointer`/`is_term_word`/`is_literal` guards, no visited set and a
  `WALK_VISIT_BUDGET`. It answers "does this graph hold a reference", not "which payloads does it
  name", but it is the template for one that could.
- **The walker's live-term holders** (a prune would have to be handed all of them): the `Env =
  HashMap<String, Eterm>` (`eval/mod.rs:175`) threaded through every `expr` call (`:469`, sequence at
  `:518-524 exprs`); `Evaluator::terms`' partially-built `Vec<Eterm>` and `list`'s `acc`
  (`:527-552`) — i.e. **recursion frames**, so a mid-expression hook is unsound; `Flow::Raise { reason,
  head }` (`:147-166`); the escaped closures' `Runtime::eval_closures: Vec<Rc<FunBody>>` with its own
  `Env` (`:182-187`, pushed by `register_fun` `:1624`); and whatever `eval_off_heap`/`eval_heap`
  themselves hold. `Runtime::as_action_process` (`runtime.rs:3059`) runs the body as a freshly spawned
  child and `stabilize_term_with`s only the **final** result (step 5).
- **`eval_off_heap` is NOT pid-less in practice: a scheduled `run`-route process grows it too.**
  `RuntimeContext::run_export` (the `ExecutionContext::call_bif` external-module route) is
  **blocking-aware for every caller**, so a `run`-route process calling e.g. `base64:encode/1` (an
  external-module entry into `base64.beam`, whose `bs_create_bin` accumulator passes
  `ERL_ONHEAP_BINARY_LIMIT` = 64 bytes) goes through `run_export_blocking`/`run_label_blocking` and
  their `runtime.adopt_escaped_binaries(machine.refc_binaries())` — see
  `adopt_escaped_binaries`'s own doc list of nested runs (`runtime.rs:1599-1613`). `pump_once` is the
  *same* path (it is the `step` closure both pass). `runtime/CONTEXT.md`'s "not attributed to any
  process" is about `process_info(P, binary)` attribution, **not** about who grows the table.
- **`beam_message::OffHeapTable` traps for anyone pruning it.** There is **no per-entry removal** —
  only `drain_binaries()` (all), so a prune is drain → filter → re-`push_binary`. `push_binary` never
  dedups (two pushes of one payload = two entries) and `binary_by_data_ptr` resolves only the
  **first** match; the `data_ptr` dedup lives in `adopt_escaped_binaries` (`runtime.rs:1618-1629`).
  `len`/`is_empty`/`iter`/`overhead_bytes` are **MSO-only** — after `push_binary`, `is_empty()` is
  still `true` — so a binary-side gate must read `binaries()`. A `GrowOutcome::Moved` realloc
  invalidates every previously handed-out `data_ptr`; re-read it from the live handles.
- **The raw-`Eterm` holders a prune must publish are already inventoried** (written for a *moving*
  heap, but the same roster is the prune's must-publish list — relocation-rooting asks for pointer
  updates, prune-rooting for read-reachability, so the requirement differs but the set does not):
  `context/IMPLEMENTATION.md` §*What a MOVING engine heap would invalidate — the at-risk roster*
  (`Runtime::persistent_terms`, `trace`'s `MatchSpec::Functions`, `seq_trace`'s `Token{label,from}`,
  `eval_closures`' `FunBody.env`, `timers`' `TimerAction::Deliver`) and its *Safe* list
  (`RunnerState.exits`/`results`, `references`/`Timers.refs` keys, ETS's own arena,
  `RuntimeContext`, `dist`/`port`, `PortMeta`). Two refinements found by inspecting the writers:
  `persistent_term:put/2`'s key+value and `TimerAction::Deliver.message` **are** stabilized
  (`context/calls.rs:713-722`; `context/runtime_hooks.rs:1343` and `native.rs:807`) and
  `trace_pattern` stabilizes its match-spec functions (`native/erts_internal/trace.rs:165-174`), so
  none of those three can name a `[BinRef][ErlSubBits]` payload (the simple copy flattens it) —
  whereas `seq_trace`'s `set_label` (`seq_trace.rs:324-329`, reached from `seq_trace_2:461`) stores
  the raw term **unstabilized**, and `eval_closures`' env can name one.
## The debt gate on the fun-applying stdlib fast paths (cross-cutting: execution · bridges · gc)
Binding facts for anyone touching `RuntimeContext::resolve_external_call`, `FunBridge` or the collection doors.
- **What it gates:** `lists`/`maps` keys whose `beam_bifs` implementation applies a fun argument back into the runtime — the roster is **not** restated in `beam_rt`: it is `beam_bifs::needs_fun_caller(module, function, arity)` (the registry's own allocation-free binary-search answer, backed by `FUN_CALLER_BIFS`' pinned 18 `lists` + 4 `maps` + 2 `ets` keys), narrowed to `module` ∈ {`lists`, `maps`} by `execution.rs`'s `nests_a_fun_apply`.
- **The predicate:** `RuntimeContext::calling_process_has_collection_debt` (`execution.rs`) reads `Process::collection_debt` for `mailbox_pid` off `mailbox_table`; a **table-less** context answers `false` (no process, no debt), which is what keeps the `-eval` route's synchronous dispatch byte-for-byte unchanged.
- **What a decline buys:** the key's real `.beam` body is entered **in this machine**, so the callee's registers/stack/frames join the caller's root set and every heap-admission point is the caller's **armed** one — the door that fires on `Process::collection_debt` and collapses the retired-nursery chain. The `lists`/`maps` bodies *are* the reference semantics of the fast paths, so no result changes; the seam's existing in-machine-identity caveats (`erlang:load_nif/2`, the fun-image BIFs) are outside this key set.
- **Why the gate must be a pure function of the heap state:** the native baseline tier consults `resolve_external_call` **twice** per `call_ext*` (`jit::baseline`'s `jit_ext_call`, then `jit_step_one`). The two consultations are back-to-back inside one instruction step with nothing executing in between, and `Process::grow_nursery` (the only setter) and a collection (the only clearer) both need execution — so the bit is constant for the instruction's duration and the second consultation cannot answer differently.
- **The two `ets` folds stay on the synchronous path on purpose:** `ets.erl`'s `foldl/3`/`foldr/3` reach `ets:first/1`/`next/2` and then a `lists` key internally, so gating them would relocate the nesting rather than remove it while replacing an implementation whose per-interior object ordering is pinned against the oracle.
## Reachability of a `-sname` node from a real peer (A.5, cross-cutting: dist · eval · scheduler)
Measured end-to-end with an OTP 29.1.1 peer (`erl -sname … -setcookie …`, same cookie as this runtime);
`node()` on both sides is `@<hostname>` (short), so no host-form mismatch is involved.
- **`net_adm:ping(<this node>)` WORKS whenever a scheduler pass runs.** `run <fixture> …` answers `pong`
  repeatedly (42–584 ms; three concurrent pongs) and `-eval` with repeated short receives answers `pong`.
  The `'N'` responder handshake, the `net_kernel` service's `is_auth` answer (`native/net_kernel.rs`), the
  alias reply (`dist/links.rs::route_alias` → `DOP_ALIAS_SEND`; `DFLAG_ALIAS` is negotiated by OTP 29) and
  the peer binding (`runtime/scheduler.rs:999`) are all live. A **wrong cookie** answers `pang` in ~20 ms
  (a real refusal) — that is the discriminator against the timeout form below.
- **The `-eval` `receive … after T` route pumps the scheduler every port-poll slice, so a peer is
  accepted however late it dials.** Both per-iteration waits of the `after` arm are bounded by
  `PORT_POLL_SLICE` (25 ms, the port poller's own budget, reused rather than duplicated) and the pump
  between them is the **non-blocking** `Runtime::pump_scheduler_now` (`TimerWait::ImminentWithin(0)`);
  the deadline is tested **before** the pump, so the wait ends on the **caller's** deadline rather
  than on a pump call's schedule: `pump_scheduler_now(RECEIVE_PUMP_SLICES)`,
  `poll_port_messages_into_mailbox(remaining.min(PORT_POLL_SLICE))` and
  `wait_for_timer(min(deadline, now + PORT_POLL_SLICE), …)` (`eval/mod.rs`, the `after` arm of
  `Evaluator::receive`), so `pump_dist` → `DistNode::pump` →
  `accept_one` is re-entered every slice, instead of charging the whole window to
  `PortRegistry::pump(timeout)` (which loops in `PORT_POLL_SLICE` `epoll_wait` slices for as long as any
  port is live — the booted kernel's `inet_gethost` child never closes, `port.rs:1802` — and would leave a
  peer's connection unaccepted in the listen backlog until `net_setuptime`). Measured
  `beam_rt -sname rustnode -setcookie c123 -noshell -eval 'receive after 25000 -> ok end.'` +
  `erl -sname peerpos … net_adm:ping(rustnode@billmini)` ⇒ **`pong` in 0.38 s**. The observable timing of
  the arm is the oracle's — `after 1/25/50/100/300/1000` all land on their own deadline, a message that
  arrives in a slice wins over the timeout, and a pending timer inside a slice is still slept to —
  pinned by the colocated `eval::tests::{a_long_after_window_is_waited_out_in_port_poll_slices,
  a_sub_slice_after_window_never_loops}` over a test-only per-thread iteration counter and by
  `eval::tests::{an_after_window_fires_on_its_own_deadline,
  a_message_that_beats_the_deadline_is_picked_up_promptly}` over the wall clock.
- **`DistNode::is_idle` (`dist/node.rs:273` → `links.rs:200`) means "no live LINK".** The listener and a pending
  accept are not counted as work, so a distributed node with nothing to run is `is_quiescent`
  (`scheduler.rs:581`). The `run` route only survives an idle `receive` because the live `inet_gethost`
  port keeps `has_nothing_to_poll()` false; without it a long-lived server node would exit
  `unsupported: run: process blocked`, and `RUN_DEADLINE` (60 s) bounds that route anyway.
- **This node is PUBLISHED to real peers (and `-hidden` is honoured).** Both distribution halves
  advertise `dist.rs::advertised_flags(hidden)` — the emulator default
  (`beam_dist::handshake::preferred_flags`, `erts_dflags.default`) **OR**
  `beam_dist::handshake::publish_flag(hidden, true)`, i.e. `DFLAG_PUBLISHED` (`0x01`) unless the node
  was started `-hidden` (a real node adds that bit per peer, `lib/kernel/src/dist_util.erl:152-160`;
  `net_kernel.erl:hidden_argument/0` reads the `-hidden` init argument). The acceptor
  (`DistConnection::new` → `ConnectionDriver::server_with_flags`) and the connector
  (`dist_client/client.rs::connect_addr` → `ConnectionDriver::client_with_flags`) are told the same value by
  `Runtime::new` through the additive `DistNode::set_hidden`/`DistClient::set_hidden`, since visibility
  is a property of the node, not of who dialled. Against an OTP 29.1.1 peer: a pinged
  **published** node appears in the peer's `nodes()`, a pinged
  `-hidden` node appears only in `nodes(hidden)`/`nodes(connected)`, and a **dialled** link
  (`net_adm:ping/1` driven from inside this runtime) puts this node in the peer's `nodes()` too. A
  hidden *peer* stays recorded-not-refused (`DistConnection::peer_visibility`,
  `a_hidden_and_a_published_peer_both_establish_and_are_recorded`).

## Contract A.5 — measured inbound/outbound behaviour limits (live vs OTP 29.1.1)
Measured against a real system `erl` (`-sname` only on this host). Facts that bind any A.5 work:

- **Inbound large-term delivery falls back to a heap fragment past the receiver's young space.** `DOP_SEND`/`DOP_REG_SEND` payloads whose term exceeds the receiver's default nursery reach it through the **heap-fragment** model: a message-and-receive-pass receiver is `Running`, so its roots are unpublished and `Process::deliver` returns the copier's **raw refusal** (no collection, no growth) — which `LocalDeliverySink::enqueue_fragmented` (`dist/inbound.rs`) turns into a `beam_message::send_fragmented`, the C's `erts_try_alloc_message_on_heap`/`new_message_buffer` path. The bound is the default nursery's arithmetic — 117 cons cells is 234 words against a 233-word young space — so it is **list length**, not wire bytes: a `lists:seq(1,N)` with `N ≥ 117` (either `STRING_EXT` or `LIST_EXT`) needs the fragment route, while a 600-byte **binary** does not (past 64 bytes it is an off-heap refc payload of 8 heap words).
- **A delivery refusal is an outcome, never a link failure.** `enqueue`'s `Alloc` arm answers `Delivered` (fragment route) or, when even the fragment cannot be allocated, `Dropped` — never `Err`, so an oversized message from a peer cannot retire the link. Pinned by `dist/inbound/tests.rs` (three tests: the fragment delivery with its `take_fragment_deliveries` record, "a delivery refusal is never a link failure", and the unchanged `NoSuchProcess`).
- **The refusal is receiver-heap capacity, never the codec or the copier.** `binary_to_term(term_to_binary(L))` round-trips at 300 elements, and a real cross-process copy (`spawn` + `P ! {N,L}`, 100–1000 elements) delivers every size — the refusal is exactly the receiver-heap capacity refusal the fragment route exists for.
- **Inbound delivery needs the target name registered before a peer sends.** Six oracle peers dialling in one at a time, each sending one message immediately after its own handshake, deliver 6 of 6 when the receiver registered its name first; a send that lands before the name exists is dropped, so an inbound assertion must register the receiver first.
- **A decoded arrival can carry a term that is not a proper list**, on which `length/1` raises `badarg` and the CLI prints `beam_rt: badarg` (exit 1).
- **Outbound dials on demand**: a local send to a remote pid, a registered `{Name, Peer}` or an alias reference emits the frame on an established link, **dialling the peer first when no link exists** (`Runtime::dist_ensure_link`/`dist_ensure_link_to_node`, reached through `dist_send_reg_send`/`dist_send_message_pid`/`dist_send_alias`), so atoms/tuples/binaries/maps/130-element lists are delivered with **no prior `net_adm:ping/1`**; only a peer the dial cannot reach answers the typed `distribution: no established connection to node <label>` (exit 1), never a silent drop. `-eval` cannot `~w`-print a term holding a **foreign pid**: `beam_rt: unsupported: external_term` (`beam_codec/src/error.rs:43`).
- **`rpc:call` is unachievable in both directions** until the distributed-spawn family exists: OTP 29 `rpc:call/5` is `erpc` (`lib/kernel/src/rpc.erl:492` → `erpc:call`), which is `spawn_request/5` + `DOP_SPAWN_REQUEST`/`DOP_SPAWN_REPLY` — not the legacy `rex` `{call,M,F,A,Gleader}` shape. Both halves are missing: `erlang:spawn_request/1..5` are in `error.rs`'s `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`, `erlang:spawn/4` is absent from `native/process.rs`'s dispatch (⇒ bare `undef`), and the inbound family is `Unsupported` at `dist/inbound.rs`'s `SpawnRequest`/`SpawnReply` arm (no `beam_process` spawn-at/reservation API and no reply channel on `InboundSink`).
- **Cross-node monitors work in both directions for a pid**: us→oracle (`DOP_MONITOR_P` out, `DOP_MONITOR_P_EXIT` back, ref/pid `=:=`, reason `normal`) and oracle→us (a peer's `erlang:monitor(process, OurPid)` records a `RemoteWatcher`, and our process's death routes the `DOP_MONITOR_P_EXIT` its record owes — the peer's `{'DOWN', …}` arrives on the reference **it** minted, and its `demonitor` suppresses it — `tests/live_signals.rs`). The remaining gap is a **named** monitor: oracle→us on `{Name,Node}` is `native/process/monitor.rs::named_node_target` (`unsupported: erlang:monitor/2 on a {Name, Node} of another node`, exit 3).
- **Oracle `monitor_node/2` on us agrees**: `{nodedown,'rustnode@billmini'}` (bare 2-tuple, no reason) arrives on our death; no `{nodeup,_}` is emitted on a later connect — a real-OTP↔real-OTP **control reproduces exactly this**, so it is oracle behaviour, not a divergence.
- **A `-sname` node survives a connected peer**: `kernel_sup`'s `global_name_server` → `net_kernel:monitor_nodes(true, #{connection_id => true})` → `nodes(visible, #{connection_id => true})` answers the peer with its **real per-link integer** (`native/net/nodes.rs::nodes_2` over `Runtime::dist_connection_id`), never a capability gap, so the peer can dial in and outlive any pass (`erl -sname peerprobe -setcookie c123` pings `rustnode@…` ⇒ `pong` with `kernel_sup`/`global_name_server` still live, and the fixture module's own `nodes(visible, #{connection_id => true})` prints `[{peerprobe3@billmini,#{connection_id => 1}}]`).

## Module literal areas — shared by word, owned by the `Module` (cross-cutting)
Binding facts for anyone asking "where does a module constant live, and can it be copied or freed?".

- **A module's `LitT` literals live in one immutable `beam_alloc::Heap` owned by the module**
  (`beam_loader/src/literal.rs:76-90`), reached as `Module::literal(idx) -> Option<Eterm>`
  (`beam_loader/src/ir.rs:425-432`), held behind `Arc<LiteralArea>`
  (`beam_loader/src/literal.rs:177-182`) and literal-tagged exactly once at load
  (`beam_loader/src/literal.rs:112-124` → `beam_term::literal::tag_literal_area`,
  `beam_term/src/literal.rs:162-170`). `Module::literal` returns a **copy of the word**, i.e. a
  pointer into that area.
- **`beam_rt` never resolves, rebuilds or copies a module literal.** No `.literal(`/`LiteralArea`
  use exists in this crate's production code (the engine resolves `Operand::Literal` against
  `&Module`), and the arena (`Runtime::alloc_words`, `runtime.rs:2351`; the admission seam
  `context/gc.rs:1536-1578`) only ever carries **newly built** terms.
- **Every copy this crate performs shares a literal verbatim, at zero destination words.**
  `Runtime::stabilize_term`/`stabilize_term_with` (`runtime.rs:2399`/`:2553`), `copy_result_fresh`
  (`context/calls.rs:1878-1890`), the two re-homing arms (`context/pool.rs:105`,
  `context/calls.rs:1480`) all drive `beam_message`'s copier, whose `is_shared`
  (`beam_message/src/copy.rs:267-275`) returns a literal-tagged compound word unchanged
  (`shallow` `:506-508`, `push_fixup` `:297-301`). A **root** literal is therefore skipped by every
  re-homing check by design: `result_is_foreign_container` (`context/calls.rs:1131-1134`), the
  immediates/literals skip (`context/calls.rs:889-890`) and `relocate_off_scratch`
  (`context/calls.rs:1361-1365`).
- **No collector or window walk touches a literal.** `word_region` answers `None` for it
  (`context/gc.rs:382-400`), and the same literal test guards the valve oracle
  (`context/gc/valve_bounds.rs:104`), the reachability descent (`context/gc_walk.rs:174`), the
  statement-window reclaim (`eval/reclaim.rs:472`, `eval/reclaim/resource.rs:313`) and the window
  latch test (`eval/scratch.rs:390`).
- **Consequence: an object on a process heap, the arena, `eval_heap` or a pooled BIF heap may hold
  a literal word as a raw child, and that is sound only while the owning `Module` is alive.**
  Retention is `CodeServer.modules: HashMap<String, Arc<Module>>` (`codeserver.rs:124`) plus every
  live clone: `ProcessEntry::Export{module}` (`runner/entry.rs:47-56`), `LocalFunTarget.module`
  (`context/apply.rs:66`), the fun-identity snapshot (`codeserver.rs:144-151`), the thread-local
  descriptor index (`context/ir_cache.rs:272-278`), `get_until`'s continuation
  (`native/file_server/get_until.rs:235`).
- **Known issue — there is no literal-area release handshake.** The code store's one removal really
  unloads (`erlang:delete_module/1` → `context/runtime_hooks.rs:824` → `CodeServer::forget_module`,
  `codeserver.rs:820-828`; a reload replaces the entry, `codeserver.rs:772-780`), each dropping the
  cache's `Arc`, while `erts_internal:purge_module/2` and
  `erts_literal_area_collector:release_area_switch/0` are the oracle's `notsup`/unimplemented
  (`context/runtime_hooks.rs:42,1518`) and no old-code slot exists. A term kept in a runtime store
  that carries a literal **word** — a `persistent_term` value (`context/persistent_term.rs:32-77`),
  a timer's `Msg` (`context/runtime_hooks.rs:1343`), a `seq_trace` label (`seq_trace.rs:287-292`),
  and any copier-based store such as ETS or a mailbox — can therefore outlive the module whose
  literal area it names. ERTS prevents this with the old-code slot plus the release handshake;
  this runtime has neither.
- **An `-eval` "literal" is a different thing**: the expression has no module literal area, so the
  evaluator builds each source literal on the run's store every time (`eval/mod.rs:774-783`
  bitstrings, `:2665-2679` integers, the string literal builder), and an `erlang:spawn/1` closure
  is compiled into a real `beam_rt_eval_fun_<n>` module (`eval/compile.rs:180,445`) whose literal
  area then belongs to the code server like any other module's.

## Runtime-level term stores — which region each word lives in (relocation inventory)
Binding for any work that would MOVE a term region (a compacting store, a relocating arena): a raw
`Eterm` in a `Runtime` field/Vec outlives every `beam_engine` call, so it must be rewritten or it
dangles. `Runtime` is at `runtime.rs:289-1082`; `stabilize_*` copies land on **`Runtime::eval_heap`**
(`runtime.rs:454`, the copy is `beam_message::copy_struct_simple(term, &mut self.eval_heap)` at
`:2462`, and `copy_struct(term, &mut self.eval_heap, &mut ctx)` at `:2636`) — **not** on the arena,
whatever the `try_stabilize_escaped` call-site comments say.
| field (line) | holds | built on | relocation status |
|---|---|---|---|
| `arena: MinimalContext` (`:398`) | every tuple/list/map/float/bignum a table-less context builds (`alloc_words`, `:2377`) | the arena's own `ContextHeap` (`heap_mark`/`heap_mark_matches`/`for_each_heap_region`/`heap_rewind`) | **pinned** only as far as a word's address goes; the region above `src/runtime/reclaim.rs`'s recorded mark is rewound when nothing reachable stands in it |
| `eval_heap: Heap` (`:454`) | `stabilize_term`/`_with` copies, BIF container results, non-byte-aligned bitstrings | itself | **relocatable in principle** — the ONE rewritable store (`eval/scratch.rs:360` `reset_to`), nothing outside it may name a word in it without being enumerated |
| `eval_heap_retired: Vec<Heap>` (`:480`) | regions `grow_eval_heap` (`:2536`) replaced | themselves | **not pinned as a chain**: the region-reclaim pass drops a whole-dead one (freeing its carrier) and the store move evacuates a partially live one, so only regions holding a word the corpus names at one of the two non-running sites survive — a *live* word must stay at its address between those sites |
| `eval_off_heap: OffHeapTable` (`:470`) | refc/Mso payload handles the copies depend on | off-heap payloads (not words) | pinned; also the prune's target |
| `info_heap: Heap` (`:431`) | `system_info`/`format_term` scratch; reset per call (`:2245`, latched by `:2257`) | itself | pinned (rewound, and `relocate_off_scratch` runs first) |
| `bif_heaps: Vec<Heap>` (`:348`) | pooled per-call BIF regions (rewound, `context/calls.rs:1052`) | themselves | pinned regions; a word a store keeps must come from a region the pool never re-issues as a rewound call region — this is why `references`/`Timers::refs` keys are minted on a **taken-and-returned** region (`native/process.rs:1231`, `native.rs:883`, `context/runtime_hooks.rs` `to_eterm(heap)`) |
| `references: HashMap<Eterm, …>` (`:755`) | reference **terms** as keys (address identity) | pooled BIF heap (mints at `native/process.rs:176`, `native/process/monitor.rs:280,304`, `nif.rs:516,635`, `trace.rs:826`) | pinned + address-identity; a compaction that touched these regions breaks the key |
| `eval_closures: Vec<Rc<FunBody>>` (`:898`) | `FunBody.env: HashMap<String, Eterm>` | arena **or** `eval_heap` (`-eval` route is table-less) | relocatable **iff** the env is rewritten; gathered as roots (`runtime.rs:2998`) |
| `eval_compiled: FastHashMap<u32, Option<CompiledClosure>>` (`:910`) | `CompiledClosure.captures: Vec<Eterm>` (`eval/compile.rs:123`), copied out of `FunBody.env` (`eval/compile.rs:195`) | same words as the closure env | **relocatable — a second copy of the closure env**: `gather_off_heap_roots` walks `eval_closures` only, so a moving pass must update `captures` in lockstep with `FunBody.env`; the mutable walk does reach it (as `RootFamily::EvalCompiledCaptures`, `runtime/roots.rs`), the value gather does not |
| `timers: Timers` (`:920`) | `Pending{action: TimerAction, reference: Option<Eterm>}`; `Deliver{from, message}` | `message` = `eval_heap` (`try_stabilize_escaped`, `native.rs:817`, `context/runtime_hooks.rs:1343`); `from` = a pid **immediate**; `reference` = a pooled-BIF-heap mint | message relocatable (rooted, `timers.rs:432`); `reference` is also a `Timers::refs` **key** ⇒ pinned |
| `timer_wake_targets: TimerWakes` (`:927`) | `(handle → ProcessId)` | ids only | no terms |
| `pending_spawns: Vec<PendingSpawn>` (`:965`) | `PendingSpawn.args: Vec<Eterm>` (`:229`) + `ProcessEntry::Fun{fun}` | `eval_heap` via `try_stabilize_escaped` (`:4173`,`:4184`) | relocatable (rooted, `:3026`) |
| `persistent_terms: Rc<RefCell<PersistentTermStore>>` (`:621`) | `entries: Vec<(Eterm, Eterm)>` (`context/persistent_term.rs:80`) | `eval_heap` (both args stabilized at `context/calls.rs:752-756`) | relocatable, but **deliberately not a gather root** (`:2957-2962`: the copy flattens an off-heap payload) |
| `seq_trace: SeqTrace` (`:735`) | `Token{label: Eterm, from: Eterm}` (`seq_trace.rs:175,179`) | **whatever heap the BIF argument stood on** — `set_label` (`seq_trace.rs:343`) stores it **unstabilized** | the one store written raw on purpose; rooted for the payload prune (`:3002` → `seq_trace.rs:292`) but **not** stabilized for a moving heap |
| `trace: TraceState` (`:336`) | `PatternEntry.match_spec: MatchSpec::Functions(Vec<Eterm>)` (`trace.rs:368`) | `eval_heap` (`native/erts_internal/trace.rs:169`), installed at `trace.rs:462` | relocatable, **deliberately not a gather root** (`:2963-2964`: stabilized copy flattens off-heap payloads) |
| `node_monitors: FastHashMap<ProcessId, Vec<NodeMonitorLatch>>` (`:888`) | `NodeMonitorLatch.node: Eterm` (`:1128`) | the shared atom table | **immediates only** — never moves |
| `local_node: Atom` (`:1014`) | an atom **index**, not a term; `local_node_term()` (`:4705`) builds one on demand | — | no stored word |
| `cookie: Vec<u8>` (`:1006`) | bytes | — | no term |
| `runner_state: RunnerState` (`:396`) | engine args (`arena`-stable), parked machines' root sets | arena | pinned; rooted via `runner/entry.rs:490` |
| `mailboxes` (`:318`) | `MailboxState` — see `mailbox.rs` | mixed | see below |
| `code_server: CodeServer` (`:298`) | `Arc<Module>`s ⇒ module **literal areas** (`beam_alloc::Heap` owned by the module) | literal areas | pinned for the `Arc`'s life; a stored literal word is a pointer into it |
### `Runtime::gather_off_heap_roots` (`:2996`) — the root enumeration, in order
`eval_closures` env (`:2998`) · `seq_trace` tokens (`:3002`) · every live process's dict + mailbox `payload/token/from` + `registers` + `saved_args` + `signals` (`:3008-3024`) · `pending_spawns` args + entry fun (`:3026`) · `RunnerState` (`:3040`, whose own walk is `runner/entry.rs:490-517`) · pending timer messages (`:3046`).
**Not enumerated by the gather, each with its stated mechanism** (the *mutable* twin, `runtime/roots.rs`, enumerates `persistent_term`, the trace match specs and `eval_compiled.captures` as well — see `runtime/CONTEXT.md`): the runtime's term *regions* (handled by `for_each_off_heap_region`, `:3066` — which visits `eval_heap`, `eval_heap_retired`, `bif_heaps`, `info_heap` and **not** the arena, although `MinimalContext::for_each_heap_region` (`beam_engine/src/context.rs:2147`) exists) · `persistent_term` (`:2957`) · trace match specs (`:2963`) · ETS objects (`:2965`) · `references` (`:2970`) · `eval_compiled.captures` (not mentioned at all).
### The store's rewinds, its drops and its one move
`eval_heap` is rewritten at four places, all of them non-running sites: the `-eval` statement window (`eval/scratch.rs:360 reset_to(window.mark)`, whose every handing-out site pins — `runtime.rs:2458`, `:2618`), region *replacement* (`grow_eval_heap`, `:2536`, which retires the old region, so it never moves a word in place), and the region-reclaim pass's two actions — the current region's dead prefix `reset_to(cursor)` and, on the move, the fresh destination installed as the new current region while the old one is reset and retired or dropped. `MAX_EVAL_HEAP_WORDS = 1 << 25` (`:170`); the ceiling is `MAX_EVAL_HEAP_WORDS.max(eval_heap.capacity())` (`:2566`), so a bigger configured store is never shrunk.
- **The store's WRITTEN volume is not in `RetentionStats`.** `eval_heap_words` is `eval_heap.capacity()` (`runtime/retention.rs:335`) — the *reserved* figure — so `BEAM_RT_MEM_TRACE=1` cannot show a regression whose mechanism is "more copy-out lands in the store": capacity stays at the 128 MiB seed while `Runtime::eval_heap.used()` grows. The used figure is in-process only (its one reader is the window latch, `runtime.rs:2889`); a store-write question therefore needs a `used` field added, or the ready-made A/B harness `context/calls/tests.rs::eval_route_pool_costs` (pool high-water vs `eval_heap.used()` per copy-out variant).
- **A nested-run engine block is now part of the process's SCANNED live set.** `context/gc.rs::alloc_words_rooted`'s unarmed arm delegates to `alloc_words` (`:1667`), so a block the `-eval` evaluator, a cross-module callee's second `Machine` or an applied fun builds lands on the running process's young heap instead of the arena — and growth is gated by `may_replace_young_space` (`:1477`, `:1561`, `:1723`), which refuses inside the exempt window `context/pump.rs:278`/`:366` opens. The two consequences a memory measurement must separate: those words are now *copied* by every collection and *inflate the live-set-sized nursery* (resident up), while the arena reserves one ladder rung less (VmSize down).
### `mailbox.rs` — positions and payload handles, no raw terms except the marker's reference
`MailboxState` (`mailbox.rs:146`): `cursor: QueueCursor` (`:148`, an index — not a term) · `markers: Vec<Marker>` (`:150`) where `Marker.term` is `make_small` (`:338`) but `Marker.reference: Option<Eterm>` (`:129`) is the **raw** `make_ref` word `recv_marker_bind` recorded (`:357`, bound at `:361`) · `retained_fragments: Vec<Fragments>` (`:163`), `retained_binaries: Vec<RefcBinary>` (`:177`), `retained_off_heap: Vec<MsoHandle>` (`:189`, in message-slot order), `retained_nodes: Vec<NodeEntry>` (`:199`) — payload handles, not words. `remove_at` (`:253`) is the only producer; there is **no release point** (`Runtime::mailboxes` is never removed from), which is the soundness mechanism for a word that escaped the message.

## Term-carrying stores in `nif*.rs` · `codeserver.rs` · `boot*` (raw-word inventory)
What in these files holds a raw `Eterm` across a call boundary, and on which heap (§ *Host-side
term stores* in `../CONTEXT.md` is the crate-wide rule; this is the sibling inventory those files
have no entry for). Verified by reading the fields, not inferred.
- **No NIF-side term is on the arena or the `-eval` store except the two `stabilize_term` copies.**
  A per-call `NifHostImpl` heap is the **pooled BIF heap** (`context/calls.rs:1698` `take_bif_frontier`
  → `:1737-1739` `put_bif_heap`); the load/unload host's is a dedicated
  `beam_alloc::Heap::new(bif_heap_words)` owned by `NifEntry.host` (`context/bridges.rs:642`,
  `nif.rs:189-198`). Neither region is compacted, so the hosts' words are stable by construction.
- `nif.rs:294-302` `PendingNifSend { from: ProcessId, to: Eterm, msg: Eterm }` — `msg` is on the
  **load host's own heap**; the `Vec` lives in the long-lived `NifhostImpl.load_sends`
  (`nif.rs:712`) between the callback's push (`nif.rs:1278`) and `take_load_sends` (`nif.rs:755`), so
  it is per-load-callback, never runtime-lifetime.
- `nif.rs:696` `NifHostImpl.self_pid: Eterm` — a **pid immediate** (no heap word); lives as long as
  the host (library lifetime for the load host).
- `nif.rs:702` / `nif/thread_env.rs:224` `envs: Vec<Box<NifEnv>>` — `NifEnv`'s own
  `pending_exception: Option<Eterm>` (`beam_abi/src/nif/host.rs:603`) and its
  `recorded_atoms: Rc<RefCell<Vec<(usize, String)>>>` (raw atom **word**, atoms are immediates,
  `host.rs:496`); both are per env, and an env a NIF leaked lives as long as the host.
- `nif.rs:516` and `nif.rs:635` (`make_ref`, `monitor_process_result`) insert the minted reference
  **term** into `Runtime::references: HashMap<Eterm, Option<MonitorRef>>` (`runtime.rs:755`).
  `Eterm`'s `Hash` is the raw word (`beam_term/src/eterm.rs:36-37`), so those keys are the reference
  block's **address** on the host heap — a store keyed by identity-of-address, not by id. Same shape
  for `Timers::refs` / `native::process::reference_term`.
- `nif_select.rs:189-197` is the only term this module **stabilises**: `reference` (or the
  `CUSTOM_MSG` message) → `Runtime::stabilize_term` → `Runtime::eval_heap`
  (`runtime.rs:2452-2462`) → stored as `SelectPayload::{Plain{reference}|Custom{message}}`
  (`port/select.rs:146-159`) in `SelectWatch.registrations: HashMap<u64, SelectRegistration>`
  (`port/select.rs:418`, field `payload` at `:401`; the sibling `handle: Eterm` at `:403` is
  `make_small(fd)`, an immediate, and `obj: *mut c_void` at `:405`). **So a NIF select registration
  holds words inside the `-eval` store** — the one cross-file landing point compaction must relocate.
  The delivery-time resource mint is the other copy: `nif.rs:776-787` → `port.rs:4305-4308`
  `stabilize_term` (same store).
- `nif_monitor.rs:98` `armed: HashMap<MonitorId, ProcessId>` — `MonitorId([u8; REF_THING_BYTES])`
  (`beam_abi/src/nif/enif/resources_monitors.rs:104`), a **byte copy** of the ref block (value
  identity), and a `ProcessId`; no term, no address. `deliver_for`'s `pid_term` is a parameter.
- `codeserver.rs` holds **no `Eterm` at all** (zero occurrences in the file). `CodeServer`
  (`:108-181`) is `atoms: Rc<AtomTable>`, `modules: HashMap<String, Arc<Module>>`, two
  `String`-keyed location maps, `loaded_generation`, `fun_modules: RefCell<Option<(u64,
  Rc<Vec<(Arc<Module>, PathBuf)>>)>>`, path vectors. The `Arc<Module>` does carry a literal area —
  `beam_loader/src/ir.rs:315 literals: Arc<LiteralArea>`, `:425-427 literal(idx) -> Option<Eterm>`
  returns a **word into that area** — and `erlang:delete_module/1` drops the `Arc`
  (`CodeServer::forget_module`), so a host store caching a literal word outlives its area (the
  documented "no literal-area release handshake" issue).
- `boot.rs` holds no term (`PreloadedModules { names: Vec<&'static str> }` at `:245-247`;
  `PRELOADED_MODULE_NAMES` / `CORE_ATOMS` are `&[&str]`). `boot/preloaded.rs` has none either.
- `boot/bootscript.rs:518` `boot_args -> Vec<Eterm>` is a **per-directive local** built through
  `RuntimeContext::new` → `Runtime::alloc_words`, i.e. the **arena**; `intern_atoms`'s
  `HashMap<String, Eterm>` (`:529`) holds atom immediates only. The `Vec` is handed straight to
  `drive::start_kernel_process` (`:480`, parameter `args: Vec<Eterm>` at `drive.rs:209`) or
  `drive::apply_boot_call` (`:484`, `&[Eterm]` at `drive.rs:353`) and never stored — but
  `Runtime::spawn_process` → `note_pending_spawn` re-copies each arg onto the arena
  (`runtime.rs:4181-4185`) into `PendingSpawn.args: Vec<Eterm>` (`runtime.rs:229`), which **is** a
  struct field. `BootScript`/`Instruction`/`Mfa.args` (`bootscript.rs:129-182`) are owned typed
  `Value`s — no term, so a parsed script survives any term movement. `drive.rs` `Mfa`-carried
  `args` and `apply_boot_call`'s machine registers are per-call.

## File-size overages
The `../CONTEXT.md` *Files ~1000 lines* constraint is not met by the pre-existing oversize below, which are **not** restructured (line counts measured with `wc -l`; a "production" count excludes the file's trailing test region — the contiguous run of `#[cfg(test)]`-gated modules at EOF, inline or sibling).
- `src/port.rs` 5 281 lines, 5 194 of production (unit suite in `port/tests.rs`, 4 226 lines, plus 22 sibling `port/*_tests.rs` suites — `accept_inherit`, `active_input`, `active_input_e2e`, `async_ready`, `busy_suspend`, `busy_wait`, `command_matrix`, `completion`, `control_capability`, `driver_output_shape`, `exit_status_matrix`, `failure`, `failure_e2e`, `inet_matrix`, `inet_opts`, `monitor`, `packet_framing`, `peer_closed`, `ram_file_matrix`, `soft_busy_builtin`, `spawn_driver`, `stream_shape` — and `port/test_support.rs`).
- `src/runtime.rs` 6 027 lines — 4 973 of production, the rest the colocated unit test module (from line 4 974).
- `src/port/driver.rs` 3 911 lines, 3 502 of production: the driver C-ABI host (`DriverPort`/`DriverPortAdapter` — the concrete `beam_abi::DriverHost` behind a driver's `ErlDrvPort`), with its suites as the sibling `port/driver/{control,control_reply_oracle,ddll,fire,hooks,liveness,monitor,port_attributes,port_term,ready_output,stop_select}_tests.rs` modules declared from its tail.
- `src/context/gc.rs` 3 257 lines, 2 367 of production — the remainder is the test-only counters modules (`valve_counters`/`prefix_counters`) and the inline suites; its unit suite **belongs** in `context/gc/tests.rs` (still owed; the `port.rs`/`port/tests.rs` pattern).
- `src/native/code.rs` 2 902 lines, 1 033 of production: the `code:*` floor (the path/load/name readers and the path mutators) with its oracle-cross-checked colocated suite in the remaining 1 869 lines.
- `src/codeserver.rs` 1 869 lines, 1 306 of production: the shared `Rc<AtomTable>`, the module cache with both location maps, the code path, the file-load routes and the store's one removal op, plus its colocated unit suite (563 lines).
- `src/context/gc_walk.rs` 2 156 lines: 1 159 of production (the valved reachability descent, whose module docs carry the soundness argument and the latch-clearing refutation), the test-only `seed_counters` module and the root-set pinning suite, which **must** stay colocated because it drives the module's private `Walk`/`WalkScratch` types.
- `src/context/runtime_hooks.rs` 1 823 lines of production with no inline tests — the `RuntimeHooks` bridge, whose per-arm rustdoc blocks carry the oracle columns and the served/fixed/gap reasoning (suites `context/tests.rs` and `tests/runtime_hooks.rs`).
- `src/context/calls.rs` 2 511 lines, 2 334 of production (the BIF-dispatch + BIF-result-copy-out machinery; the 177-line tail is the test-only dispatch notes plus the declaration of the suite in `context/calls/tests.rs`, 1 677 lines).
- `src/runtime/scheduler.rs` 2 011 lines, 1 813 of production (the pass family), the remaining tail being inline pins for the code they measure plus the declaration of the sibling suite (`runtime/scheduler/tests.rs`, 1 389 lines).
- `src/error.rs` 1 333 lines, 777 of production: the `pub(crate)` `BeamError` constructors, the halt helpers, `enforce_term_heap_limit`, `unsupported_reason`/`live_reason_term` and the `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` set with its drift guard, plus its colocated suite (556 lines).
- `src/native/file_server.rs` 1 185 lines, 1 183 of production (the io-device request dispatch table; suite in `native/file_server/tests.rs`, 1 970 lines).
- `src/port/failure_e2e_tests.rs` 863 lines — a **test-only** suite (declared from `port/failure.rs`'s tail), so it is legitimately long: it compiles its own real C `.so` and drives one row per oracle opcode through the whole `port_command → output → FailureLatch → settle → signal_failure → drain_failures → push_ready` road, asserting each published `PortSignal` list whole.
- `src/port/peer_closed_tests.rs` 1 520 lines — **all test-only** (declared from `port/peer_closed.rs`'s tail), so the ~1000-line target does not bind it: it holds the peer-close channel's whole evidence, the mirror/latch units, the real-loopback delivery routes, and two spent-grant cases whose doc comments carry the quoted C lines plus the oracle's exact printed answers beside the two `erl` cross-checks of those same sequences.
- `src/port/driver/port_term_tests.rs` 761 lines — a **test-only** suite (declared from `port/driver.rs`'s tail) for the `ERL_DRV_PORT` hook: the stamped handle↔`PortId` round trip through the real term-spec decoder, the foreign/null/unstamped refusals, the gone bit after `clear_driver_state`, and an end-to-end row through a real C driver that emits `{ERL_DRV_PORT, driver_mk_port(port)}`.
- `src/port/failure.rs` 208 lines, 201 of production — **within target**; it is the driver-failure channel's runtime half (`signal_reason`, `PortRegistry::drain_failures`/`fail_port`, the ready-queue publication), declared inside `exit.rs`, and its two suites are `port/failure_tests.rs` (159, the reason-mapping units) and `port/failure_e2e_tests.rs` (above).
- **Other files past the target in production lines** (one responsibility each, no split prescribed; same measurement): `eval/mod.rs` 4 049 / 2 876, `cli.rs` 3 684 / 2 029, `native/prim_file.rs` 1 601 / 1 601, `runner/run.rs` 1 585 / 1 585, `nif.rs` 2 964 / 1 524, `native/memory/mod.rs` 1 423 / 1 419, `context/pump.rs` 1 453 / 1 399, `eval/parse.rs` 1 642 / 1 284, `native/erts_internal.rs` 2 839 / 1 273, `native/process.rs` 1 883 / 1 238, `context/bridges.rs` 1 357 / 1 212, `native.rs` 2 032 / 1 210, `context/execution.rs` 1 141 / 1 141 (its debt gate added the predicate, the roster seam and their rustdoc; no test region, the suite is `context/tests.rs`), `native/prim.rs` 4 066 / 1 139, `dist_client/client.rs` 1 079 / 1 079, `trace.rs` 1 850 / 1 074, `dist/inbound.rs` 1 011 / 1 008.
- **Files past ~1000 total lines whose production count stays under the target** (module docs and/or a trailing test region carry the rest; no split owed): `runtime/gc.rs` 1 645 / 962, `native/application.rs` 1 661 / 953, `cli/flags.rs` 1 210 / 864, `config.rs` 1 126 / 823, `boot/bootscript.rs` 2 103 / 801, `info.rs` 1 225 / 769, `seq_trace.rs` 1 052 / 761, `native/port.rs` 1 814 / 733, `timers.rs` 1 107 / 644, `native/init.rs` 1 270 / 548, `native/process/monitor.rs` 1 047 / 512, `context/gc/bin_vheap.rs` 1 110 / 466, `context/process_bridge.rs` 1 302 / 463, `mailbox.rs` 1 056 / 397.

## Routing Table
- `boot.rs` + `boot/` → preloaded-module set (`PreloadedModules`/`CORE_ATOMS`), the boot banner, the
  boot-script reader/executor and the system code-path resolution (`boot/CONTEXT.md`).
- `cli.rs` + `cli/` → the A.6 CLI parser/driver, the four `EXIT_*` codes and the boot-script drive
  (`cli/CONTEXT.md`).
- `context/` → the concrete `ExecutionContext` glue, the host-seam bridges and the NIF-dispatch order
  (`context/CONTEXT.md`), including `context/execution.rs`'s **debt-gated fun-applying seam**
  (`resolve_external_call`'s `nests_a_fun_apply` ∧ `calling_process_has_collection_debt`, own section
  above); its children own one subject each — `context/calls/CONTEXT.md` (the registered-call
  path: the gated dispatch arms, the lent/held `TableRegistry`, `PendingInterner`, the `[safe]` export gate),
  `context/pool/CONTEXT.md` (the BIF-result copy-out: the pooled frontier, the table-less arm, the learned
  copy-out hint, the aliased `ets` witness) and `context/gc/CONTEXT.md` (the collection doors, the unscanned-root
  valve, the refc-binary reclamation and the NIF-resource prune) — with `context/IMPLEMENTATION.md` holding the
  remaining dispatch/heap seam detail; the `load`-callback `enif_send` window is drained by `NifBridge::load_nif`
  (`context/bridges.rs`) the moment the callback returns, over the pass's own process table.
- `native.rs` + `native/` → the runtime-native BIF floor (`try_native`, before the registry), the
  native `file_server_2` service and the code/init/application/memory floors (`native/CONTEXT.md`, whose
  per-family children are `native/memory/CONTEXT.md` (A.8 `process_info`), `native/process/CONTEXT.md`
  (the process-control families and the on-demand collection), `native/erts_internal/CONTEXT.md` and
  `native/file_server/CONTEXT.md`).
- `info.rs` + `info/` → the A.8 `system_info`/`process_info` introspection surface, incl. this file's
  version producers (`info/CONTEXT.md`).
- `port.rs` + `port/` → the A.4 `PortHook` provider, the port registry/poller and the driver C-ABI
  host (`port/CONTEXT.md`).
- `runtime.rs` + `runtime/` → the `Runtime` façade, the scheduler-pass family and the GC/ETS-owner
  state (`runtime/CONTEXT.md`); its pid-keyed bookkeeping (`group_leaders`/`async_dist`/`monitor_nodes`/
  `timer_resumes`) and the `runtime/timer_wakes.rs` `TimerWakes` mirror are `crate::hasher` maps.
- `runtime/reclaim.rs` + `runtime/reclaim/` → the **region-reclaim pass** over the runtime's two long-lived
  term regions, **two mechanisms at two sites** (`runtime/CONTEXT.md` *The region-reclaim pass*): the
  whole-region actions (`reclaim.rs` — drop a whole-dead `eval_heap_retired` region, reset the current
  region's dead prefix, Tier A `arena.heap_rewind`) and the **store move** (`reclaim/compact.rs` — evacuate a
  partially live region into one fresh region through `beam_gc`'s `Relocator`, gated by the post-move
  verification and its `KeptBoth` obligation); the descent runs only at the `-eval` statement boundary and
  the driver exit, its corpus is `Runtime::for_each_root_mut` + the caller's `MoveSlots`, and the residual is
  **Phase 2**: a nested in-process `compile:file/2`'s in-flight machine roots and the evaluator's Rust-frame
  terms have no `&mut` reachability at any in-run point, so the single-compile peak is unchanged.
- `runner/` → the `beam_process::ProcessRunner` (`runner/CONTEXT.md`, whose measurement record is
  `runner/MEASURED.md`); `RunnerState`'s four pid-keyed
  maps (`engines`/`results`/`exits`/`errors`) are `crate::hasher::FastHashMap`.
- `dist.rs` + `dist/` + `dist_client/` → the distributed node, accepting and connecting halves
  (`dist/CONTEXT.md`, `dist_client/CONTEXT.md`).
- `eval/` → the `-eval` evaluator (AST/parser/`Flow`) (`eval/CONTEXT.md`).
- `malloc.rs` → the crate's marked `unsafe` module and its **one** allocator lever:
  `release_free_pages()` (`libc::malloc_trim(0)` on glibc, an honest `None` elsewhere), called once per
  process from `main.rs` **after the driver thread has been joined** — the only quiescent *and* complete
  point, because the driver thread still holds most of the run's memory until it exits (a trim from inside
  `cli::run`, even after the runtime it built has been dropped, recovers only ~16 MB of ~75 MB). Measured
  exit RSS (glibc 2.44; arena `in_use` ≤ 218 kB at that point in every case): bare boot 90 760 → 18 400 kB,
  `compiler/cerl_clauses.erl` 211 168 → 19 068 kB, `stdlib/orddict.erl` 218 544 → 22 240 kB. Peak `VmHWM`
  is **unchanged** (115 / 314 / 305 MB) because during the run that memory is *live*, not slack (a bare
  boot's idle arena is 97 681 kB in use of 97 948 kB) — the census, the arena/heap layout, the measured
  non-effect of a trim at the boot→action boundary (266 kB free there) and the fragmentation limit are in
  the module docs. `BEAM_RT_MEM_TRACE` prints the call's answer as one `[rt-mem] heap-release=…` line.
- `mailbox.rs` → the per-pid scan state, and the **fragment lifetime invariant** that makes a received term usable after `remove_message`.
  A removed message's `Fragments` chain is *moved* into `MailboxState::retained_fragments` (`mailbox.rs:267-268`) — retention, never `Fragments::move_into`, which no site in the workspace calls.
  **There is no release point:** `Runtime::mailboxes` is only ever `entry()`/`get()`/`get_mut()`ed, never removed, so every fragment a process ever removed stays alive for the runtime's lifetime (the field's own doc, "released when the process's scan state is", cannot be reached).
  That absence *is* the soundness mechanism for a payload word that escaped the message (the process heap, a container built around a received subterm, the `seq_trace` label, an ETS `copy_struct` source), and it is also why a receiver collection leaves such a payload alone (`Target::Foreign`: `beam_gc/src/copy.rs:334-361` classifies anything outside the from-set verbatim and does not descend into it).
  Consequence for any future cleanup: adding a `mailboxes.remove(pid)` on process death would be a use-after-free for every raw-word store.
  The two `Process::receive(|_| true)` **dequeue** sites (`native/file_server.rs:342`, `native/net_kernel.rs:215`) bypass the retention entirely: they are sound only because the reply envelope they derive is deep-copied (`beam_message::copy::copy_struct` takes no source-heap argument, so a fragment-resident subterm is copied, never shared) inside the same loop iteration, while the `Message` binding is still alive.
- `codeserver.rs`, `error.rs`, `config.rs`, `format.rs`, `malloc.rs`, `timers.rs` (a min-heap of
  absolute deadlines over `crate::hasher::FastHashMap` indexes, whose `prune_cancelled` reclaims
  non-head cancelled tombstones with a watermark heap rebuild), `nif.rs`, `hasher.rs`
  (the crate's one deterministic non-cryptographic `BuildHasher` — `FastHasher`/`FastBuildHasher`/
  `FastHashSet`/`FastHashMap` — used by the per-pid verdict-only structures, never a keyed-by-`Arc<str>`
  or security-sensitive map), the observability modules and `main.rs` → documented in `../CONTEXT.md` (the crate routing table).

## See Also
- `../CONTEXT.md` → the crate-level API surface, CLI table, exit codes, `system_info` keys and gaps.
- `../M1_GAPS.md` · `../DESIGN_DECISIONS.md` · `../TRACE_AND_PERF.md` · `../TEST_STRATEGY.md` ·
  `../PERF.md` → companion docs.
