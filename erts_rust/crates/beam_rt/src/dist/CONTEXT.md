# `beam_rt` — distribution, accepting half (`src/dist.rs` + `src/dist/`)

## Intent
The accepting half of the distributed node (contract **A.5**, phases 1–2), split in two by responsibility.
`src/dist.rs` is the module root and **node half**: the module docs, the `epmd` registration, the bound
listener and the per-link `'N'` handshake/drain.
`src/dist/inbound.rs` is the **inbound delivery sink**: the runtime side of `beam_dist`'s `InboundSink`
seam, which routes a classified inbound `Delivery` to the addressed local process.
The connecting half is the sibling `src/dist_client.rs` (not part of this node).
This directory holds only the sink submodule and this doc; the node half is the parent file `src/dist.rs`.

## API Surface
- `dist.rs` (module `crate::dist`, FROZEN façade): `NodeName` (+ `local_hostname`), `DistConnection`,
  `DistNode`, `resolve_cookie`, and the consts `NONODE_NAME`, `DIST_READ_SLICE`, `DIST_LISTEN_ADDR`
  (also `MAX_ACCEPTS_PER_PUMP`/`DIST_DISPATCH_HEAP_WORDS`).
- `dist/inbound.rs`: `LocalDeliverySink` — the `impl InboundSink` sink plus its private `Target` enum and
  routing helpers (`envelope_from`, `receipt`, `remote_pid`, `small_uint`).
  **Path identity is mandatory**: it is re-exported from the parent as
  `pub use inbound::LocalDeliverySink;`, so `crate::dist::LocalDeliverySink` (and
  `beam_rt::dist::LocalDeliverySink`) resolve exactly as before the split.

## The two halves, one line each
- `dist.rs` — bind a wildcard listener, register with `epmd` (best-effort), and drive each accepted link
  through `beam_dist::ConnectionDriver` (server `'N'` → `'s'`/`'S'` → `'r'` → `'a'`), then per-link
  checked drain (`dispatch_inbound_checked`) handing each classified `Delivery` to the sink.
- `dist/inbound.rs` — the `InboundSink` sink: route the message family and the route-free receipt signal
  families into the local process table, report the genuinely unrouted families as the honest
  `DeliveryOutcome::Unsupported`.

## Constraints
- Errors are return-based (`beam_common::Result`); a link-level failure **retires that one link** and is
  recorded, never raised — it must never fail a scheduler pass.
- `unsafe`-free (the crate is `#![deny(unsafe_code)]`).
- Never deliver to the wrong process: an addressed party not local to *this* node is `NoSuchProcess`, a
  party outside its opcode's wire contract is `Unsupported`, never a silent mis-delivery.
- Do not hand-edit generated artifacts; Apache-2.0 header block required on every `.rs` file.
- Keep each file ~1000 lines; `dist.rs` is ~796, `dist/inbound.rs` is ~865.

## Missing sibling APIs (the still-`Unsupported` families)
- **The distributed-spawn family** (`SpawnRequest`/`SpawnReply`) needs a `beam_process` spawn-responder/reservation API — a `DOP_SPAWN_REPLY` sender plus the reservation bookkeeping a `DOP_SPAWN_REQUEST` keys into.
- **`Delivery::Other(_)`** is the link driver's (the C kills the connection outright, `dist.c:2277-2282`).
- **`beam_dist` still needs three things**: a reply channel on `InboundSink` (the `DOP_EXIT`/`DOP_MONITOR_P_EXIT` `noproc` replies and the `DOP_UNLINK_ID_ACK` cannot be emitted at all — the drain counts only `Delivered`); a connection/peer identity on the sink (the C's arms need the peer's sysname for the envelope and the connection id for the origin checks); and a local-node identity in `decode_control` (inbound references decode as pure external, so a wire reference cannot key an alias or a monitor DOWN).
- **Routing an inbound creation family through the nearest public `beam_process` entry point would be WRONG today**: those entry points are the *local-initiated outbound* half and route a `DOP_*` through the installed `DistSignalRouter`, whose auto-echo the C's inbound receipt arms never send (`dist.c:2258` `break; /* Done */`).
- `beam_process`'s route-free receipt primitives (all present, none echoing a `DOP_*`): `record_remote_link`, `apply_remote_exit_signal`, `unlink_from_remote`/`unlink_ack_from_remote`, `monitor_from_remote_ref`/`demonitor_from_remote_ref` (the peer-minted wire-reference spellings; the `MonitorRef` ones stay for an internal ref), `remote_monitor_down`, `set_group_leader_from_remote` — the per-family routing table is [`../runtime/CONTEXT.md`](../runtime/CONTEXT.md)'s.

## Routing Table
- `dist.rs` (parent) → the node half: `DistNode`/`DistConnection`/`NodeName`/`resolve_cookie`/`NONODE_NAME`; declares `mod inbound;` and re-exports `LocalDeliverySink`.
- `dist/inbound.rs` → the inbound delivery sink over `beam_dist::InboundSink` (contract A.5): `LocalDeliverySink` + routing helpers.

## See Also
- `../dist_client.rs` → the connecting half (outbound client handshake + the `DistSignalRouter`).
- `../CONTEXT.md` → the crate façade and the A.5 status; `../runtime/CONTEXT.md` → the per-pass distribution drive.
