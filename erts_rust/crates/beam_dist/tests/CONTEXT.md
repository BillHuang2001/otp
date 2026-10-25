# `beam_dist` — integration suites (`tests/`)

## Intent
The crate's integration gates: what each suite **proves** about contract A.5 (distribution/ETF wire) beyond the per-module unit tests.
Two families: **hermetic** suites (no `erl`, no `epmd` — two in-process endpoints over a real loopback socket, or `MemoryTransport`) and **live** suites against a real `erl` node.
This node owns the claimed proof per suite; the crate's own `../CONTEXT.md` owns the API surface, the wire facts and the known gaps.

## Test Strategy
- Run: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_dist` (all suites), or `--test <name>` for one.
- Every live suite **skips honestly** (clear `eprintln`, never a faked pass) when `erl`/`epmd` is absent from `PATH`; hermetic suites never skip.
- A suite's claim is a behavior claim, not a walkthrough: keep the list below in sync when a suite's coverage changes, and do not pin a pass/fail count here (it goes stale).
- Do **not** run `cargo fmt` on this crate — it is intentionally not default-rustfmt-clean, and a bulk pass rewrites hundreds of unrelated lines.
- The shared spawn harness for the live suites is `common/mod.rs` (`ErlangNode`, `erl_hostname`, `free_port`, `resolve_listen_port`).

## Notes for Agents
- `erl` can be pointed at a private `epmd` with `erl -epmd_port <N>` (`erl_epmd:get_epmd_port/0` reads `init:get_argument(epmd_port)`; the client connects to `127.0.0.1`) — that is how `hermetic_epmd.rs` validates the in-process stub without a host daemon.
- In a live probe a read timeout is **not** a peer rejection: `DEFAULT_READ_TIMEOUT` surfaces as `Io(WouldBlock, errno 11)` — the link is still open and nothing arrived.
- A safe external-term builder exists for tests: `beam_codec::term::{make_external_pid, make_external_port, make_external_ref}`.
- The hermetic suites place small-int stand-ins in pid/ref slots, which compare `==` across heaps.

## Routing Table
- `common/mod.rs` → the shared live-node spawn harness (`ErlangNode`, `erl_hostname`, `free_port`, `resolve_listen_port`) — used by every `live_*` suite
- `constants.rs` → exact-value pins for every `DFLAG_*`/`DOP_*`, the in-term flag vocabularies (incl. the shared bit 0 in distinct namespaces), the handshake/epmd tags
- `auth.rs` → RFC 1321 KATs through the workspace's single `beam_common::md5`, plus `gen_digest` known answers derived from the oracle (`erlang:md5/1` of `<<Cookie, Challenge>>`)
- `framing.rs` → 2-/4-byte framing round-trips, the zero-length tick, over-ceiling `SystemLimit`, the EOF-aware `read_*_or_eof` forms
- `handshake.rs` → the six codecs, `Status::kind`/`error_kind`/`status_error`, the three refusal texts (`snok`/`snot_allowed`/`salive`), `AcceptDecision`
- `control.rs` → wire tuple bytes (`DOP_SEND` args `[1,2]` ⇒ `[104,3,97,2,97,1,97,2]`), `encode_control_cached` byte-identical with no cache and `[82, index]` with one, arity negatives
- `dop_roundtrip.rs` → the exhaustive 30/30 round-trip (a wildcard-free `match` arg table) + 30 oracle-cross-checked golden vectors
- `delivery.rs` → `classify` over every opcode (arg-order pins, flag decoding incl. the out-of-range `BadArg`, `DOP_UNLINK` ⇒ `Other`), the validator's per-opcode reference guards built from heap terms (the only way to hand it an internal reference), and the per-opcode unlink-id rule (zero id = `BadArg` for `DOP_UNLINK_ID`, accepted for `DOP_UNLINK_ID_ACK`)
- `monitor_exit_vectors.rs` → hand-written golden vectors independent of the encoder: `DOP_MONITOR_P_EXIT`'s `watched`-first 5-tuple (opcode 21), `DOP_PAYLOAD_MONITOR_P_EXIT`'s 4-tuple (28) + trailing reason, the deliberately mirrored reverse-order `DOP_MONITOR_P`, the arity rules as `BadArg`, each vector's `classify_validated` view
- `validated_delivery.rs` → the checked entry points over a real wire with real external pids/refs (a peer `DOP_LINK` validates, a third-node `from` is `Err`, a small-int monitor watcher is `Err`); the driver is fed the **byte-identical** frame twice so accept/reject differ in no byte; the `DOP_UNLINK_ID`/`_ACK` legs prove the id rule (not the endpoints) and that `take_output()` stays empty — this crate writes no unlink acknowledgement
- `local_identity.rs` → the decode-side identity seam hermetically: this-node pids/ports/refs demote on every wire tag and in every send-family payload, foreign node/creation/unrepresentable-id forms stay external, the C's identity-gated bounds engage, the demoted reference then passes `classify` **and** `classify_validated`; the identity-less legs prove additivity; two end-to-end legs drive a real `Connection` and a byte-fed `ConnectionDriver` through install → decode → clear
- `fragment.rs` → byte-exact `'E'`/`'F'` prefixes and split/reassemble round-trips
- `atom_cache.rs` → segment addressing, `New`/`Cached` classification, header apply, ref resolution through `beam_codec`, and `encode_control_cached` driven by a real `OutboundAtomCache`
- `connection.rs` → hermetic loopback (a real `TcpListener` on `127.0.0.1:0`, server on a thread): both handshake roles, a `DOP_*` round-trip, a >`DIST_FRAGMENT_SIZE` message, a tick, a wrong-cookie failure that is not a hang, a clean peer close ⇒ `Io(UnexpectedEof)` (never `BadArg`), the outbound atom cache over a real link, the `TcpListener` accept path, the typed send helpers, and the accept policy (default `sok` byte-exact, `Ok` byte-identical, `Refuse` flushing `salive`/`snok` before the client's shared `Io` mapping, a mandatory-check failure never reaching the decision)
- `driver.rs` → two drivers shuttling `take_output()` (whole-buffer and byte-at-a-time), negotiated flags/peer names, an atom-cache round trip (`New` then `Cached`), the empty header `131,68,0`, fragments, a tick answered, malformed/truncated/EOF/send-before-established errors, the accept policy (`sok` first frame, `Refuse` buffering exactly the framed status and going to `Failed`), and the decode-then-commit policy (a retained frame, a repeated typed error, no advance)
- `epmd.rs` → the codecs plus **live epmd**: publish, lookup over a second connection, de-registration, `connect_to_node`, a live `DUMP` asserting the published node's own `Active` line, `lookup_at`/`lookup_port_at`, the `PORT2_REQ` framing pin (frame length = `1 + name.len()`), `not_registered` ⇒ `Io(NotFound)`; `KILL`/`STOP` are codec-only
- `two_node_loopback.rs` → two in-process endpoints over a real loopback socket: bidirectional send family, monitor round-trip, ticks, fragments, an `InboundSink` routing a delivery, peer death as `Closed` vs a typed error — the crate's hermetic "two connected nodes" proof
- `two_node_session.rs` → every two-node-session inbound family, in both directions
- `hermetic_epmd.rs` → an independent in-process `epmd` stub, cross-checked against a real node started with `erl -epmd_port <stub>`
- `live_interop.rs` → the real-`erl` claims: `DOP_REG_SEND` both ways, `DOP_SEND(_SENDER)`, a monitor round-trip, a client-originated `DOP_MONITOR_P` whose watched worker the peer kills (`DOP_PAYLOAD_MONITOR_P_EXIT` with all four fields), `DOP_EXIT2`, an outbound `DOP_UNLINK_ID` with a non-zero id answered by the peer's `DOP_UNLINK_ID_ACK`, and fragmented >`DIST_FRAGMENT_SIZE` messages both ways with `'E'`/`'F'` frames recorded on the wire
- `live_accept.rs` → the **non-blocking driver** accept path against a real `erl` peer
- `live_accept_blocking.rs` → the **blocking** accept path against a real `erl` peer (the two accept implementations share the codecs but not the state machine, so both are pinned)
- `live_driver.rs` → the non-blocking `ConnectionDriver` against a real `erl` peer
- `dist_probe_cli.rs` → the bin's CLI contract via `env!("CARGO_BIN_EXE_dist_probe")`: every literal stdout line, exit `0`, empty stderr, byte-identity across repeated runs
