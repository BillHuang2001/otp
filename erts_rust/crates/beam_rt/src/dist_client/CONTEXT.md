# `beam_rt::dist_client` — the connecting (client) half of distribution (contract A.5, phase 2)
## Intent
A registry of *outbound* distribution links this runtime dials, the routing of the process layer's outbound link/monitor/exit signals onto them, and the sending of messages to a peer.
[`crate::dist`](../dist/CONTEXT.md) is the listening half; this node is the mirror — it dials a peer resolved through `epmd`, drives the client side of the same `'N'` handshake, and carries a two-way `DOP_*` session on the resulting link.
It is wiring, not protocol code: every byte of the wire lives in the sibling `beam_dist` crate (`ConnectionDriver::client`, `send_control`, `beam_dist::epmd::lookup_port_at`, `ConnectionDriver::dispatch_inbound_checked`).
What this node adds is the *driving state* the runtime owns: a socket per peer, a bounded pump, a failure-recorded retirement policy and the [`DistSignalRouter`] the process table installs.
## API Surface (the frozen façade — `mod.rs`)
`pub use client::DistClient;` — the registry the runtime holds and calls (`new`, `connect`/`connect_addr`, `pump`, `dispatch_inbound`, `send_reg_send`/`send_message_pid`/`send_alias`, `last_error`, `is_connected`/`is_connected_any_half`/`is_connected_to_node`, `connected_nodes`/`all_connected_nodes`/`connection_id_for`, `has_links`, `link_names` (every link of either half, **any** handshake state — the peer-loss "still linked" view), `is_idle`, `has_pending_outbound`, `set_identity`/`set_creation`/`set_hidden`, `use_lost_peers` (installs the runtime's shared peer-loss recorder), `router`).
`pub(crate) use client::DistClientInner;` — the shared state every half borrows (the links cell, the outbound queue, the three scratch heaps); crate-visible because a sibling's rustdoc names it.
`pub(crate) use handshake::ClientLink;` — one outbound link; crate-visible because the one node→link table (`../dist/links.rs`) shares it with the accepting half.
`pub(crate) use router::DistRouter;` — the `beam_process::DistSignalRouter` implementation installed on the runtime's process table.
`pub const DIST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);` — the bound `Runtime::dist_connect` drives the handshake under (`runtime.rs:3593`).

## The outbound path, end to end (verified against the code; `file:line` within this node unless stated)
There are **two** outbound producers, and they do not share a queue.

**(a) A routed `DistSignal` (link/unlink/monitor/demonitor/exit/exit2/monitor-exit) — queued, then flushed.**
1. `ProcessTable` calls `DistSignalRouter::route` (`client.rs`-independent seam: `beam_process/src/remote.rs:460-467`) → `DistRouter::route` (`router.rs:49-55`), which `try_borrow_mut`s the shared cell (`router.rs:50-53`) and calls `DistClientInner::queue_signal`.
   **Nothing is written to a socket here**; a cell already borrowed (a pump/dispatch in flight) is the typed `distribution: the client is already in use`, not a wait.
2. `queue_signal` (`client.rs:946-955`) gates on `DistLinks::may_reach_node` (`../dist/links.rs:224`) — an **exact node-atom-term** match over *both* halves — and answers `no_link_error` (`../dist/links.rs:521`) for a node no link names.
   It then calls `encode_signal` and `push_back`s a `Queued { node: Eterm, dop: Dop, args: Vec<Eterm> }` (`client.rs:71-78`) onto `pending: VecDeque<Queued>` (`client.rs:97`).
3. `encode_signal` (`client.rs:971-1079`) runs **synchronously, at route time**, and writes the signal's terms on the stable `scratch: Heap` (`SCRATCH_WORDS = 1<<12`, `client.rs:59`) so a dying process's heap may be dropped the moment `route` returns.
   It captures `our_node`/`creation`/`next_unlink_id` first (`client.rs:972-974`) and **bumps `next_unlink_id` for every encoded signal, not only `Unlink`** (`client.rs:975` — harmless: the peer only needs a non-zero id unique to the unlink).
4. `DistClient::pump` (`client.rs:391-396`) = `step_links()` **then** `flush_pending(atoms)`; `dispatch` is **not** part of `pump` (see (b)).
   This is the pass-driven flush; the **second** entry to `flush_pending` is each message send's drain (see (b)'s ordering rule).
   `flush_pending` (`client.rs:830-868`) `mem::take`s the queue and per item: `names_node` false ⇒ **drop-and-record** the typed reason in `last_error` (`client.rs:838-845`; the only honest place, since the router already answered `Ok`); not yet established ⇒ push to `retry` and stay queued (`client.rs:846-851`); established ⇒ `DistLinks::route_control` (`client.rs:852-861`, and an encode/write failure is also recorded, not raised).
   `self.pending = retry`, and `scratch.reset()` runs **exactly when the queue empties** (`client.rs:863-866`), so no queued term can dangle — and a queue parked behind a handshaking link keeps consuming the fixed scratch heap until it runs out (typed allocation error, never a silent drop).
5. `DistLinks::route_control` (`../dist/links.rs:318-333`) looks the **outbound** half up first (`:325-327`) then the **accepted** half (`:328-331`, borrowing the shared `Rc<RefCell<Vec<DistConnection>>>`), so a peer that dialled *us* is served identically.

**(b) A message / alias send — written inline, after draining the control queue.**
`DistClient::send_reg_send` (`client.rs:587`, `Dop::RegSend`, args `[local, "" , to_name, msg]`), `send_message_pid` (`client.rs:616` → `DistLinks::route_message`, `../dist/links.rs:345-361`) and `send_alias` (`client.rs:651` → `DistLinks::route_alias`, `../dist/links.rs:391-422`, with the per-link `DFLAG_ALIAS` gate `../dist/links.rs:408`/`:416`).
Their opcode choice for the send family is `beam_dist`'s: `ConnectionDriver::send_message` picks `Dop::SendSender` when the peer negotiated `DFLAG_SEND_SENDER`, else `Dop::Send` (`beam_dist/src/driver.rs:754-767`).
They are reached only from `Runtime::dist_send_*` (`runtime.rs:3723`/`:3751`/`:3783`), each of which **dials an unconnected peer first** (`runtime.rs:3648-3657`).
**Each of the three first drains the queued controls** (`flush_queued_controls`, `client.rs:693` — one call to `DistClientInner::flush_pending`), so a message can never overtake a control signal created earlier on the same link: ERTS writes a control synchronously when the signal is created, under the link lock (`../erts/emulator/beam/dist.c`'s `erts_dsig_send_*`), while this runtime's router can only queue it (see (a)), so the ordering guarantee is re-established at the message write instead. The drain must come **before** any term the send materialises on `inner.scratch` (`local_pid_term`, `RemotePid::to_eterm`), because `flush_pending` resets that heap when the queue empties; `outbound_payload`'s separate `payloads` arena is unaffected and may run either side.
All seven signal kinds travel through that one `flush_pending`, so the rule covers the whole routed-signal family, not only the kinds a given message happens to follow.
**The `DistSignal` → `Dop` map (complete — the enum has exactly seven variants, `beam_process/src/remote.rs:409-509`)**
| signal | `Dop` | args (all on `scratch`) | site |
|---|---|---|---|
| `Link { from, to }` | `Link` (`DOP_LINK`) | `[local(from), to.to_eterm()]` | `client.rs:894-897` |
| `Unlink { from, to }` | `UnlinkId` (`DOP_UNLINK_ID`) | `[unlink_id_term(id), local(from), to.to_eterm()]` | `client.rs:898-905` |
| `Monitor { watcher, target, reference, kind }` | `MonitorP` | `[local(watcher), target.to_eterm(), monitor_ref_term(reference)]` | `client.rs:906-918` |
| `Demonitor { watcher, target, reference }` | `DemonitorP` | `[local(watcher), target.to_eterm(), monitor_ref_term(reference)]` | `client.rs:919-930` |
| `Exit { from, to, reason }` | `Exit` (`DOP_EXIT`) | `[local(from), to.to_eterm(), copy_struct_simple(reason)]` | `client.rs:931-938` |
| `Exit2 { from, to, reason }` | `Exit2` (`DOP_EXIT2`) | `[local(from), to.to_eterm(), copy_struct_simple(reason)]` | `client.rs:939-946` |
| `MonitorExit { watched, watcher, reference, reason }` | `MonitorPExit` (`DOP_MONITOR_P_EXIT`) | `[local(watched), watcher.to_eterm(), ref, copy_struct_simple(reason)]` | `client.rs:947-982` |
`Monitor`'s `kind: MonitorKind` is destructured with `..` (`client.rs:910`) and **does not reach the wire** (`DOP_MONITOR_P` has no tag field).
`MonitorExit` is the **one** variant whose tuple is *watched*-first — the reverse of the monitor family's `{watcher, watched, ref}` — and therefore the one routed to the node named by the element that is **not** its subject (`watcher.node()`, `signals.rs:49`); the C is `erts_dsig_send_m_exit` (`dist.c:1497-1522`, `TUPLE5` at `:1516`).
Its `ref` is written in the spelling the watcher table recorded (`beam_process::RemoteWatcherRef`): `Remote(r)` → `make_external_ref(heap, r.node(), r.ids(), r.creation())` **verbatim** (`client.rs:964-966`), `Local(m)` → `monitor_ref_term` (`client.rs:969-971`, i.e. `{this node, this creation, canonical ids}`).
`Exit`/`Exit2`/`MonitorExit`'s **`reason`** is copied **verbatim** (`copy_struct_simple`, `client.rs:939`/`:947`/`:982`) and the control encoder installs no local-node identity (`beam_dist/src/control.rs:241`), so a reason carrying an **internal** pid/port/reference fails the whole control encode: `terminate_into` logs and drops that one signal (`beam_process/src/ptab.rs:2500-2507`), never substituting a reason.
What produces the signal is a watched local process's death: `ProcessTable::terminate_into` routes one `DistSignal::MonitorExit` per drained `RemoteWatcher`, best-effort and before the dying heap is released (`beam_process/src/ptab.rs:2493-2508`).
It is emitted **unconditionally in the plain 5-tuple form**: the C's `DFLAG_EXIT_PAYLOAD` preference for `DOP_PAYLOAD_MONITOR_P_EXIT` + a message body (`dist.c:1511-1519`) is deliberately not reproduced, because `advertised_flags` describes what this runtime *understands inbound* (it does handle `DOP_PAYLOAD_MONITOR_P_EXIT`, `../dist/inbound.rs::remote_monitor_down`) rather than what it emits, and the plain form is legal on every peer carrying `DFLAG_DIST_MONITOR` (a **mandatory-set** flag — the C only skips the send when even that is absent, `dist.c:1504-1508`).
No per-link flag gate exists on either half: `DistLinks::route_control` and `ConnectionDriver::send_control` gate only on the link being **established**, never on the opcode.
`Dop::Unlink` (4), `Dop::GroupLeader` (7), `Dop::UnlinkIdAck` (36), `Dop::AltactSigSend` (37) and `Dop::PayloadMonitorPExit` are **never emitted** anywhere in `beam_rt` (verified by grep over `src/`): unlink uses the id form, group-leader is an inbound-only receipt primitive (`ProcessTable::set_group_leader_from_remote`), there is no outbound group-leader signal, and the payload monitor-exit form is the decision above.

**Quiescence account**: `DistClient::has_pending_outbound` (`client.rs:564`, delegating to `DistClientInner::has_pending_outbound` at `client.rs:712-727`) answers `pending` non-empty **or** either half's driver holding unwritten output (`ClientLink::driver.has_output()`, `DistConnection::has_pending_output`), and is consulted by `../runtime/scheduler.rs::is_quiescent` beside `DistClient::has_links`.
A queued signal is work even though `queue_signal` gated it on `may_reach_node`, because the link can be retired **between** queueing and the flush — the item is then dropped at the flush with its typed reason in `last_error`, so the flush must still run.
O(link count), allocation-free, and `true` on an unborrowable cell (doubt ⇒ keep pumping).

**Argument materialisation (`signals.rs`)**
`signal_node` (`signals.rs:40-51`) reads the addressed node term out of the variant (`to.node()` / `target.node()`, and — for the watched-first `MonitorExit` — `watcher.node()`); it is the router's gate key.
`monitor_ref_term` (`signals.rs:73-81`) writes a **`NEWER_REFERENCE_EXT`** external ref (`make_external_ref`) whose three identifier words are **`beam_codec::ref_id_to_numbers(reference.id())`** — the shared `beam_codec::ref_wire` authority for the C's canonical `erts_set_ref_numbers` pack (`erl_bif_unique.h:115-134`), the one mapping the inbound decode also unpacks (`../dist/inbound.rs::wire_monitor_ref_id` via `beam_codec::ref_numbers_to_id`), so our own reference comes back as the *same* id once the decode demotes it.
It is never `MonitorRef::to_eterm`'s in-heap internal form, whose node slot is the M1 `make_small(0)` substitution an ETF encoder cannot render (`signals.rs:58-65`), and never a hand-rolled id layout: any other bit layout would make our own reference come back as a *different* id.
`unlink_id_term` (`signals.rs:97-102`) is a **checked** small integer: `i64::try_from` then `try_make_small`, both mapped to `BeamError::SystemLimit` rather than wrapping (`make_small` is unchecked).

**Bytes on the socket — both halves write immediately, no per-link outbound queue**
`ClientLink::send_control` (`handshake.rs:95-103`) → `ConnectionDriver::send_control` (`beam_dist/src/driver.rs:773`: header + atom cache + `'E'`/`'F'` fragmentation) → `flush_output` (`handshake.rs:123-131`) → `stream.write_all(&bytes)` (`handshake.rs:126`) + `flush()` (`:127`) + `bytes_out +=` (`:128`).
`DistConnection::send_control`/`send_message` is the accepted half's identical pair (`../dist/connection.rs:248-251`/`:259-268` → `flush_output` `:229-237`, `write_all` at `:232`). So `route_control` **writes** for both halves; there is no accepted-side flush to wait for.
**The pump is bounded, not non-blocking.** Only the *listener* is `set_nonblocking(true)` (`../dist/node.rs:117`); a link stream is a blocking `TcpStream` with `set_nodelay(true)` + `set_read_timeout(DIST_READ_SLICE = 25 ms)` (`client.rs:307-311`, `../dist.rs:140`), and `TcpStream::connect` is a blocking connect (the 5 s `DIST_CONNECT_TIMEOUT` bounds `dist_connect`'s drive loop, not the socket call).
Reads treat a timeout/`WouldBlock` as "no data" (`handshake.rs:150-153`, `is_would_block` `:182-184`); **writes have no would-block handling** — a full send buffer blocks the scheduler pass in `write_all`.
`ClientLink::pump` (`handshake.rs:137-175`) does read → `driver.feed` → `driver.step()` loop until `WouldBlock` → `flush_output`; on a driver error it flushes first then returns the error (`:164-170`).

**Link lifecycle / retirement**
`step_links` (`client.rs:660-678`) steps **only the outbound** half; a link that is no longer `is_live()` (`handshake.rs:82-87`) or whose pump errored is `retire`d (`client.rs:681-684`) — removed and its reason recorded in `last_error`, never raised. The retirement also records the peer's `node@host` in the runtime's shared `LostPeers` recorder (`use_lost_peers` installs that cell; `client.rs:728` is the one `note_lost_peer` site) — but **only when the retired link had established** — asked as `was_established()` (the latch [`ClientLink::pump`] sets the moment the driver reports `Established`), **not** `is_established()`: the pump that discovers the death is what moves the driver to `Closed`/`Failed`, so the live state answers `false` for every dying link. A link that never handshook was never a node down, and a graceful `DistClient::close` or `connect_addr`'s replace records nothing. `Runtime::service_lost_dist_peers` drains that recorder into `ProcessTable::node_down` on the next scheduler pass.
A cleanly-closed link records the placeholder `BadArg` (`client.rs:671`).
`dispatch` (`client.rs:787-837`, public `dispatch_inbound` `client.rs:374-381`) is the **inbound** half of the same links: `dispatch_heap.reset()` once per drain (`:788`), then per established outbound link `driver.dispatch_inbound_checked` (`:821-823`), counting `DeliveryOutcome::Delivered` and retiring only the failing link (`:833`).
The accepted links are drained by `DistNode::dispatch_inbound` (`../dist/node.rs:374`).
Scheduler pass order (`../runtime/scheduler.rs:1022-1070`): node monitors → `dispatch_dist_inbound` (server half, `:1035`, fn at `:1312`) → `pump_dist_client` (`:1049`, fn at `:1361`) → `dispatch_dist_client_inbound` (`:1058`, fn at `:1385`) → `fire_due_timers` (`:1070`).

**Payload externalisation** (`outbound_payload`, `client.rs:704-707`) is the one rewrite step of the message family: `PayloadArena::externalize` (`../dist/externalize.rs:208-248`) gates on `holds_local_id`, and for a payload carrying one of *our* internal pids/ports/refs re-encodes it with `Encoder::with_local_node(node_atom, creation)` and decodes the image back onto its own arena, so `Peer ! {reply, self()}` is sendable.
A no-id payload is returned **unchanged, unallocated**.

## Constraints
Every path (`crate::dist_client::X`, `beam_rt::dist_client::X`, `super::X` from a sibling submodule) resolves through the `mod.rs` façade.
Helpers a sibling file calls carry `pub(super)`.
No `unsafe` (the crate is `#![deny(unsafe_code)]`); every fallible API returns `beam_common::Result` and no failure is swallowed.
No item here is re-implemented from the lower crates: terms come from `beam_term`/`beam_codec`, signals from `beam_process`, bytes from `beam_dist`.

## Known Issues
- **A signal routed while the client cell is borrowed is a typed error, not a deferral** (`router.rs:50-53`): `route` cannot queue during a pump/dispatch.
- **`DistClientInner::next_unlink_id` advances per encoded signal** (`client.rs:881`), not per unlink.
- **No pending-unlink record is kept** — the id is minted and forgotten, so an inbound `DOP_UNLINK_ID_ACK` has nothing here to match (the inbound side likewise has no reply channel to emit one).
- **`DistSignal::Monitor`'s `kind` (`MonitorKind`) is dropped** by the `..` at `client.rs:910`: a **port** monitor encodes as `Dop::MonitorP` with no gate and no typed refusal.
- **`step_links`' retirement of a cleanly-closed link records `BadArg`** (`client.rs:671`) — the diagnostic reads as a bad-argument error for an orderly EOF.

## Test Strategy
The colocated suite (`tests.rs`, 18 tests) is hermetic: a loopback socket and a server-side `ConnectionDriver`, no `erl` and no real `epmd`.
It pins the handshake, node visibility (`-hidden`), a decoded `DOP_REG_SEND`, the control-before-message ordering rule (`a_message_never_overtakes_a_queued_control` `tests.rs:261` — a routed signal left queued, then a message send, and the peer decodes the control **first** with no `pump` in between), a routed `Exit` surviving its source heap (`tests.rs:341` — the synchronous-materialisation contract), the monitor argument order (`tests.rs:397`), the `DOP_MONITOR_P_EXIT` argument order + both reference spellings (`a_monitor_exit_names_the_watched_first_and_returns_the_peer_reference_verbatim` `tests.rs:636`, `a_locally_minted_monitor_exit_reference_uses_the_monitor_spelling` `tests.rs:717`), the routing key of every `DistSignal` variant (`every_routed_signal_addresses_the_peer_that_must_receive_it` `tests.rs:762`), a routed `MonitorExit`'s queued→flushed lifecycle + the peer-side decode (`a_queued_monitor_exit_is_pending_until_the_flush_and_decodes_watched_first` `tests.rs:816`, which is also the one case exercising `flush_pending`'s **keep-queued** branch), a signal with no link being typed (`tests.rs:453`), the checked unlink id (`tests.rs:481`, `:504`) and the `epmd` `PORT2_REQ` lookup (`tests.rs:542`, `:590`).
It is run by the crate gate `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib dist_client`.
The end-to-end, oracle-cross-checked evidence is the integration suites `tests/dist_client.rs`, `tests/live_signals.rs` and `tests/alias.rs`.
The compatibility gate `../tools/differential/`'s `wire` group covers the ETF codec; a live handshake needs two peers and is evidence of the `beam_dist` `live_interop` suite instead.

## Routing Table
- `mod.rs` → module docs, `DIST_CONNECT_TIMEOUT`, `mod` declarations and the four re-exports.
- `client.rs` → `DistClient`, `DistClientInner`, `Queued`, `SCRATCH_WORDS`/`DISPATCH_HEAP_WORDS`, `step_links`, `retire`, `outbound_payload`, `local_pid_term`, `flush_pending`, `flush_queued_controls` (the message path's control-queue drain — the ordering rule), `dispatch`, `queue_signal`, `encode_signal`, `share_links_with`.
- `handshake.rs` → `ClientLink` (`node`/`name` `pub(crate)`, `driver`/`stream`/`was_established`/`bytes_in`/`bytes_out` `pub(super)`), `is_established`, `was_established`, `negotiated`, `is_live`, `send_control`, `send_message`, `flush_output`, `pump`, `is_would_block`.
- `router.rs` → `DistRouter` and its `impl DistSignalRouter`.
- `signals.rs` → `signal_node`, `monitor_ref_term`, `unlink_id_term` (all `pub(super)`).
- `lookup.rs` → `host_part`, `lookup_dist_port`, `lookup_dist_port_at` (all `pub(super)`).
- `tests.rs` → the colocated suite (handshake against a server driver, node visibility, the outbound message and routed-signal path, the `epmd` lookup).