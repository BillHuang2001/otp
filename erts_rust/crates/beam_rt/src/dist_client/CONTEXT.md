# `beam_rt::dist_client` — the connecting (client) half of distribution (contract A.5, phase 2)
## Intent
A registry of *outbound* distribution links this runtime dials, the routing of the process layer's outbound link/monitor/exit signals onto them, and the sending of messages to a peer.
[`crate::dist`](../dist/CONTEXT.md) is the listening half; this node is the mirror — it dials a peer resolved through `epmd`, drives the client side of the same `'N'` handshake, and carries a two-way `DOP_*` session on the resulting link.
It is wiring, not protocol code: every byte of the wire lives in the sibling `beam_dist` crate (`ConnectionDriver::client`, `send_control`, `beam_dist::epmd::lookup_port_at`, `ConnectionDriver::dispatch_inbound_checked`).
What this node adds is the *driving state* the runtime owns: a socket per peer, a bounded non-blocking pump, a failure-recorded retirement policy and the [`DistSignalRouter`] the process table installs.
## API Surface (the frozen façade — `mod.rs`)
`pub use client::DistClient;` — the registry the runtime holds and calls (`connect`, `pump`, `send`, `last_error`, `is_established`, `connected_nodes`, …).
`pub(crate) use client::DistClientInner;` — the shared state every half borrows (the links cell, the outbound queue, the three scratch heaps); crate-visible because a sibling's rustdoc names it.
`pub(crate) use handshake::ClientLink;` — one outbound link; crate-visible because the one node→link table (`../dist/links.rs`) shares it with the accepting half.
`pub(crate) use router::DistRouter;` — the `beam_process::DistSignalRouter` implementation installed on the runtime's process table.
`pub const DIST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);` — the bound `Runtime::dist_connect` drives the handshake under.
## Constraints
Every path (`crate::dist_client::X`, `beam_rt::dist_client::X`, `super::X` from a sibling submodule) resolves through the `mod.rs` façade.
Helpers a sibling file calls carry `pub(super)`.
No `unsafe` (the crate is `#![deny(unsafe_code)]`); every fallible API returns `beam_common::Result` and no failure is swallowed.
No item here is re-implemented from the lower crates: terms come from `beam_term`/`beam_codec`, signals from `beam_process`, bytes from `beam_dist`.
# File layout
`mod.rs` — the module docs (the handshake order, the `DOP_*` argument order, the honest limits) and the frozen façade above.
`client.rs` — `DistClient` and `DistClientInner` together: the registry's public API, the outbound queue's drain/flush and the per-link inbound dispatch.
`handshake.rs` — `ClientLink`: the connecting-side state machine over its own socket and the non-blocking read/step/write round `pump` drives.
`router.rs` — `DistRouter`: the `DistSignalRouter` implementation that only *materialises* a signal into the client's queue.
`signals.rs` — the `DOP_*` argument materialisation of a routed `DistSignal` (the external pid/reference builders and the checked `DOP_UNLINK_ID` id).
`lookup.rs` — the `epmd` `PORT2_REQ` port lookup and the `node@host` name helpers.
`tests.rs` — the colocated `#[cfg(test)]` suite.
## Routing Table
- `mod.rs` → module docs, `DIST_CONNECT_TIMEOUT`, `mod` declarations and the four re-exports.
- `client.rs` → `DistClient`, `DistClientInner`, `Queued`, `SCRATCH_WORDS`, `DISPATCH_HEAP_WORDS`, `step_links`, `retire`, `outbound_payload`, `local_pid_term`, `flush_pending`, `dispatch`, `queue_signal`, `encode_signal`.
- `handshake.rs` → `ClientLink` (`node`/`name` `pub(crate)`, `driver`/`stream`/`bytes_in`/`bytes_out` `pub(super)`), `is_established`, `negotiated`, `is_live`, `send_control`, `send_message`, `flush_output`, `pump`, `is_would_block`.
- `router.rs` → `DistRouter` and its `impl DistSignalRouter`.
- `signals.rs` → `signal_node`, `monitor_ref_term`, `unlink_id_term` (all `pub(super)`).
- `lookup.rs` → `host_part`, `lookup_dist_port`, `lookup_dist_port_at` (all `pub(super)`).
- `tests.rs` → the colocated suite (handshake against a server driver, node visibility, the outbound message and routed-signal path, the `epmd` lookup).
## Test Strategy
The colocated suite (`tests.rs`, 11 tests) is hermetic: a loopback socket and a server-side `ConnectionDriver`, no `erl` and no real `epmd`.
It is run by the crate gate `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib dist_client`.
The end-to-end, oracle-cross-checked evidence is the integration suites `tests/dist_client.rs`, `tests/live_signals.rs` and `tests/alias.rs`.
The compatibility gate `../tools/differential/`'s `wire` group covers the ETF codec; a live handshake needs two peers and is evidence of the `beam_dist` `live_interop` suite instead.
