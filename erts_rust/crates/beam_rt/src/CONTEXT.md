# `beam_rt/src` — runtime source root

## Intent
The crate's implementation root: the `main.rs` shim, the `Runtime` façade (`runtime.rs`, `runtime/`),
the CLI (`cli.rs`, `cli/`), the concrete `ExecutionContext` glue (`context/`), the runtime-native BIF
floor (`native.rs`, `native/`), the A.4 port layer (`port.rs`, `port/`), the A.8 introspection surface
(`info.rs`, `info/`), the distribution node (`dist.rs`, `dist/`, `dist_client/`), and the
observability modules (`trace.rs`, `seq_trace.rs`, `crash_dump.rs`, `perf_map.rs`, `debug_trace.rs`).
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
  by `cli.rs:1217`), `nif.rs:476,480`.
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
  `cli.rs::boot_file_root:1443-1446`, `boot/bootscript.rs::expand_root:418-435` (the script's own
  `$ROOT` expansion).
- **Stale doc comment to be aware of:** `native/init.rs:397-401` claims the boot script this runtime
  loads is the committed `bootstrap/bin/start.boot` with header `{"Erlang/OTP","30"}`. That is not
  what happens on a host with an `erl` on `$PATH` — the resolved default is the system install's
  script (see above).

## The `-eval` route's term storage and machine ownership (cross-cutting seam)
Binding facts for anyone touching `Action::Eval`, `Runtime::eval` or `crate::eval` (spread across
`runtime.rs`, `context/{pool,pump,calls}.rs` and `eval/mod.rs`, so recorded here).

- **No `Machine` spans a `-eval` expression.** `Runtime::eval` (`runtime.rs:2981`) builds one
  `RuntimeContext::new(&mut *runtime)` and hands it to `crate::eval::eval` (`eval/mod.rs:349`), whose
  `Evaluator` holds only `ctx: &'a mut RuntimeContext<'b>` (`eval/mod.rs:460-463`) — the tree-walker
  builds terms through `beam_engine::terms`/`beam_bifs`, never through an engine. The only
  `Machine`s on this route are the **per-nested-call, short-lived** ones in
  `context/pump.rs:270` (`run_export_blocking`) and `:340` (`run_label_blocking`), dropped at scope
  exit after `Runtime::adopt_escaped_binaries(machine.refc_binaries())` (`pump.rs:282-283`,
  `:349-350` → `runtime.rs:1618`). So there is no long-lived `Machine::refc_binaries` reservoir to
  prune: the reservoir-equivalents are `Runtime` fields.
- **`RuntimeContext::new` sets `mailbox_table: None`** (`context/runtime_context.rs:176-191`), so on
  this route `ExecutionContext::alloc_words`/`alloc_words_rooted` (`context/execution.rs:498,509`)
  bypass the process heap entirely and go to `Runtime::alloc_words` → `Runtime::arena`
  (`runtime.rs:1414`, `arena: MinimalContext` at `:240`). Every tuple/list/map/float/bignum and every
  ≤64-byte binary the walker builds (`terms::make_*`) lives in that **pointer-stable, never-freed**
  arena; `gc_armed` is `false` (`runtime_context.rs:186`), so no collection ever runs there.
- **Four storage regions, none ever freed.** `arena` (`:240`); the pooled BIF heap `bif_heaps:
  Vec<Heap>` (`:222`, taken/returned by `take_bif_heap`/`put_bif_heap` `:1398,1406`, grown by
  **region retirement** in `context/calls.rs:158 take_bif_frontier`); `eval_heap: Heap` (`:271`) plus
  `eval_heap_retired: Vec<Heap>` (`:296`, filled by `grow_eval_heap` `:1511`); and `eval_off_heap:
  beam_message::OffHeapTable` (`:286`), the off-heap payload **owner** table.
- **A big binary literal is ON-heap.** `Evaluator::materialize_bits` (`eval/mod.rs:601-610`) sends a
  byte-aligned binary ≤64 bytes to `terms::make_heap_binary` (arena) and everything else —
  non-byte-aligned, or past `ERL_ONHEAP_BINARY_LIMIT` — to `beam_codec::term::make_bitstring` on
  `eval_heap`. `binary:copy/2` likewise builds **on-heap** on the pooled BIF heap
  (`beam_bifs/src/binary.rs:679-697`, refusing `system_limit` past the region's `available()*8`), and
  the container result is then re-homed onto `eval_heap` (`context/pool.rs:105
  move_result_into_runtime_store` ← `calls.rs:1175 rehome_container_result`, chosen because
  `mailbox_table.is_none()`).
- **Off-heap refc payloads on this route come only from a nested `Machine`** (`bs_init_writable`/
  `bs_create_bin` → `beam_engine/src/machine/mod.rs:791/834 refc_binaries`), and their handles are
  adopted into `eval_off_heap` before the machine drops. `beam_bifs` never builds one (no
  `OffHeapBinaries` host is installed anywhere in production — the handoff is still open per
  `beam_codec/CONTEXT.md`), and `stabilize_term` (`runtime.rs:1456`, the `-eval` single-pass form)
  **flattens** an unresolvable payload on-heap.
- **The runner's park prune cannot be reused here.**
  `context/gc/bin_vheap.rs:352 reclaim` needs `&mut ProcessTable` + a `Process` (for
  `collector().scanned_bounds()`) + `&mut Machine`; and its gate 6
  (`context/gc_walk.rs:381 reaches_an_unscanned_word`) uses `Criterion::PayloadPrune`
  (`gc_walk.rs:791-798`), which answers `hit = true` on the **first pointer-bearing word in
  `WordRegion::Foreign`** — and every eval-route root (arena / `eval_heap` / retired regions) is
  `Foreign` by construction (`context/gc.rs:370 word_region`, `WordRegion` at `:259`). So a prune
  against the walker's roots always declines, and
  `beam_gc::for_each_reachable_binary_payload` (`beam_gc/src/offheap.rs:107`, whose walk only enters
  a from-space word) can never see them either.
- **The only region-agnostic walker in this crate** is `RuntimeContext::graph_holds_reference`
  (`context/calls.rs:1049`) — a `#![allow(unsafe_code)]` `Walklist` walk over
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
  `PORT_POLL_SLICE` (25 ms, the port poller's own budget, reused rather than duplicated) and the wait ends
  only once the **caller's** deadline has elapsed: `poll_port_messages(remaining.min(PORT_POLL_SLICE))`
  and `wait_for_timer(min(deadline, now + PORT_POLL_SLICE), …)` (`eval/mod.rs`, the `after` arm of
  `Evaluator::receive`), so `Runtime::pump_scheduler` — and with it `pump_dist` → `DistNode::pump` →
  `accept_one` — is re-entered every slice, instead of charging the whole window to
  `PortRegistry::pump(timeout)` (which loops in `PORT_POLL_SLICE` `epoll_wait` slices for as long as any
  port is live — the booted kernel's `inet_gethost` child never closes, `port.rs:1802` — and would leave a
  peer's connection unaccepted in the listen backlog until `net_setuptime`). Measured
  `beam_rt -sname rustnode -setcookie c123 -noshell -eval 'receive after 25000 -> ok end.'` +
  `erl -sname peerpos … net_adm:ping(rustnode@billmini)` ⇒ **`pong` in 0.38 s**. The observable timing of
  the arm is exact (the window still ends it; a message that arrives in a slice still wins over the
  timeout, and a pending timer inside a slice is still slept to), pinned structurally by the colocated
  `eval::tests::{a_long_after_window_is_waited_out_in_port_poll_slices,
  a_sub_slice_after_window_never_loops}` over a test-only per-thread iteration counter.
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
  is a property of the node, not of who dialled. Measured against an OTP 29.1.1 peer: a pinged
  **published** node now appears in the peer's `nodes()` (`[pubprobe@billmini]`, was `[]`), a pinged
  `-hidden` node appears only in `nodes(hidden)`/`nodes(connected)`, and a **dialled** link
  (`net_adm:ping/1` driven from inside this runtime) puts this node in the peer's `nodes()` too. A
  hidden *peer* stays recorded-not-refused (`DistConnection::peer_visibility`,
  `a_hidden_and_a_published_peer_both_establish_and_are_recorded`).

## Contract A.5 — measured inbound/outbound behaviour limits (live vs OTP 29.1.1)
Measured against a real system `erl` (`-sname` only on this host). Facts that bind any A.5 work:

- **Inbound large-term delivery: the sink now falls back to a heap fragment.** `DOP_SEND`/`DOP_REG_SEND` payloads whose term is a **list of ≳117 elements** reach the receiver through the **heap-fragment** model: a message-and-receive-pass receiver is `Running`, so its roots are unpublished and `Process::deliver` returns the copier's **raw refusal** (no collection, no growth) — which `LocalDeliverySink::enqueue_fragmented` (`dist/inbound.rs`) turns into a `beam_message::send_fragmented`, the C's `erts_try_alloc_message_on_heap`/`new_message_buffer` path. The threshold is the default nursery's arithmetic: 117 cons cells is 234 words against a 233-word young space, which is why a 600-byte **binary** was never affected (past 64 bytes it is an off-heap refc payload of 8 heap words). Measured before the fallback, one message per fresh link: `lists:seq(1,115)` (`STRING_EXT`, 118 B body) arrived; `lists:seq(1,118)` (121 B) and `lists:seq(1000,1117)` (`LIST_EXT`, 118 elems) did not; `binary:copy(<<"y">>,600)` (605 B) did. So the bound is **list length**, not wire bytes.
- **A delivery refusal is an outcome, never a link failure.** `enqueue`'s `Alloc` arm answers `Delivered` (fragment route) or, when even the fragment cannot be allocated, `Dropped` — never `Err`, so an oversized message from a peer cannot retire the link. Pinned by `dist/inbound/tests.rs` (three tests: the fragment delivery with its `take_fragment_deliveries` record, "a delivery refusal is never a link failure", and the unchanged `NoSuchProcess`).
- **The refusal was never the codec.** The same shapes are fine locally: `binary_to_term(term_to_binary(L))` round-trips at 300 elements (`beam_codec` is correct), and a real cross-process copy (`spawn` + `P ! {N,L}`, 100–1000 elements) delivers every size (`copy_struct`/`Process::deliver` are correct — the refusal is the receiver-heap *capacity* refusal the fragment route exists for).
- **Sequential distinct-peer inbound delivery is reliable once the target name is registered.** Measured on a private `epmd`: six separate oracle peers dialled in one after another, each sending one message (`{echo, Node} ! msgN`) immediately after its own handshake, and **6 of 6** arrived when the receiver had registered the name before the first peer started; with the peers launched as soon as the node was published to `epmd` (i.e. before the `run` target process registered) the **first** message was dropped and the remaining 5 arrived, and a peer that waited 1 s after its handshake had its message delivered — so the only loss measured was a send landing before the name existed, and inbound delivery must be measured with the receiver registered first.
- **A malformed arrival can surface as a fatal `badarg`.** When the wrong-shape message does arrive, `length/1` on it raises `badarg` and the CLI prints `beam_rt: badarg` (exit 1) — i.e. the decode can yield a term that is not a proper list.
- **Outbound is the strong direction, and it dials on demand**: a local send to a remote pid, a registered `{Name, Peer}` or an alias reference emits the frame on an established link, **dialling the peer first when no link exists** (`Runtime::dist_ensure_link`/`dist_ensure_link_to_node`, reached through `dist_send_reg_send`/`dist_send_message_pid`/`dist_send_alias`), so atoms/tuples/binaries/maps/130-element lists are delivered with **no prior `net_adm:ping/1`**; only a peer the dial cannot reach answers the typed `distribution: no established connection to node <label>` (exit 1), never a silent drop. `-eval` cannot `~w`-print a term holding a **foreign pid**: `beam_rt: unsupported: external_term` (`beam_codec/src/error.rs:43`).
- **`rpc:call` is unachievable in both directions** until the distributed-spawn family exists: OTP 29 `rpc:call/5` is `erpc` (`lib/kernel/src/rpc.erl:492` → `erpc:call`), which is `spawn_request/5` + `DOP_SPAWN_REQUEST`/`DOP_SPAWN_REPLY` — not the legacy `rex` `{call,M,F,A,Gleader}` shape. Both halves are missing: `erlang:spawn_request/1..5` are in `error.rs`'s `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`, `erlang:spawn/4` is absent from `native/process.rs`'s dispatch (⇒ bare `undef`), and the inbound family is `Unsupported` at `dist/inbound.rs`'s `SpawnRequest`/`SpawnReply` arm (no `beam_process` spawn-at/reservation API and no reply channel on `InboundSink`).
- **Cross-node monitors are one-directional**: us→oracle works (`DOP_MONITOR_P` out, `DOP_MONITOR_P_EXIT` back, ref/pid `=:=`, reason `normal`); oracle→us on `{Name,Node}` is the documented gap at `native/process/monitor.rs::named_node_target` (`unsupported: erlang:monitor/2 on a {Name, Node} of another node`, exit 3).
- **Oracle `monitor_node/2` on us WORKS**: `{nodedown,'rustnode@billmini'}` (bare 2-tuple, no reason) arrives on our death; no `{nodeup,_}` is emitted on a later connect — a real-OTP↔real-OTP **control reproduces exactly this**, so it is oracle behaviour, not a divergence.
- **A `-sname` node survives a connected peer**: `kernel_sup`'s `global_name_server` → `net_kernel:monitor_nodes(true, #{connection_id => true})` → `nodes(visible, #{connection_id => true})` answers the peer with its **real per-link integer** (`native/net/nodes.rs::nodes_2` over `Runtime::dist_connection_id`), never a capability gap, so the peer can dial in and outlive any pass (measured: `erl -sname peerprobe -setcookie c123` pings `rustnode@…` ⇒ `pong` with `kernel_sup`/`global_name_server` still live, and the fixture module's own `nodes(visible, #{connection_id => true})` prints `[{peerprobe3@billmini,#{connection_id => 1}}]`).

## File-size overages
The `../CONTEXT.md` *Files ~1000 lines* constraint is not met by the pre-existing oversize below, which are **not** restructured (line counts measured with `wc -l`; a "production" count excludes the file's own test region — everything from its first test-module declaration to EOF, whether that declaration opens an inline module or a sibling `#[path]` one).
- `src/port.rs` 4 076 lines, 4 020 of production (unit suite lives in `port/tests.rs`, 3 824 lines, plus the sibling `port/{driver_output_shape,inet_matrix,ram_file_matrix,completion,control_capability,monitor,spawn_driver,packet_framing,stream_shape}_tests.rs` and `port/test_support.rs`).
- `src/runtime.rs` 4 032 lines — 3 168 of production, the rest the colocated unit test module (from line 3 169).
- `src/port/driver.rs` 3 313 lines, 2 920 of production: the driver C-ABI host (`DriverPort`/`DriverPortAdapter` — the concrete `beam_abi::DriverHost` behind a driver's `ErlDrvPort`), with its suites as the sibling `port/driver/{fire,port_term,control,ddll,stop_select,ready_output,liveness,monitor,hooks}_tests.rs` modules declared from its tail.
- `src/context/gc.rs` 3 210 lines, 1 802 of production — the remainder is the test-only counters modules (`valve_counters`/`prefix_counters`) and the inline suites; its unit suite **belongs** in `context/gc/tests.rs` (still owed; the `port.rs`/`port/tests.rs` pattern).
- `src/native/code.rs` 2 902 lines, 1 033 of production: the `code:*` floor (the path/load/name readers and the path mutators) with its oracle-cross-checked colocated suite in the remaining 1 869 lines.
- `src/codeserver.rs` 1 879 lines, 1 329 of production: the shared `Rc<AtomTable>`, the module cache with both location maps, the code path, the file-load routes and the store's one removal op, plus its colocated unit suite (550 lines).
- `src/context/gc_walk.rs` 1 853 lines: 857 of production (the valved reachability descent, whose module docs carry the soundness argument and the latch-clearing refutation), the test-only `seed_counters` module and the root-set pinning suite, which **must** stay colocated because it drives the module's private `Walk`/`WalkScratch` types.
- `src/context/runtime_hooks.rs` 1 823 lines of production with no inline tests — the `RuntimeHooks` bridge, whose per-arm rustdoc blocks carry the oracle columns and the served/fixed/gap reasoning (suites `context/tests.rs` and `tests/runtime_hooks.rs`).
- `src/context/calls.rs` 1 604 lines, 1 558 of production (the BIF-dispatch + BIF-result-copy-out machinery; the tail is the test-only dispatch notes plus the suite in `context/calls/tests.rs`, 866 lines).
- `src/runtime/scheduler.rs` 1 518 lines, 1 396 of production (the pass family), the 122-line tail being two inline pins for the code they measure plus the declaration of the sibling suite (`runtime/scheduler/tests.rs`, 1 225 lines).
- `src/error.rs` 1 311 lines, 755 of production: the `pub(crate)` `BeamError` constructors, the halt helpers, `enforce_term_heap_limit`, `unsupported_reason`/`live_reason_term` and the `KNOWN_UNIMPLEMENTED_ERLANG_BIFS` set with its drift guard, plus its colocated suite (556 lines).
- `src/native/file_server.rs` 1 207 lines of production of 1 209 (the io-device request dispatch table; suite in `native/file_server/tests.rs`, 1 970 lines).
- `src/port/failure_e2e_tests.rs` 848 lines — a **test-only** suite (declared from `port/failure.rs`'s tail), so it is legitimately long: it compiles its own real C `.so` and drives one row per oracle opcode through the whole `port_command → output → FailureLatch → settle → signal_failure → drain_failures → push_ready` road, asserting each published `PortSignal` list whole.
- `src/port/driver/port_term_tests.rs` 761 lines — a **test-only** suite (declared from `port/driver.rs`'s tail) for the `ERL_DRV_PORT` hook: the stamped handle↔`PortId` round trip through the real term-spec decoder, the foreign/null/unstamped refusals, the gone bit after `clear_driver_state`, and an end-to-end row through a real C driver that emits `{ERL_DRV_PORT, driver_mk_port(port)}`.
- `src/port/failure.rs` 200 lines, 193 of production — **within target**; it is the driver-failure channel's runtime half (`signal_reason`, `PortRegistry::drain_failures`/`fail_port`, the ready-queue publication), declared inside `exit.rs`, and its two suites are `port/failure_tests.rs` (159, the reason-mapping units) and `port/failure_e2e_tests.rs` (above).
- **Other files past the target in production lines** (one responsibility each, no split prescribed; same measurement): `eval/mod.rs` 3 616 / 2 655, `cli.rs` 2 968 / 1 791, `native/prim_file.rs` 1 601 / 1 601, `runner/run.rs` 1 450 / 1 450, `native/erts_internal.rs` 2 839 / 1 273, `native/memory/mod.rs` 1 273 / 1 269, `eval/parse.rs` 1 593 / 1 235, `native/process.rs` 1 841 / 1 196, `native/prim.rs` 4 066 / 1 139, `trace.rs` 1 850 / 1 074, `context/bridges.rs` 1 167 / 1 022, `nif.rs` 2 385 / 1 160.

## Routing Table
- `boot.rs` + `boot/` → preloaded-module set (`PreloadedModules`/`CORE_ATOMS`), the boot banner, the
  boot-script reader/executor and the system code-path resolution (`boot/CONTEXT.md`).
- `cli.rs` + `cli/` → the A.6 CLI parser/driver, the four `EXIT_*` codes and the boot-script drive
  (`cli/CONTEXT.md`).
- `context/` → the concrete `ExecutionContext` glue, the host-seam bridges and the NIF-dispatch order
  (`context/CONTEXT.md`); the `load`-callback `enif_send` window is drained by `NifBridge::load_nif`
  (`context/bridges.rs`) the moment the callback returns, over the pass's own process table.
- `native.rs` + `native/` → the runtime-native BIF floor (`try_native`, before the registry), the
  native `file_server_2` service and the code/init/application/memory floors (`native/CONTEXT.md`).
- `info.rs` + `info/` → the A.8 `system_info`/`process_info` introspection surface, incl. this file's
  version producers (`info/CONTEXT.md`).
- `port.rs` + `port/` → the A.4 `PortHook` provider, the port registry/poller and the driver C-ABI
  host (`port/CONTEXT.md`).
- `runtime.rs` + `runtime/` → the `Runtime` façade, the scheduler-pass family and the GC/ETS-owner
  state (`runtime/CONTEXT.md`); its pid-keyed bookkeeping (`group_leaders`/`async_dist`/`monitor_nodes`/
  `timer_resumes`) and the `runtime/timer_wakes.rs` `TimerWakes` mirror are `crate::hasher` maps.
- `runner/` → the `beam_process::ProcessRunner` (`runner/CONTEXT.md`); `RunnerState`'s four pid-keyed
  maps (`engines`/`results`/`exits`/`errors`) are `crate::hasher::FastHashMap`.
- `dist.rs` + `dist/` + `dist_client/` → the distributed node, accepting and connecting halves
  (`dist/CONTEXT.md`, `dist_client/CONTEXT.md`).
- `eval/` → the `-eval` evaluator (AST/parser/`Flow`) (`eval/CONTEXT.md`).
- `codeserver.rs`, `error.rs`, `config.rs`, `format.rs`, `mailbox.rs`, `timers.rs` (a min-heap of
  absolute deadlines over `crate::hasher::FastHashMap` indexes, whose `prune_cancelled` reclaims
  non-head cancelled tombstones with a watermark heap rebuild), `nif.rs`, `hasher.rs`
  (the crate's one deterministic non-cryptographic `BuildHasher` — `FastHasher`/`FastBuildHasher`/
  `FastHashSet`/`FastHashMap` — used by the per-pid verdict-only structures, never a keyed-by-`Arc<str>`
  or security-sensitive map), the observability modules and `main.rs` → documented in `../CONTEXT.md` (the crate routing table).

## See Also
- `../CONTEXT.md` → the crate-level API surface, CLI table, exit codes, `system_info` keys and gaps.
- `../M1_GAPS.md` · `../DESIGN_DECISIONS.md` · `../TRACE_AND_PERF.md` · `../TEST_STRATEGY.md` ·
  `../PERF.md` → companion docs.
