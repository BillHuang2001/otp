# `beam_rt::dist_client` — the connecting (client) half of distribution (contract A.5, phase 2)
## Intent
A registry of *outbound* distribution links this runtime dials, the routing of the process layer's outbound link/monitor/exit signals onto them, and the sending of messages to a peer.
[`crate::dist`](../dist/CONTEXT.md) is the listening half; this node is the mirror — it dials a peer resolved through `epmd`, drives the client side of the same `'N'` handshake, and carries a two-way `DOP_*` session on the resulting link.
It is wiring, not protocol code: every byte of the wire lives in the sibling `beam_dist` crate (`ConnectionDriver::client`, `send_control`, `beam_dist::epmd::lookup_port_at`, `ConnectionDriver::dispatch_inbound_checked`).
What this node adds is the *driving state* the runtime owns: a socket per peer, a bounded pump, a failure-recorded retirement policy and the [`DistSignalRouter`] the process table installs.
## API Surface (the frozen façade — `mod.rs`)
`pub use client::DistClient;` — the registry the runtime holds and calls (`connect`, `pump`, `send`, `last_error`, `is_established`, `connected_nodes`, `has_links`, `has_pending_outbound`, …).
`pub(crate) use client::DistClientInner;` — the shared state every half borrows (the links cell, the outbound queue, the three scratch heaps); crate-visible because a sibling's rustdoc names it.
`pub(crate) use handshake::ClientLink;` — one outbound link; crate-visible because the one node→link table (`../dist/links.rs`) shares it with the accepting half.
`pub(crate) use router::DistRouter;` — the `beam_process::DistSignalRouter` implementation installed on the runtime's process table.
`pub const DIST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);` — the bound `Runtime::dist_connect` drives the handshake under (`runtime.rs:3593`).

## The outbound path, end to end (verified against the code; `file:line` within this node unless stated)
There are **two** outbound producers, and they do not share a queue.

**(a) A routed `DistSignal` (link/unlink/monitor/demonitor/exit/exit2) — queued, then flushed.**
1. `ProcessTable` calls `DistSignalRouter::route` (`client.rs`-independent seam: `beam_process/src/remote.rs:460-467`) → `DistRouter::route` (`router.rs:49-55`), which `try_borrow_mut`s the shared cell (`router.rs:50-53`) and calls `DistClientInner::queue_signal`. **Nothing is written to a socket here**; a cell already borrowed (a pump/dispatch in flight) is the typed `distribution: the client is already in use`, not a wait.
2. `queue_signal` (`client.rs:749-758`) gates on `DistLinks::may_reach_node` (`../dist/links.rs:224`) — an **exact node-atom-term** match over *both* halves — and answers `no_link_error` (`../dist/links.rs:521`) for a node no link names. It then calls `encode_signal` and `push_back`s a `Queued { node: Eterm, dop: Dop, args: Vec<Eterm> }` (`client.rs:68-75`) onto `pending: VecDeque<Queued>` (`client.rs:94`).
3. `encode_signal` (`client.rs:762-833`) runs **synchronously, at route time**, and writes the signal's terms on the stable `scratch: Heap` (`SCRATCH_WORDS = 1<<12`, `client.rs:59`) so a dying process's heap may be dropped the moment `route` returns. It captures `our_node`/`creation`/`next_unlink_id` first (`client.rs:763-766`) and **bumps `next_unlink_id` for every encoded signal, not only `Unlink`** (`client.rs:766` — harmless: the peer only needs a non-zero id unique to the unlink).
4. `DistClient::pump` (`client.rs:323-328`) = `step_links()` **then** `flush_pending(atoms)`; `dispatch` is **not** part of `pump` (see (b)). `flush_pending` (`client.rs:654-692`) `mem::take`s the queue and per item: `names_node` false ⇒ **drop-and-record** the typed reason in `last_error` (`client.rs:662-669`; the only honest place, since the router already answered `Ok`); not yet established ⇒ push to `retry` and stay queued (`client.rs:670-675`); established ⇒ `DistLinks::route_control` (`client.rs:676-686`, and an encode/write failure is also recorded, not raised). `self.pending = retry`, and `scratch.reset()` runs **exactly when the queue empties** (`client.rs:687-690`), so no queued term can dangle — and a queue parked behind a handshaking link keeps consuming the fixed scratch heap until it runs out (typed allocation error, never a silent drop).
5. `DistLinks::route_control` (`../dist/links.rs:318-333`) looks the **outbound** half up first (`:325-327`) then the **accepted** half (`:328-331`, borrowing the shared `Rc<RefCell<Vec<DistConnection>>>`), so a peer that dialled *us* is served identically.

**(b) A message / alias send — never queued, written inline.**
`DistClient::send_reg_send` (`client.rs:493-510`, `Dop::RegSend`, args `[local, "" , to_name, msg]`), `send_message_pid` (`client.rs:519-537` → `DistLinks::route_message`, `../dist/links.rs:345-361`) and `send_alias` (`client.rs:551-562` → `DistLinks::route_alias`, `../dist/links.rs:391-422`, with the per-link `DFLAG_ALIAS` gate `../dist/links.rs:408`/`:416`). Their opcode choice for the send family is `beam_dist`'s: `ConnectionDriver::send_message` picks `Dop::SendSender` when the peer negotiated `DFLAG_SEND_SENDER`, else `Dop::Send` (`beam_dist/src/driver.rs:754-767`). They are reached only from `Runtime::dist_send_*` (`runtime.rs:3723`/`:3751`/`:3783`), each of which **dials an unconnected peer first** (`runtime.rs:3648-3657`).

**The `DistSignal` → `Dop` map (complete — the enum has exactly seven variants, `beam_process/src/remote.rs:409-509`)**
| signal | `Dop` | args (all on `scratch`) | site |
|---|---|---|---|
| `Link { from, to }` | `Link` (`DOP_LINK`) | `[local(from), to.to_eterm()]` | `client.rs:779-782` |
| `Unlink { from, to }` | `UnlinkId` (`DOP_UNLINK_ID`) | `[unlink_id_term(id), local(from), to.to_eterm()]` | `client.rs:783-790` |
| `Monitor { watcher, target, reference, kind }` | `MonitorP` | `[local(watcher), target.to_eterm(), monitor_ref_term(reference)]` | `client.rs:791-803` |
| `Demonitor { watcher, target, reference }` | `DemonitorP` | `[local(watcher), target.to_eterm(), monitor_ref_term(reference)]` | `client.rs:804-815` |
| `Exit { from, to, reason }` | `Exit` (`DOP_EXIT`) | `[local(from), to.to_eterm(), copy_struct_simple(reason)]` | `client.rs:816-823` |
| `Exit2 { from, to, reason }` | `Exit2` | `[local(from), to.to_eterm(), copy_struct_simple(reason)]` | `client.rs:824-831` |
| `MonitorExit { watched, watcher, reference, reason }` | `MonitorPExit` (`DOP_MONITOR_P_EXIT`) | `[local(watched), watcher.to_eterm(), ref, copy_struct_simple(reason)]` | `client.rs:832-881` |
`Monitor`'s `kind: MonitorKind` is destructured with `..` (`client.rs:795-796`) and **does not reach the wire** (`DOP_MONITOR_P` has no tag field).
`MonitorExit` is the **one** variant whose tuple is *watched*-first — the reverse of the monitor family's `{watcher, watched, ref}` — and therefore the one routed to the node named by the element that is **not** its subject (`watcher.node()`); the C is `erts_dsig_send_m_exit` (`dist.c:1499-1522`, `TUPLE5` at `:1516`). Its `ref` is written in the spelling the watcher table recorded (`beam_process::RemoteWatcherRef`): `Remote(r)` → `make_external_ref(heap, r.node(), r.ids(), r.creation())` **verbatim**, `Local(m)` → `monitor_ref_term` (`{this node, this creation, ids}`).
It is emitted **unconditionally in the plain 5-tuple form**: the C's `DFLAG_EXIT_PAYLOAD` preference for `DOP_PAYLOAD_MONITOR_P_EXIT` + a message body (`dist.c:1511-1519`) is not reproduced, because `advertised_flags` describes what this runtime *understands inbound* (it does handle `DOP_PAYLOAD_MONITOR_P_EXIT`, `../dist/inbound.rs::remote_monitor_down`) rather than what it emits, and the plain form is legal on every peer carrying `DFLAG_DIST_MONITOR` (mandatory set — the C only skips the send when even that is absent, `dist.c:1504-1508`). No per-link flag gate exists on either half: `DistLinks::route_control` and `ConnectionDriver::send_control` gate only on the link being **established**, never on the opcode.
`Dop::Unlink` (4), `Dop::GroupLeader` (7), `Dop::UnlinkIdAck` (36) and `Dop::AltactSigSend` (37) are **never emitted** anywhere in `beam_rt` (verified by grep over `src/`): unlink uses the id form, group-leader is an inbound-only receipt primitive (`ProcessTable::set_group_leader_from_remote`), and there is no outbound group-leader signal.

**Quiescence account**: `DistClient::has_pending_outbound` (`client.rs`, delegating to `DistClientInner::has_pending_outbound`) answers `pending` non-empty **or** either half's driver holding unwritten output (`ClientLink::driver.has_output()`, `DistConnection::has_pending_output`), and is consulted by `../runtime/scheduler.rs::is_quiescent` beside `DistClient::has_links`. A queued signal is work even though `queue_signal` gated it on `may_reach_node`, because the link can be retired **between** queueing and the flush — the item is then dropped at the flush with its typed reason in `last_error`, so the flush must still run. O(link count), allocation-free, and `true` on an unborrowable cell (doubt ⇒ keep pumping).

**Argument materialisation (`signals.rs`)**
`signal_node` (`signals.rs:34-50`) reads the addressed node term out of the variant (`to.node()` / `target.node()`, and — for the watched-first `MonitorExit` — `watcher.node()`); it is the router's gate key.
`monitor_ref_term` (`signals.rs:57-66`) writes a **`NEWER_REFERENCE_EXT`** external ref (`make_external_ref`, ids `[0, id>>32, id]`, this node + creation) — never `MonitorRef::to_eterm`'s in-heap internal form, whose node slot is the M1 `make_small(0)` substitution an ETF encoder cannot render (`signals.rs:44-56`).
`unlink_id_term` (`signals.rs:82-87`) is a **checked** small integer: `i64::try_from` then `try_make_small`, both mapped to `BeamError::SystemLimit` rather than wrapping (`make_small` is unchecked).

**Bytes on the socket — both halves write immediately, no per-link outbound queue**
`ClientLink::send_control` (`handshake.rs:95-103`) → `ConnectionDriver::send_control` (`beam_dist/src/driver.rs:719-747`: header + atom cache + `'E'`/`'F'` fragmentation) → `flush_output` (`handshake.rs:123-131`) → `stream.write_all(&bytes)` (`handshake.rs:126`) + `flush()` (`:127`) + `bytes_out +=` (`:128`).
`DistConnection::send_control`/`send_message` is the accepted half's identical pair (`../dist/connection.rs:226-229`/`:237-246` → `flush_output` `:207-215`, `write_all` at `:210`). So `route_control` **writes** for both halves; there is no accepted-side flush to wait for.
**The pump is bounded, not non-blocking.** Only the *listener* is `set_nonblocking(true)` (`../dist/node.rs:117`); a link stream is a blocking `TcpStream` with `set_nodelay(true)` + `set_read_timeout(DIST_READ_SLICE = 25 ms)` (`client.rs:275-279`, `../dist.rs:140`), and `TcpStream::connect` is a blocking connect (the 5 s `DIST_CONNECT_TIMEOUT` bounds `dist_connect`'s drive loop, not the socket call). Reads treat a timeout/`WouldBlock` as "no data" (`handshake.rs:150-153`, `is_would_block` `:182-184`); **writes have no would-block handling** — a full send buffer blocks the scheduler pass in `write_all`.
`ClientLink::pump` (`handshake.rs:137-175`) does read → `driver.feed` → `driver.step()` loop until `WouldBlock` → `flush_output`; on a driver error it flushes first then returns the error (`:164-170`).

**Link lifecycle / retirement**
`step_links` (`client.rs:578-596`) steps **only the outbound** half; a link that is no longer `is_live()` (`handshake.rs:82-87`) or whose pump errored is `retire`d (`client.rs:599-602`) — removed and its reason recorded in `last_error`, never raised. A cleanly-closed link records the placeholder `BadArg` (`client.rs:589`).
`dispatch` (`client.rs:705-734`, public `dispatch_inbound` `client.rs:342-349`) is the **inbound** half of the same links: `dispatch_heap.reset()` once per drain (`:706`), then per established outbound link `driver.dispatch_inbound_checked` (`:718-721`), counting `DeliveryOutcome::Delivered` and retiring only the failing link (`:730`). The accepted links are drained by `DistNode::dispatch_inbound` (`../dist/node.rs:362`).
Scheduler pass order (`../runtime/scheduler.rs:1002-1068`): node monitors → `dispatch_dist_inbound` (server half, `:1024`) → `pump_dist_client` (`:1038`, `:1310-1315`) → `dispatch_dist_client_inbound` (`:1047`, `:1334-1367`) → `fire_due_timers`.

**Payload externalisation** (`outbound_payload`, `client.rs:622-625`) is the one rewrite step of the message family: `PayloadArena::externalize` (`../dist/externalize.rs:208-248`) gates on `holds_local_id`, and for a payload carrying one of *our* internal pids/ports/refs re-encodes it with `Encoder::with_local_node(node_atom, creation)` and decodes the image back onto its own arena, so `Peer ! {reply, self()}` is sendable. A no-id payload is returned **unchanged, unallocated**.

## Constraints
Every path (`crate::dist_client::X`, `beam_rt::dist_client::X`, `super::X` from a sibling submodule) resolves through the `mod.rs` façade.
Helpers a sibling file calls carry `pub(super)`.
No `unsafe` (the crate is `#![deny(unsafe_code)]`); every fallible API returns `beam_common::Result` and no failure is swallowed.
No item here is re-implemented from the lower crates: terms come from `beam_term`/`beam_codec`, signals from `beam_process`, bytes from `beam_dist`.

## Known Issues
- **`mod.rs`'s "Correction to a sibling's inline gloss" paragraph (`mod.rs:109-119`) is stale.** `beam_process/src/remote.rs:53-58` now states exactly these argument orders (the wrong `&[to_term, watcher.to_eterm(), ref_term]` / `&[from, to]` table was corrected in commit `645b2b6992`), so the paragraph describes a disagreement that no longer exists. Drop it; do not act on it.
- **A signal routed while the client cell is borrowed is a typed error, not a deferral** (`router.rs:50-53`): `route` cannot queue during a pump/dispatch.
- **`DistClientInner::next_unlink_id` advances per encoded signal** (`client.rs:766`), not per unlink.
- **No pending-unlink record is kept** — the id is minted and forgotten, so an inbound `DOP_UNLINK_ID_ACK` has nothing here to match (the inbound side likewise has no reply channel to emit one).
- **`DistSignal::Monitor`'s `kind` (`MonitorKind`) is dropped** by the `..` at `client.rs:795`: a **port** monitor encodes as `Dop::MonitorP` with no gate and no typed refusal.- **`step_links`' retirement of a cleanly-closed link records `BadArg`** (`client.rs:589`) — the diagnostic reads as a bad-argument error for an orderly EOF.
- **No test in this node's suite covers `flush_pending`'s two branches** (still-handshaking ⇒ keep queued; link retired ⇒ drop-and-record). The drop branch is exercised from the sibling suite `../dist/links/tests.rs:376`; the keep-queued branch is unexercised anywhere.

## Test Strategy
The colocated suite (`tests.rs`, 18 tests) is hermetic: a loopback socket and a server-side `ConnectionDriver`, no `erl` and no real `epmd`. It pins the handshake, node visibility (`-hidden`), a decoded `DOP_REG_SEND`, a routed `Exit` surviving its source heap (`tests.rs:252` — the synchronous-materialisation contract), the monitor argument order (`tests.rs:308`), the `DOP_MONITOR_P_EXIT` argument order + both reference spellings (`a_monitor_exit_names_the_watched_first_and_returns_the_peer_reference_verbatim`, `a_locally_minted_monitor_exit_reference_uses_the_monitor_spelling`), the routing key of every `DistSignal` variant (`every_routed_signal_addresses_the_peer_that_must_receive_it`), a routed `MonitorExit`'s queued→flushed lifecycle + the peer-side decode (`a_queued_monitor_exit_is_pending_until_the_flush_and_decodes_watched_first`), a signal with no link being typed (`tests.rs:364`), the checked unlink id (`tests.rs:392`, `:415`) and the `epmd` `PORT2_REQ` lookup (`tests.rs:453`, `:501`).
It is run by the crate gate `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib dist_client`.
The end-to-end, oracle-cross-checked evidence is the integration suites `tests/dist_client.rs`, `tests/live_signals.rs` and `tests/alias.rs`.
The compatibility gate `../tools/differential/`'s `wire` group covers the ETF codec; a live handshake needs two peers and is evidence of the `beam_dist` `live_interop` suite instead.

## Routing Table
- `mod.rs` → module docs, `DIST_CONNECT_TIMEOUT`, `mod` declarations and the four re-exports.
- `client.rs` → `DistClient`, `DistClientInner`, `Queued`, `SCRATCH_WORDS`/`DISPATCH_HEAP_WORDS`, `step_links`, `retire`, `outbound_payload`, `local_pid_term`, `flush_pending`, `dispatch`, `queue_signal`, `encode_signal`, `share_links_with`.
- `handshake.rs` → `ClientLink` (`node`/`name` `pub(crate)`, `driver`/`stream`/`bytes_in`/`bytes_out` `pub(super)`), `is_established`, `negotiated`, `is_live`, `send_control`, `send_message`, `flush_output`, `pump`, `is_would_block`.
- `router.rs` → `DistRouter` and its `impl DistSignalRouter`.
- `signals.rs` → `signal_node`, `monitor_ref_term`, `unlink_id_term` (all `pub(super)`).
- `lookup.rs` → `host_part`, `lookup_dist_port`, `lookup_dist_port_at` (all `pub(super)`).
- `tests.rs` → the colocated suite (handshake against a server driver, node visibility, the outbound message and routed-signal path, the `epmd` lookup).