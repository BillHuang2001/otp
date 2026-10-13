# `beam_dist` — Distribution Protocol (v6) + `epmd` Client

## Intent
Owns compatibility contract **A.5** (*distribution/ETF wire*) at the *protocol/value* layer plus the
Stage-0 foundations of Part II §H: the connect/challenge handshake and its MD5 authentication,
capability (`DFLAG_*`) negotiation, control-message (`DOP_*`) framing + term payloads, message
fragmentation/reassembly, the distribution atom cache, the on-wire packet-length framing, the
distribution-header envelope, and the `epmd` port-mapper client protocol.

The crate's core is **pure protocol/value code over the abstract [`transport`] seam** — the codecs
never open a socket. The [`connection`] module adds a concrete `std::net::TcpStream` adapter
([`connection::TcpTransport`] + [`connection::TcpListener`]) and drives a **live distribution link
end-to-end** (epmd lookup → handshake → `DOP_*` exchange) through the blocking
[`connection::Connection`]; the [`driver`] module adds a **non-blocking `ConnectionDriver`** (a
poll/step state machine over a byte-fed internal buffer) so a runtime can multiplex many links on one
thread. Both are live-tested against a real `erl` node (see Test Strategy). The runtime's production
transport remains `beam_io`'s job.

Term encoding/decoding is **never re-implemented here**: every term crosses the wire through
`beam_codec` (ETF). This crate owns only the *distribution-specific* wire: the flag/opcode vocabulary,
the handshake state machine, the length framing, the header envelope, fragmentation, and the atom
cache.

Dependents: `beam_rt` (node startup `-name`/`-sname`, `system_info(dist)`), `beam_io` (transport).

Ground truth (read-only, never modified):
- `erts/emulator/beam/dist.h` — authoritative `DFLAG_*` (capability bits) + `enum dop` (control
  opcodes) + the mandatory/default/rejectable flag sets + the `ERTS_DIST_SPAWN_FLAG_*` /
  `ERTS_DOP_ALTACT_SIG_FLG_*` bit vocabularies; `ERTS_DIST_FRAGMENT_SIZE`/
  `ERTS_DIST_FRAGMENT_HEADER_SIZE`.
- `erts/emulator/beam/dist.c` — control-message dispatch (`erts_dist_command`, the 1-based control
  tuple), fragment reassembly (`ERTS_PREP_DIST_EXT_FRAG_CONT`), `gen_digest` directions.
- `erts/emulator/beam/external.c` — `erts_encode_ext_dist_header_setup`/`_fragment`/`_finalize`, the
  atom-cache update, `enc_atom`/`dec_atom`.
- `erts/emulator/beam/packet_parser.c` — length framing (a tick is a zero-length packet).
- `erts/emulator/beam/external.h` — `'D'`/`'E'`/`'F'`/`PASS_THROUGH` tags; `ERTS_ATOM_CACHE_SIZE`
  2048; `ERTS_MAX_INTERNAL_ATOM_CACHE_ENTRIES` 255.
- `erts/doc/guides/erl_dist_protocol.md` + `erl_ext_dist.md` — the wire guides (worked examples).
- `lib/kernel/src/dist_util.erl` — the Erlang-side handshake (`gen_digest/2`).
- `lib/erl_interface/src/connect/ei_connect.c` + `ei_connect_int.h` — handshake message layouts,
  `gen_digest`, `gen_challenge`, preferred flags.
- `lib/erl_interface/src/epmd/*.c` + `ei_epmd.h`, `erts/epmd/src/epmd_srv.c` + `epmd_int.h` — epmd wire.

All byte values, tag bytes and field orders are **wire-frozen** (A.5): reproduce them exactly.

## Status
**Implemented and live-verified.** `DFLAG_*`/`DOP_*` vocabulary, the abstract transport seam, 2-/4-byte
framing, handshake message codecs + MD5 authentication, the byte-exact distribution header, message
fragmentation/reassembly, the **outbound and inbound distribution atom cache**, the control-message
codec (all 30 `Dop` variants), inbound `DOP_*` delivery classification, the `epmd` client (real TCP
lookup + the full request set), a blocking `Connection` API, and a **non-blocking `ConnectionDriver`**
(the poll/step state machine), plus a `src/bin/dist_probe.rs` CLI that exposes the hermetic two-node
loopback to the differential harness. `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
beam_dist` is green (**382 tests, 0 failed**) and the workspace builds warning-free; the crate is
safe Rust (`#![deny(unsafe_code)]`).

**All 30 `Dop` opcodes round-trip exhaustively.** `tests/dop_roundtrip.rs` drives every variant through
`encode` -> `decode` -> byte-identical re-encode (the arg table is a wildcard-free `match`, so a new
variant is a compile error) and pins a **byte-exact golden vector per opcode**, all 30 cross-checked
against the system `erl` oracle (`term_to_binary/1` of the equivalent tuple, `VERSION_MAGIC` stripped,
hex-compared). Honest wire facts pinned by the suite: there is **no** `DOP_NODEUP`/`DOP_NODEDOWN` in v6
(node up/down are *local* emulator events, `dist.c:6931` `send_nodes_mon_msgs`), and
`DOP_DEMONITOR_ID`/`DOP_UNLINK_MONITOR_ID`/`DOP_DEMONITOR_MONITOR_ID` **do not exist**; `Dop::from_u8`
rejects `0, 5, 9-11, 14-15, 17, >= 38` and accepts exactly 30. `control::carries_message`
classifies the 16 opcodes carrying a trailing **Message** term (the 9 send family + 5 payload-exit +
`SPAWN_REQUEST`/`SPAWN_REQUEST_TT`); `SPAWN_REPLY`/`_TT` are excluded (`dist.c:1897` sets
`ctx->msg = THE_NON_VALUE`).

**Live interop (blocking `Connection`, `tests/live_interop.rs`).** Against a real `erl -sname` node
whose port is resolved through **`epmd`** (`epmd::lookup_port`, fallback to the pinned port): the OTP-23
`'N'` handshake completes; a `DOP_REG_SEND` is delivered (the node prints `GOT {hello,42}`) and its
reply is decoded back over the same connection; the node echoes its own pid; the client sends a
`DOP_SEND`/`DOP_SEND_SENDER` to that pid (the node prints `GOT {direct,99}`); and a **monitor
round-trip** completes — the node's `DOP_MONITOR_P` is decoded, the client answers
`DOP_MONITOR_P_EXIT`, and the node prints `DOWN … noproc`. Also LIVE-proven: a **client-originated
`DOP_MONITOR_P` is ACCEPTED** (the node kills the watched worker with `boom`, the client decodes
`DOP_PAYLOAD_MONITOR_P_EXIT` with all four fields asserted, incl. the reason); `DOP_EXIT2` (the trapping
process prints the reason); `DOP_UNLINK_ID` -> the node answers `DOP_UNLINK_ID_ACK`; and **fragmented
messages > `DIST_FRAGMENT_SIZE` (200,000 bytes) in both directions**, with `'E'`/`'F'` frames recorded
on the wire.

**Live interop (server side, `tests/live_accept.rs`).** The crate also accepts an inbound link from a
real node — a genuine Rust->node->Rust round trip: the Rust side completes the `'N'` handshake as
**server**, sends a `DOP_REG_SEND` addressed to the peer's registered name with the Rust side's own
external pid as `From`, the node replies `{echo,Seq}` to that pid, and the reply is decoded back as
`Delivery::Send` (raw opcode `DOP_SEND_SENDER`). The working combination is: the pid's node name must be
the **full `node@host`** (a bare short name is refused) and the creation is the **epmd-assigned**
`Alive2Resp::creation()` (threaded into both the driver and the pid). Peer **visibility** is proven live
in both directions: with `server_with_flags(..., preferred_flags() | publish_flag(false, true))` the
peer prints `PEER_VIS visible=true` / `PEER_NODES [rustacc2@billmini]`; with the default (hidden) it
prints `visible=false`.

**Live interop (blocking server path, `tests/live_accept_blocking.rs`).** The **blocking**
`connection::accept_server` / `Connection::server` — the path that previously had only hermetic
coverage — is now pinned against a real `erl -sname` peer dialling the Rust listener: `accept_server`
completes the `'N'` handshake (peer name = full `node@host`, peer prints `CONN=true`), and
`Connection::server` then decodes the inbound signals the peer *initiates* against this node's external
pid — `Pid ! {hi,Seq}` → `Delivery::Send`, `erlang:link(Pid)` → `Delivery::Link`,
`erlang:monitor(process,Pid)` → `Delivery::Monitor` — with the pid threaded through a `DOP_REG_SEND`.
Both handshake implementations (blocking and driver) are therefore live-tested, so they cannot drift.

**Live interop (non-blocking `ConnectionDriver`, `tests/live_driver.rs`).** A genuine non-blocking
`TcpStream` is driven manually (`feed`/`step`/`take_output`/`next_message`) against a real `erl` node:
the handshake completes incrementally and a `DOP_REG_SEND` is delivered (the node prints
`GOT {hello,7,…}`).

**Hermetic two-node loopback (no `erl`, no `epmd`), `tests/two_node_loopback.rs`.** Two in-process
`beam_dist` endpoints — each with its own node name and creation — complete the `'N'` handshake over a
real loopback TCP socket and exchange a name-addressed `DOP_REG_SEND` and a `DOP_SEND_SENDER` in
**both** directions, with a monitor round-trip (`DOP_MONITOR_P` → `DOP_MONITOR_P_EXIT` /
`DOP_PAYLOAD_MONITOR_P_EXIT`), a tick answered per side, a >`DIST_FRAGMENT_SIZE` message both ways
(`'E'` fragment frames proven on the wire), an `InboundSink` routing a received delivery
(`DeliveryOutcome::Delivered`), a `ConnectionDriver` pair over two real **non-blocking `TcpStream`s**,
and peer-death evidence (a clean socket close → `StepOutcome::Closed`; an abrupt mid-frame drop → a
typed error). This is the crate's A.5 "two connected nodes" proof and runs in CI with no external
process.
`tests/two_node_session.rs` extends the hermetic loopback to the **whole inbound signal-family
surface** of a two-node session: `DOP_LINK`, `DOP_UNLINK_ID`/`_ACK`, `DOP_DEMONITOR_P`,
`DOP_EXIT`/`DOP_EXIT2`, `DOP_PAYLOAD_EXIT`/`_EXIT2`, `DOP_GROUP_LEADER`, `DOP_ALIAS_SEND`,
`DOP_SPAWN_REQUEST`/`_REPLY` and `DOP_ALTACT_SIG_SEND` (token and non-token forms) each cross the
loopback **in both directions** and are asserted on their classified `Delivery` fields, plus the `_TT`
token field and a byte-level `'D'` distribution-header pin — so every inbound family a two-node session
uses is proven end-to-end, not classification-only.
`tests/hermetic_epmd.rs` adds an in-process `epmd` **registry stub** (independent
hand-written codecs for `ALIVE2_REQ`/`PORT2_RESP`/`NAMES`/`STOP`, deregistering on connection close)
so the `EpmdClient` is exercised without a host `epmd`, and cross-checks the stub against a real node
spawned with `erl -epmd_port <stub>` (the node's `ALIVE2_REQ` is parsed by the stub and its port
resolved back through the crate's client).

The **outbound atom-cache path is wired end-to-end**: `OutboundAtomCache` implements
`beam_codec::AtomCacheWriter`, `encode_control_cached` threads the cache through a single `Encoder` per
control message, and both `Connection::send_payload` and `ConnectionDriver::send_control` emit the real
`OutboundAtomCache::take_header()` header — an atom-bearing message writes `ATOM_CACHE_REF` (`82`) +
index on the payload and teaches/re-uses those refs in the header; an atom-free message emits the exact
`131, 68, 0` header.

**`epmd` over real TCP works**, covering the complete request set: `ALIVE2_REQ`/`ALIVE2_RESP`/`ALIVE2_X_RESP`,
`PORT2_REQ`/`PORT2_RESP` (`PORT_PLEASE2_REQ`, the same wire byte `122`/`'z'`, an alias), `NAMES_REQ`,
and the interactive `DUMP_REQ`/`KILL_REQ`/`STOP_REQ`. `EpmdClient::connect` / `epmd::lookup_port` /
`epmd::lookup` talk to a live `epmd`; a live test publishes a node (`ALIVE2_REQ`), looks its port up
over a *second* connection, observes de-registration, and reads the real `DUMP_RESP` node table
(asserting its own `Active` line). `connect_to_node(host, node, our_name, cookie)` chains epmd lookup ->
TCP connect -> handshake.

The `DOP_*` **in-term flag vocabularies** are complete: `DIST_SPAWN_FLAG_LINK`/`_MONITOR` and
`DOP_ALTACT_SIG_FLG_PRIO`/`_TOKEN`/`_ALIAS`/`_NAME`/`_EXIT` (plus the `DistSpawnFlags`/`AltactSigFlags`
typed helpers), value-pinned from `dist.h:185-192`, and decoded by the `SpawnRequest`/`SpawnReply`/
`AltactSigSend` deliveries. The handshake **error path is typed**: `Status::kind()` classifies the peer
status text into `StatusKind { Ok, Simultaneous, Named, Nok, NotAllowed, Alive, Other }` and the single
shared `handshake::status_error(&Status) -> BeamError` maps it (`Nok` -> `ConnectionRefused`,
`NotAllowed` -> `PermissionDenied`, `Alive` -> `ConnectionAborted`, `Named` -> `AlreadyExists`,
`Simultaneous` -> `ConnectionAborted`, unknown -> `InvalidData`); `no_creation`/`system_limit` are not
status texts in this release (`Other`). A **refusing acceptor tells the peer** — both acceptors
frame+flush `Status::not_allowed()` (the exact 14 bytes `snot_allowed`) before returning the error
(`dist_util.erl:286`) — and driver handshake errors are terminal. `epmd` `not_registered` ->
`BeamError::Io(NotFound)`, never a silent port `0`.

**The acceptor can refuse** through the additive `AcceptDecision` seam (see Design Decisions and Known
Issues): `accept_server_with_decision`, `Connection::server_with_decision` and
`ConnectionDriver::server_with_decision` consult a caller-supplied policy with the peer's decoded `Name`
after the mandatory check. `Ok` is the default and byte-unchanged; `Refuse(status)` writes that status
(`nok`/`alive`/`not_allowed`) framed+flushed to the peer *before* returning `status_error(&status)`, on
both the blocking and the non-blocking path. What is not modelled is the registry that *decides* — see
the Known Issues entry.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_dist::prelude::*` (see `src/lib.rs`/`src/prelude.rs`).

### `src/lib.rs`
`pub mod {atom_cache, auth, connection, control, delivery, dist_header, driver, epmd, flags, fragment,
framing, handshake, prelude, transport}`, flat `pub use` of the principal items below.
Crate lints: `#![deny(unsafe_code)]`, `#![warn(missing_docs)]`.
Crate constants: `DIST_VERSION: u16 = 6`, `DIST_VERSION_5: u16 = 5`, `EPMD_PORT: u16 = 4369`,
`VERSION_MAGIC: u8 = 131` (alias of `beam_codec::VERSION_MAGIC`).

### `src/flags.rs` — wire-frozen vocabulary (values from `dist.h`, `ei_connect_int.h`)
- `pub type DistFlags = u64` + the `DistFlags` newtype (`EMPTY`, `bits()`, `from_bits`, `contains`,
  `insert`/`remove`, `BitOr/BitAnd/BitXor/Not`).
- Capability bits (`DFLAG_*`: `PUBLISHED` … `NATIVE_RECORDS`), the derived sets
  (`DFLAG_DIST_MANDATORY_25`/`_26`/`DFLAG_DIST_MANDATORY`/`MANDATORY_DIST_FLAGS`/`DFLAG_DIST_HOPEFULLY`
  /`DFLAG_DIST_DEFAULT`/…).
- `Dop` opcodes: `pub const DOP_*: u8` + `enum Dop { … }` with `from_u8`, `as_u8`, `name`, `is_tt`,
  `arity` (control arguments **after** the opcode).
- The **in-message flag vocabularies** (`dist.h:185-192`): `pub const DIST_SPAWN_FLAG_LINK`/`_MONITOR`
  and `DOP_ALTACT_SIG_FLG_PRIO`/`_TOKEN`/`_ALIAS`/`_NAME`/`_EXIT` (`u32`) + the `DistSpawnFlags` /
  `AltactSigFlags` newtypes (`EMPTY`, per-bit consts, `bits`/`from_bits`/`contains`/`insert`/`remove`).
  These are the bit fields carried *inside* the `DOP_ALTACT_SIG_SEND` / `DOP_SPAWN_REPLY*` control
  terms (`dist.c:2590-2698`, `dist.c:3090-3146`) — the vocabulary only.
- Handshake/version tags (`NAME_TAG`, `CHALLENGE_TAG`, `STATUS_TAG`, `CHALLENGE_REPLY_TAG`,
  `CHALLENGE_ACK_TAG`, `COMPLEMENT_TAG`, `DIST_VER_HIGH`/`LOW`) and the epmd tags/node types
  (`EPMD_ALIVE2_REQ`/`_RESP`/`_X_RESP`, `EPMD_PORT2_REQ` + the alias `EPMD_PORT_PLEASE2_REQ`,
  `EPMD_NAMES_REQ`, `EPMD_DUMP_REQ` (`'d'`), `EPMD_KILL_REQ` (`'k'`), `EPMD_STOP_REQ` (`'s'`)).

### `src/transport.rs` — the abstract transport seam (safe)
- `trait Read` / `trait Write` (with default big-endian helpers) + blanket `trait Transport`.
- `Read::read_exact_or_eof(&mut self, buf) -> Result<bool>` — the frame-boundary seam: `Ok(true)`
  when `buf` was filled, `Ok(false)` on a **clean end of stream before any byte was read**, a
  truncated read (`BadArg`) when some bytes arrived but `buf` was not filled. The trait default
  delegates to `read_exact` (so it can only return `Ok(true)`/an error, and no implementor is forced
  to change); `TcpTransport` and `MemoryTransport` override it.
- `struct MemoryTransport` — full-duplex in-memory test double (its exhausted input is a clean EOF
  at a frame boundary for `read_exact_or_eof`, a `BadArg` for `read_exact`).
- Errors: transport failure → `BeamError::Io`; a short read → `BeamError::BadArg`.

### `src/framing.rs` — packet-length framing (safe)
- `enum PacketHeaderSize { Two, Four }`; `read_length`/`read_packet`/`write_packet`/`write_tick`/
  `is_tick`; `MAX_PACKET_BODY` (`u32::MAX`). A zero-length packet is a **tick**; over-ceiling
  advertised length ⇒ `BeamError::SystemLimit`.
- `read_length_or_eof`/`read_packet_or_eof` — the EOF-aware forms: `Ok(None)` when the stream ended
  **at the frame boundary** (no byte of the length prefix arrived), otherwise identical to
  `read_length`/`read_packet` (a truncated prefix/body is still `BadArg`, a zero-length packet is
  still `Ok(Some(tick))`).
### `src/handshake.rs` — connect/challenge handshake messages (safe)
- Body codecs (tag + fields, no length prefix) for `Name` (`'N'`), `Status` (`'s'`), `Challenge`
  (`'N'`), `ChallengeReply` (`'r'`), `ChallengeAck` (`'a'`), `Complement` (`'c'`) — each
  `encode<W: Write>(&self, w)` / `decode(body)`.
- `preferred_flags()` (= `DFLAG_DIST_DEFAULT`), `check_mandatory(flags)` (the `MANDATORY_25_DIGEST`
  rule + mandatory-set check).
- The authentication seam: `trait ChallengeDigest { fn digest(&self, cookie, challenge) -> [u8;16] }`,
  `trait ChallengeSource { fn next_challenge(&self) -> u32 }`, `verify_digest(got, our_challenge,
  cookie, d)`. The concrete MD5 provider is [`crate::auth::Md5Digest`].
- Status classification (a *value* type, not a new error enum): `enum StatusKind { Ok, Simultaneous,
  Named, Nok, NotAllowed, Alive, Other }`, `Status::kind()`,
  `Status::error_kind() -> Option<StatusKind>` (the error kinds only), the refusal constructors
  `Status::not_allowed()`/`Status::nok()`/`Status::alive()` (`"not_allowed"`/`"nok"`/`"alive"`;
  `"alive"` has no dedicated variant, so it is carried verbatim in `Status::Error`), and the single
  shared `status_error(&Status) -> BeamError` mapping (used by both `connection.rs` and `driver.rs`).
- The accept-policy seam type: `enum AcceptDecision { Ok, Refuse(Status) }` (+ `is_ok()`) — the
  accepting side's verdict on a peer's `Name`. `Ok` proceeds with `"ok"`; `Refuse(status)` writes that
  status to the peer and aborts with `status_error(&status)`. Ground truth `dist_util.erl:318-368`
  (`mark_pending/1`).
- Node visibility: `enum PeerVisibility { Normal, Hidden }`, `publish_type(flags: DistFlags) ->
  PeerVisibility` (reads `DFLAG_PUBLISHED`), `publish_flag(hidden: bool, published: bool) -> DistFlags`,
  `enum PublishKind { Normal, Hidden }` + `PublishKind::publish_flag(published) -> DistFlags`.
  `preferred_flags()` stays `DFLAG_DIST_DEFAULT` (the per-connection `publish_flag` piece is the
  override). `struct Handshake { …, peer_visibility: PeerVisibility }`.

### `src/auth.rs` — MD5 challenge/response authentication (safe, no external dep)
- `pub fn md5(data: &[u8]) -> [u8; 16]` — self-contained RFC 1321.
- `pub fn gen_digest(cookie: &[u8], challenge: u32) -> [u8; 16]` = `md5(cookie ++ decimal(challenge))`.
- `pub fn verify(cookie: &[u8], challenge: u32, got: &[u8; 16]) -> bool`.
- `pub fn gen_challenge() -> u32` (nonce); `struct Md5Digest` (impls `ChallengeDigest`);
  `struct SystemChallengeSource` (impls `ChallengeSource`).

### `src/dist_header.rs` — the distribution header (`'D'`) + the `'E'` fragment header (safe)
- Normal header `VERSION_MAGIC + 'D' + N + Flags + AtomCacheRefs` (byte-exact; see the module doc for
  the half-byte `Flags`/`LongAtoms` layout and the `Cached`/`New` ref shapes):
  `struct DistHeader { refs: Vec<AtomCacheRef>, long_atoms: bool }`, `enum AtomCacheRef`,
  `DistHeader::{empty, encode, decode, decode_tail}`, `MAX_ATOM_CACHE_REFS`, `ATOM_CACHE_SEGMENTS`.
- Fragmented first frame: `struct FragmentedHeader { fragment: FragmentHeader, header: DistHeader,
  payload_offset }` + `decode_fragmented_header(...)` — recognises `'E'` and reads the fragment
  prefix before reusing the shared tail logic (no duplicated flag/ref code).
- `enum DistFragment { Header, Continue }` — `'E'`/`'F'` tag recognition.

### `src/fragment.rs` — message fragmentation: `'E'` header, `'F'` continuation (safe)
- `DIST_FRAGMENT_SIZE` (`64*1024`), `DIST_FRAGMENT_HEADER_SIZE` (`18`), `DEFAULT_MAX_FRAGMENTED_MESSAGE`
  (`64 MiB`).
- `struct FragmentHeader { seq_id, fragment_count }` (`VERSION_MAGIC + 'E' + seq_id(8) + count(8)`),
  `struct FragmentCont { seq_id, fragment_index }` (`VERSION_MAGIC + 'F' + seq_id(8) + index(8)`).
- Sender: `split(seq_id, header_tail, payload, fragment_size) -> Result<Vec<Vec<u8>>>`.
- Receiver: `struct FragmentAssembler` (`new`/`with_limit`/`with_encoding`/`push`/`is_pending`/`reset`),
  `enum Assembly { NotFragmented, Pending, Complete(Vec<u8>) }`.

### `src/atom_cache.rs` — distribution atom cache / segments (safe)
- Addressing: `ENTRIES_PER_SEGMENT` (256), `ATOM_CACHE_SIZE` (2048), `USE_ATOM_CACHE_SIZE` (2039),
  `slot_index(segment, index)`, `atom_to_cix(atom)` (`atom.raw() % 2039`).
- `struct SegmentCache` — persistent 2048-slot 8×256 store (`get`/`set`/`clear`/`occupied`).
- Receive side: `apply_header(&DistHeader, &mut SegmentCache, &mut beam_codec::AtomCache)` +
  `struct ReceiveAtomCache { new, apply, table, persistent, ref_count, clear }` — applies a header's
  refs so `beam_codec` resolves payload `ATOM_CACHE_REF`s (the `beam_codec::AtomCache` is the
  per-message ≤255-entry translation table; the 8-segment persistent space lives here).
- Send side: `struct OutboundAtomCache { new, ref_for, payload_index, payload_ref_for, header,
  take_header, advance_segment, clear, len, is_empty, persistent }` — classifies atoms `New`/`Cached`
  and yields `AtomCacheRef`s; `payload_ref_for` returns the payload bytes `[82, index]`.
- **`impl beam_codec::AtomCacheWriter for OutboundAtomCache`** — `intern(atom, atoms)` = `ref_for`
  (classification) + `payload_index`, i.e. the concrete hook `Encoder::with_atom_cache` calls. This is
  what makes the encoder emit `[82, index]` for a cached atom instead of the inline atom name.

### `src/control.rs` — `DOP_*` control-message codec (safe, wire-exact)
The control message is an ETF **tuple** `SMALL_TUPLE_EXT`/`LARGE_TUPLE_EXT + arity + element_1 …`,
where `element_1` is the opcode as a `SMALL_INTEGER_EXT`; for message-carrying opcodes a single
trailing **Message** term follows the tuple in the same body. No leading `VERSION_MAGIC` (the enclosing
distribution header carries it).
- `struct ControlMessage { dop: Dop, terms: Vec<Eterm> }` (flattened: control args, then the trailing
  message when present).
- `carries_message(dop) -> bool` — the 16 opcodes that carry a trailing **Message** term (the 9 send
  family + 5 payload-exit + `SPAWN_REQUEST`/`SPAWN_REQUEST_TT`); `SPAWN_REPLY`/`_TT` are excluded.
- `encode_control_cached(dop, terms, atoms, Option<&mut dyn beam_codec::AtomCacheWriter>) ->
  Result<Vec<u8>>`; `encode_control(dop, terms, atoms) -> Result<Vec<u8>>` (a thin wrapper over the
  cache-less case). The opcode + control args + trailing message are encoded through **one**
  `Encoder::new_body(atoms)` so a supplied cache sees every atom in payload order — which is exactly
  the header ref order.
- `decode_control(body, &mut Heap, &mut AtomTable, Option<&AtomCache>) -> Result<ControlMessage>`.
  Unknown opcode / wrong arity / malformed tuple ⇒ `BeamError::BadArg`. Term encoding is **only**
  `beam_codec`.

### `src/epmd.rs` — `epmd` port-mapper + admin client (safe)
2-byte-BE-framed **requests** (`ALIVE2_REQ`/`RESP`/`X_RESP`, `PORT2_REQ`/`RESP`, `NAMES_REQ`,
`DUMP_REQ`, `KILL_REQ`, `STOP_REQ`) + `struct EpmdClient<T: Transport>`
(`publish`/`port_please`/`names`/`dump`/`kill`/`stop`). **Replies are NOT length-framed** (`epmd` writes
the body straight to the socket) — the client reads them unframed/self-delimiting (see Design
Decisions).
- `EpmdClient<TcpTransport>::connect(addr) -> Result<EpmdClient<TcpTransport>>` and the free
  `epmd::connect(addr)` — a real TCP client to `epmd` (sets `DEFAULT_READ_TIMEOUT` + `TCP_NODELAY`).
- `epmd::lookup(name) -> Result<Port2Resp>` (raw) and `epmd::lookup_port(name) -> Result<u16>`
  (`Port2Resp::Error` ⇒ `BeamError::Io(NotFound)`, never a silent `0`); both connect to
  `EPMD_DEFAULT_ADDR` (`"127.0.0.1:4369"`, pinned to `EPMD_PORT`) and delegate to the
  address-taking forms below.
- `epmd::lookup_at(addr: &str, name) -> Result<Port2Resp>` and
  `epmd::lookup_port_at(addr: &str, name) -> Result<u16>` — the same lookups against an **arbitrary**
  `epmd` address (the plumbing that lets a name be resolved against a private daemon, e.g.
  `erl -epmd_port N`). `lookup_port_at` applies the identical `Port2Resp::Error` ⇒ `Io(NotFound)`
  translation (never a silent `0`).
- Typed accessors: `Alive2Resp::{is_accepted, is_rejected, is_alive_conflict, creation}`,
  `Port2Resp::{is_registered, is_not_registered}`.
- Admin/DUMP codecs: `DumpReq`/`DumpResp`/`DumpLine`/`DumpNodeKind { Active, OldUnused }` (the
  node-table dump), `KillReq`/`KillResp { Ok, Refused }` (`"OK"`/`"NO"`), `StopReq { name }`/`StopResp
  { Stopped, NoExist, NoReply }` (success text is **`"STOPPED"`**, not `"OK"` — `epmd_srv.c`).
  `MAX_DUMP_RESP_BODY` (`1 MiB`) bounds the open-ended `DUMP_RESP`; over-ceiling ⇒ `SystemLimit`.
  `kill`/`stop` are **codec-only by design** (destructive — never issued by a test).
- `NamesResp::encode` is lossy on per-node ports (only `epmd_port` + names are retained).

### `src/connection.rs` — the blocking connection-level API + `TcpStream` adapter (safe)
- `struct TcpTransport` (impls `Read`/`Write`); `connect(addr)` sets a read timeout + `TCP_NODELAY`;
  `from_stream`; `set_read_timeout`/`set_nodelay`/`stream`/`into_inner`; `DEFAULT_READ_TIMEOUT` (10 s).
- `struct TcpListener` (wrapping `std::net::TcpListener`): `bind(addr)`/`local_addr`/`accept` (yields a
  `TcpTransport` with the read timeout + `TCP_NODELAY`)/`set_nonblocking`/`into_inner` — the server side.
- Handshake driver (2-byte framing): `connect_client(t, our_name, cookie) -> Result<Handshake>`,
  `connect_client_with_creation(t, our_name, cookie, creation)`, `accept_server(t, our_name, cookie,
  creation)`, plus the opt-in flag overrides `connect_client_with_flags(t, our_name, cookie, creation,
  flags)` and `accept_server_with_flags(t, our_name, cookie, creation, flags)`, all
  `-> Result<Handshake>`, where `struct Handshake { client_flags, server_flags, negotiated,
  peer_creation, peer_name, peer_visibility, buffered }`. `pub const CLIENT_CREATION: u32 = 0` is the
  default client creation. `Connection::peer_visibility()` reads the negotiated peer visibility.
- The accept-policy seam:
  `accept_server_with_decision(t, our_name, cookie, creation, flags, decide: impl FnOnce(&Name) ->
  AcceptDecision) -> Result<Handshake>` — the same accept sequence, consulting `decide(&name)` after
  the mandatory check; `Refuse(status)` sends the status frame (via `send_refusal`) *before* returning
  `status_error(&status)`. `accept_server`/`accept_server_with_flags` delegate with the private
  `continue_handshake` (always `Ok`) — byte-identical to before. `Connection::server_with_decision(...)`
  is the `Connection` constructor counterpart.
- `connect_to_node(host, node_name, our_name, cookie) -> Result<Connection<TcpTransport>>` (+
  `connect_to_node_with_creation`) — `epmd::lookup_port` → `TcpTransport::connect` → `Connection::client`.
- Established link (4-byte framing): `struct Connection<T: Transport>` —
  `client(t, name, cookie)` / `client_with_creation(t, name, cookie, creation)` /
  `server(t, name, cookie, creation)`; `negotiated`/`client_flags`/`server_flags`/`peer_name`/
  `peer_creation`/`handshake`/`ticks_answered`/`into_inner`; `send_control(dop, terms, atoms)`,
  `send_message(from, to, msg, atoms)`, `send_tick()`, `recv(&mut Heap, &mut AtomTable) ->
  Result<ControlMessage>`; typed `DOP_*` helpers `send_reg_send(from, cookie, to_name, msg, atoms)` /
  `send_link(from, to, atoms)` / `send_exit(from, to, reason, atoms)` /
  `send_monitor_p(from, to, ref, atoms)` / `send_demonitor_p(from, to, ref, atoms)`.
  `MAX_ESTABLISHED_PACKET_BODY`. The connection owns an `OutboundAtomCache` that `send_payload` drains
  via `take_header()`. **The frame read (`read_message_body`) distinguishes a clean peer EOF from
  malformed data**: a peer that closed the socket at a frame boundary surfaces as `BeamError::Io` with
  `std::io::ErrorKind::UnexpectedEof`, while a truncated/malformed frame stays `BadArg` (the
  handshake reads keep the plain `read_packet`/`BadArg` behaviour). The `TcpTransport` `Read` impl
  overrides `read_exact_or_eof` to make that distinction over a real socket.

### `src/delivery.rs` — inbound `DOP_*` delivery classification + the `InboundSink` seam (safe)
Turns a decoded [`ControlMessage`] into its **semantic** delivery and routes it to the local recipient.
- `enum Delivery { Send{from,to,message,token}, RegSend{from,cookie,name,message,token},
  AliasSend{..}, Link{..}, Exit{..}, Exit2{..}, UnlinkId{id,from,to}, UnlinkIdAck{..},
  Monitor{watcher,watched,reference}, Demonitor{..}, MonitorExit{..}, GroupLeader{from,to},
  PayloadExit{..}, PayloadExit2{..}, PayloadMonitorExit{..}, SpawnRequest{..}, SpawnReply{..},
  AltactSigSend{..}, Other(Dop) }` + `Delivery::dop()` and `Delivery::classify(&ControlMessage) ->
  Result<Delivery>` (argument order from `dist.c`; **all 30 opcodes are classified** — the only
  `Other(dop)` is `DOP_UNLINK`, which the C itself *rejects* (`dist.c:2277-2282`), never a substituted
  send). The `SpawnRequest`/`SpawnReply`/`AltactSigSend` variants carry the typed
  `DistSpawnFlags`/`AltactSigFlags`; an out-of-range flag (`>= 1 << 27`) or a non-small/negative altact
  flag is an honest `BadArg`.
- `enum DeliveryOutcome { Delivered, NoSuchProcess, Unsupported, Dropped }`,
  `struct Dispatched { delivery, outcome }`.
- `trait InboundSink { fn deliver(&mut self, &Delivery) -> Result<DeliveryOutcome> }` — **defaulted**
  to `Unsupported` (a runtime with no process registry never mis-delivers); `struct UnsupportedSink`
  is the do-nothing impl. The production impl belongs to `beam_rt` (over `beam_process`/`Mailbox`) —
  keeping the value layer free of the message/process crates (dependency DAG).
- The **checked** (validated) view: `struct LinkIdentity { peer_node, local_node }` +
  `LinkIdentity::intern(&mut AtomTable, peer_name, local_name)` (interns the two node names into the
  table the message was decoded against) and `Delivery::classify_validated(&ControlMessage,
  &LinkIdentity) -> Result<Delivery>` — `classify` **plus** the C's per-opcode `invalid_message`
  shape/`dep` anti-spoof checks (`dist.c`), so a forged or malformed message is `Err(BeamError::BadArg)`
  instead of mis-delivered. Opt-in: `classify` is unchanged.
- The two entry points each have an unchecked and a checked form:
  `ConnectionDriver::next_delivery`/`dispatch_inbound` and `Connection::recv_delivery`/
  `dispatch_inbound` (the semantic view) vs `ConnectionDriver::next_delivery_checked`/
  `dispatch_inbound_checked` and `Connection::recv_delivery_checked`/`dispatch_inbound_checked` (the
  per-link validated view).

### `src/driver.rs` — the non-blocking poll/step state machine (safe)
`struct ConnectionDriver` + `enum DriverState { Handshaking, Established, Closed, Failed }` +
`enum StepOutcome { WouldBlock, Handshaking, Established, Progress, Closed, Failed }`. **Byte-fed**: the
runtime owns the socket and does the syscalls; the driver only parses/serializes.
- `client(our_name, cookie, creation)` / `server(our_name, cookie, creation)` (+ `*_with_challenge` and
  the `*_with_flags(…, flags)` flag overrides), `is_client`.
- `server_with_decision(our_name, cookie, creation, flags, decide: Box<dyn Fn(&Name) -> AcceptDecision
  + Send + Sync>)` — the accept-policy seam; `server`/`server_with_flags` are the same constructor with
  no policy (always `Ok`).
- `feed(&[u8])` / `feed_eof()` — appends socket bytes / signals a clean EOF.
- `step() -> Result<StepOutcome>` — makes all possible progress (incremental 2-byte handshake, then
  4-byte-framed established frames, tick replies, `'E'`/`'F'` reassembly); returns `WouldBlock` when
  nothing more can be done.
- `take_output() -> Vec<u8>` / `has_output()` — bytes the runtime must write to the socket.
- `next_message(&mut Heap, &mut AtomTable) -> Result<Option<ControlMessage>>` — decodes the next queued
  payload (applying the receive atom cache).
- `send_control(dop, terms, atoms)` / `send_message(from, to, msg, atoms)` / `send_tick()` — encode +
  envelope into the output buffer.
- `state()` / `negotiated()` / `client_flags()` / `server_flags()` / `peer_name()` / `peer_creation()` /
  `peer_visibility() -> Option<PeerVisibility>` / `handshake()` / `ticks_answered()` / `is_eof()`.
- Runtime drive loop: `read → feed → loop{step until WouldBlock} → take_output → write → while
  next_message{ handle }`; `Closed`/`Failed` mean drop the link.

### `src/bin/dist_probe.rs` — the hermetic-loopback CLI (bin target, not part of the library API)
`dist_probe loopback <report>`. Cargo auto-discovers `src/bin/`, so no `Cargo.toml` change. The bin is
**self-contained** (it re-uses only the crate's public API — `Connection`/`ConnectionDriver`/
`TcpListener`/`TcpTransport`/`Delivery`/`InboundSink`/`connect_client_with_creation`/`send_control`/
`send_reg_send`/`send_message`/`send_link`/`send_exit`/`send_monitor_p`/`recv_delivery`/
`dispatch_inbound` — plus `beam_codec`/`beam_term`/`beam_alloc`): it stands up two in-process endpoints
(`alpha@localhost` acceptor / `beta@localhost` connector) over a real loopback socket and completes the
`'N'` handshake both ways. Every report prints **exactly one** deterministic, host-independent stdout
line (no timestamps/addresses/ports) and exits `0`:

| `<report>` | stdout (one line) |
|---|---|
| `ping` | `pong` iff both handshakes completed (else `pang`) |
| `nodes` | the number of peers node A handshook with (`1`) |
| `term` | `hex(VERSION_MAGIC ++ encode_term_body(received))` of the delivered `{1,2,3}` (`836803610161026103`) |
| `nodeid` | the negotiated `DFLAG_*` word (`u64` hex) + A's peer name/creation/visibility |
| `handshake` | a **pinned**, byte-exact transcript of the `'N'` handshake (both directions) |
| `badcookie` | the refusal shape when the connector presents a wrong cookie (`server=`/`client=` error class + the tags that crossed) |
| `frag` | a >[`DIST_FRAGMENT_SIZE`] payload round trip: received length + checksum + the count of `'E'`/`'F'` frames A really wrote |
| `alias` | the decoded `DOP_ALIAS_SEND` fields |
| `monitor` | the decoded `DOP_MONITOR_P` / `DOP_LINK` / `DOP_EXIT` fields |
| `regsend_missing` | the local outcome of a `DOP_REG_SEND` to an unregistered name |
| `utf8` | the uppercase hex ETF image of a non-Latin-1 atom that crossed the wire |

- `ping`/`nodes`/`term` drive the mutual `{1,2,3}` round trip (A → B name-addressed as a `DOP_REG_SEND`,
  B echoes it to A's pid as a `DOP_SEND(_SENDER)`, A receives it) and derive their observable from it.
  Their stdout is **frozen** — the harness's existing `dist/*` cases depend on those exact bytes.
- `handshake` drives two `ConnectionDriver`s with **pinned** challenge nonces (the `*_with_challenge`
  seams), so the digests are constant across runs; the emitted fields are the raw 2-byte-framed wire
  bytes of each direction. `frag` wraps each endpoint's transport in a `Spy` (a `Transport` that counts
  `VERSION_MAGIC`+`'E'`/`'F'` writes) and fails loudly if fewer than two frames were written.
- `regsend_missing` models the **receiver's** half with an `InboundSink`: ground truth
  (`dist.c:2484-2509`) is that a `DOP_REG_SEND` to an unregistered name is *silently dropped* — the C
  looks the name up and, on a miss, returns with no error and nothing sent back (so there is no
  public-API path that yields a `badarg` here).
- An unknown/missing subcommand → a usage line on **stderr**, exit `2`. A loopback that cannot be set
  up, or a report that cannot be produced → `dist_probe: unsupported: <reason>` on **stderr**, exit `3`
  (the harness's capability-gap convention) — never a fabricated line.
- Bin target: its own `#![deny(unsafe_code)]` + `//!` doc (crate lints are not inherited by a bin);
  safe Rust only.

## Constraints
- **Safe Rust only** — `#![deny(unsafe_code)]`. `TcpTransport` uses only `std::net`.
- **Errors**: only `beam_common::{Result, BeamError}`. Malformed/short/unknown wire data ⇒ `BadArg`;
  an advertised length/size beyond a wire/message ceiling ⇒ `SystemLimit`; transport failure ⇒ `Io`.
  **Never** panic on wire data, never `unwrap()` on decoded bytes.
- **Big-endian** for every multi-byte field. Field order/exact byte counts are wire-frozen.
- **No duplicated term codec** — all term encode/decode goes through `beam_codec`.
- **No new external dependencies** (the in-tree MD5 replaces the `md-5` crate).
- **Bounded allocation**: validate advertised lengths/buffered fragments against a ceiling before
  allocating; oversize ⇒ `SystemLimit`.
- Files target ~1000 lines; split by responsibility. Licensing: Apache-2.0. Three files run slightly
  over (`epmd.rs` ~1210, `connection.rs` ~1040, `driver.rs` ~1030) and carry top-of-module justification;
  `connection.rs` already path-includes its unit tests from `connection_tests.rs`.

## Test Strategy
- In-module `#[cfg(test)]` unit tests per module + `tests/` integration tests for cross-module seams.
- **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_dist` green;
  plus `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Coverage highlights:
  - `tests/constants.rs` — exact-value pins for `DFLAG_*`/`DOP_*`, the `DIST_SPAWN_FLAG_*` /
    `DOP_ALTACT_SIG_FLG_*` vocabulary (incl. the shared bit 0 in distinct namespaces) and the
    handshake/epmd tags.
  - `tests/auth.rs` + in-module — RFC 1321 KATs; `gen_digest` known answers derived from the system
    oracle (`erl -noshell -eval 'io:format("~s",[binary:encode_hex(erlang:md5(<<"COOKIE",...>>))])'`).
  - `tests/handshake.rs`/`framing.rs`/`control.rs`/`epmd.rs` — round-trips + byte-exact vectors +
    negatives; `control.rs` pins the wire tuple bytes (`DOP_SEND` args `[1,2]` ⇒ `[104,3,97,2,97,1,97,2]`)
    and proves `encode_control_cached` is byte-identical to `encode_control` with no cache and writes
    `[82, index]` with one; `handshake.rs` covers `Status::kind`/`error_kind`/`status_error`, the three
    refusal constructors' exact wire texts (`snok`/`snot_allowed`/`salive`), `AcceptDecision`, and the
    `not_allowed` wire bytes; `epmd.rs` covers the admin (`DUMP`/`KILL`/`STOP`) codecs + the typed
    conflict accessors.
  - `tests/dop_roundtrip.rs` — the **exhaustive 30/30 opcode round-trip** (encode → decode →
    byte-identical re-encode, arg table a wildcard-free `match`) + the 30 byte-exact golden vectors
    cross-checked against the system `erl` oracle (`term_to_binary/1`, `VERSION_MAGIC` stripped).
  - `tests/delivery.rs` — `Delivery::classify` over every opcode (arg-order pins; the
    `SpawnRequest`/`SpawnReply`/`AltactSigSend` flag decoding incl. the out-of-range `BadArg` negative;
    `DOP_UNLINK` ⇒ `Other`).
  - `tests/fragment.rs` — byte-exact `'E'`/`'F'` prefixes, split/reassemble round-trips, negatives.
  - `tests/atom_cache.rs` — segment addressing, `New`/`Cached` classification, header apply, ref
    resolution through `beam_codec`, and `encode_control_cached` driven by a real `OutboundAtomCache`
    (the header it yields resolves the payload the encoder wrote).
  - `tests/connection.rs` — hermetic loopback (real `TcpListener` on `127.0.0.1:0`, server on a
    thread): both handshake roles, a `DOP_*` round-trip, a >`DIST_FRAGMENT_SIZE` message, a tick, a
    wrong-cookie failure that is not a hang, a **clean peer close at a frame boundary** surfacing as
    `Io(UnexpectedEof)` (never `BadArg`), and the **outbound atom cache over a real link** (one
    `New` ref per atom, `82 + index` payload, the peer resolves the names). A spy transport asserts
    `'E'` frames really crossed; the new `TcpListener` path is exercised (`accept` on a thread, plus a
    non-blocking `accept` → `WouldBlock`), and the typed `send_reg_send`/`send_link`/`send_exit`/
    `send_monitor_p` helpers round-trip. The **accept policy** is pinned here too: the default acceptor's
    status frame is byte-exactly `sok` (spied, and never `sok_simultaneous`); an `Ok` decision is
    byte-identical and completes; a `Refuse` decision flushes its status frame (asserted as the exact
    `salive`/`snok` body inside a 2-byte frame) before the client surfaces the shared
    `Io(ConnectionAborted)`/`Io(ConnectionRefused)` mapping, and a `Name` failing the mandatory check
    never reaches the decision.
  - `src/connection_tests.rs` — the same frame-read seam hermetically over `MemoryTransport`: a clean
    peer EOF at a frame boundary ⇒ `Io(UnexpectedEof)`, while a **truncated length prefix** and a
    **truncated body** both stay `BadArg` (boundary vs mid-frame, no real socket needed).
  - `tests/epmd.rs` — the epmd codecs plus **live epmd**: `publish` an `ALIVE2_REQ` on one connection,
    `port_please`/`lookup_port` it over a second, observe de-registration on close, `connect_to_node`
    end-to-end, a live `DUMP_REQ` (asserting the published node's own `Active` line), and a live
    `lookup_at`/`lookup_port_at` resolution; the `PORT2_REQ` framing pin (frame length =
    `1 + name.len()`, the name is the packet remainder) and the `not_registered` → `Io(NotFound)` typed
    error. `KILL`/`STOP` are codec-only (destructive). Skips loudly when epmd is unreachable.
  - `tests/driver.rs` — the `ConnectionDriver`, hermetic: two drivers shuttling `take_output()` bytes
    (whole-buffer **and** byte-at-a-time), negotiated flags/peer names, an atom-cache round trip
    (`New` then `Cached`), the exact empty header `131,68,0`, a >`DIST_FRAGMENT_SIZE` message both ways
    (`'E'` verified), a tick answered, and malformed/truncated/EOF/send-before-established errors. The
    **accept policy** is pinned too: the default server driver's first frame is the byte-exact `sok`;
    an `Ok` decision is byte-identical to the default and drives a full handshake to completion; a
    `Refuse` decision buffers exactly the 2-byte-framed `salive`/`snok` frame (no `Challenge` follows),
    returns the shared `Io` kind and goes to `DriverState::Failed`.
  - `tests/live_interop.rs` (blocking) + `tests/live_driver.rs` (non-blocking) + `tests/live_accept.rs`
    (server side) — **live interop with a real `erl` node.** Each starts `erl -sname <uniq> -setcookie
    <c> -kernel inet_dist_listen_min P inet_dist_listen_max P` (P a free loopback port), resolves P
    through **`epmd::lookup_port`** (falling back to P if epmd is unreachable), and completes the
    handshake. `live_interop.rs` sends a `DOP_REG_SEND` (the node prints `GOT {hello,42}`), receives
    the node's `DOP_SEND_SENDER` reply, sends a `DOP_SEND` to the node pid the node echoed back (the
    node prints `GOT {direct,99}`), completes a monitor round-trip (`DOP_MONITOR_P` decoded →
    `DOP_MONITOR_P_EXIT` → the node prints `DOWN … noproc`), proves a **client-originated
    `DOP_MONITOR_P`** is accepted (`DOP_PAYLOAD_MONITOR_P_EXIT` with reason `boom`), `DOP_EXIT2`,
    `DOP_UNLINK_ID`→`DOP_UNLINK_ID_ACK`, and **fragmented >`DIST_FRAGMENT_SIZE` messages both ways**.
    `live_accept.rs` completes the handshake as **server** and does the Rust->node->Rust round trip (the
    full `node@host` name + epmd-assigned creation) plus published-vs-hidden peer visibility.
    `live_accept_blocking.rs` pins the **blocking** `accept_server`/`Connection::server` against a real
    peer and decodes the inbound `Send`/`Link`/`Monitor` signals the peer initiates against our pid.
    `live_driver.rs` drives the `ConnectionDriver` over a genuine non-blocking `TcpStream`. The shared
    spawn/harness lives in `tests/common/mod.rs`. All skip honestly (clear `eprintln`) only when `erl`
    is absent on `PATH`; never fake a pass. Opcodes/paths that cannot be exercised against a live peer
    are pinned by **byte-exact golden vectors** instead (labelled as such).
  - `tests/two_node_loopback.rs` — the **hermetic two-node loopback** (contract A.5's "two connected
    nodes", in one process, no `erl`/`epmd`, bounded waits): two named endpoints with distinct
    creations handshake over a real loopback socket and exchange `DOP_REG_SEND`/`DOP_SEND(_SENDER)`
    **both ways**; a monitor round-trip; a tick answered per side; a >`DIST_FRAGMENT_SIZE` message both
    ways (`'E'` frames proven via a spy transport); an `InboundSink` routing a delivery
    (`DeliveryOutcome::Delivered`, not `Unsupported`); a `ConnectionDriver` pair over two real
    non-blocking `TcpStream`s; peer-death evidence (clean close → `Closed`, abrupt drop → typed error);
    and the v6 "no `DOP_NODEUP`/`DOP_NODEDOWN`" pin.
  - `tests/two_node_session.rs` — the **full inbound signal-family** loopback: 11 tests drive every
    two-node-session family (`Link`, `UnlinkId`/`_Ack`, `DemonitorP`, `Exit`/`Exit2`, `PayloadExit`/
    `_Exit2`, `GroupLeader`, `AliasSend`, `SpawnRequest`/`SpawnReply`, `AltactSigSend` token/non-token,
    plus a `_TT` token field) across a real loopback socket in **both** directions with typed `Delivery`
    asserts, and a spy transport pins a real `'D'` header on the wire.
  - `tests/hermetic_epmd.rs` — a test-local `epmd` **registry stub** (an **independent** hand-written
    request decoder / reply encoder, so the round-trip is not self-consistent by construction) driving
    the crate's `EpmdClient` (`publish`/`port_please`/`names`/`stop`) and the address-taking
    `lookup_at`/`lookup_port_at` (the stub's ephemeral port is exactly the non-default address those
    forms exist for — including the `Io(NotFound)` translation) hermetically, plus an **oracle
    cross-check** that spawns a real node with `erl -epmd_port <stub>` and asserts the stub parsed the
    node's `ALIVE2_REQ` and the crate's client resolved its port. Needs no host `epmd`; skips loudly
    only when `erl` is absent.
  - `tests/dist_probe_cli.rs` — drives the built `dist_probe` bin via `env!("CARGO_BIN_EXE_dist_probe")`:
    for **every** report (`ping`/`nodes`/`term` + the eight `badcookie`/`nodeid`/`frag`/`alias`/
    `monitor`/`regsend_missing`/`handshake`/`utf8`) the **exact** stdout line and exit `0` (and empty
    stderr), a repeated-run byte-identity check, plus exit `2` for a missing/unknown/over-long
    subcommand. The bin also carries `#[cfg(test)]` unit tests for the `term` hex encoding, the
    `frame_tags` handshake-tag walker, the FNV-1a checksum, and the `Spy`'s fragment-frame counting.

## Design Decisions
- **MD5 is implemented in-tree, not via a dependency.** The digest is one 16-byte hash; `md-5` would
  pull in `digest`/`generic-array`/`typenum`/`block-buffer` and churn the shared `Cargo.lock`. A
  self-contained RFC 1321 implementation (KAT-verified) keeps the crate dependency-light. The
  `ChallengeDigest` seam stays so a future provider can replace it.
- **The digest is `md5(cookie ++ decimal(challenge))` — cookie first.** Ground truth: `dist_util.erl`
  `gen_digest/2` = `erlang:md5([atom_to_list(Cookie)|integer_to_list(Challenge)])` and `ei_connect.c`
  `gen_digest` = `md5(cookie)` then `sprintf("%u", challenge)`. The challenge is rendered as its
  **unsigned decimal** string (no leading zeros / no length prefix / cookie has no NUL).
- **`DistFlags` newtype + raw `u64` consts** (ergonomic `contains`/bitops while value pins stay
  `u64`); chosen over `bitflags` (no new dep). `DistSpawnFlags`/`AltactSigFlags` mirror the same shape
  over `u32` (the in-term flag field width).
- **The outbound atom cache is threaded through a single `Encoder` per control message.** The trait
  hook lives in `beam_codec` (which must not depend on `beam_dist`); `encode_control_cached` builds
  the whole tuple body — opcode, control args and the trailing message — through one
  `Encoder::new_body(atoms).with_atom_cache(cache)`. Because the encoder interns atoms in payload
  order and `OutboundAtomCache` assigns ref positions in the same order (ERTS `insert_acache_map`),
  the payload's `82` index is exactly the header ref position. `send_payload` then drains the cache
  with `take_header()` and writes the real header; the message-level ref list is consumed, the
  persistent per-connection cache survives to serve `Cached` refs on later messages.
- **`DistHeader::decode_tail` is shared** by the `'D'` and `'E'` (fragmented) decoders so the
  flag/ref/long-atoms logic is written once.
- **Fragment wire layout.** `'E'`/`'F'` prefixes are `VERSION_MAGIC + tag + seq_id(8) + frag_id(8)`
  (18 bytes); there is **no** separate `dist_hdr_flags` byte on the wire (ERTS folds it into the
  `Flags` half-bytes). `'E` carries the normal-header tail verbatim; `'F'` carries raw payload. The
  continuation fragment id counts **down** (the last fragment is `1`). The reassembler is strictly
  in-order (as ERTS requires) and bounds buffered bytes.
- **Atom cache: two spaces.** `beam_codec::AtomCache` is the **per-message translation table**
  (≤255 entries, addressed by the payload `ATOM_CACHE_REF` byte = the ref's *position* in the header);
  the **persistent 8×256 segment space** lives in `atom_cache::SegmentCache`. The `SegmentIndex` is a
  fixed function of the atom hash (`atom.raw() % 2039`), not a per-message rotation.
- **Control message is a wire tuple.** The opcode is the tuple's first element as a small integer (not
  a raw leading byte) — required by real ERTS. `ControlMessage` keeps a *flattened* `terms` view
  (control args + trailing message) as an ergonomic decoded form.
- **The `connection` module deliberately opens a `TcpStream`.** The A.5 core stays transport-agnostic;
  the adapter is what makes the handshake + `DOP_*` layer usable end-to-end (contract A.5's "wire" is
  only demonstrable over a real socket). The runtime's production transport remains `beam_io`'s.
- **Handshake framing is 2-byte; established framing is 4-byte.** The frame read after `ChallengeAck`
  is treated as the OTP-23 `Complement` only when its tag is `'c'`; otherwise it is buffered and
  delivered by the next `recv`.
- **The accept handshake exists twice (by design) and both halves are live-tested.** The blocking
  `connection::accept_server_core` and the non-blocking `driver::ConnectionDriver` server states each
  implement the accept sequence (they share the message codecs, `check_mandatory`, `auth` and
  `status_error`, but not the state machine). `tests/live_accept.rs` pins the driver path against a real
  `erl` peer and `tests/live_accept_blocking.rs` pins the blocking path, so the two cannot drift
  silently.
- **epmd frames its requests but not its replies.** A client *request* is 2-byte length-framed
  (`epmd_port.c`), but `epmd` writes every *reply* body straight to the socket with no length prefix
  (`epmd_srv.c` `reply()`); the client reads it unframed/self-delimiting — by tag for `'y'`/`'v'`/`'w'`,
  and "read until the transport goes quiet" for the open-ended `NAMES_RESP` line list.
- **The accept policy was added as an additive, defaulted seam — never a signature change.**
  `AcceptDecision` (`Ok` | `Refuse(Status)`) plus three new entry points
  (`accept_server_with_decision`, `Connection::server_with_decision`,
  `ConnectionDriver::server_with_decision`) leave every existing function's type and behaviour intact:
  `accept_server`/`accept_server_with_flags`/`Connection::server*` pass the private always-`Ok`
  `continue_handshake`, and `ConnectionDriver::server*` store `None`, so the default status frame stays
  the byte-identical `131`-free 2-byte-framed `sok`. The consultation happens **after**
  `check_mandatory` (a real node runs `check_dflags` first, `dist_util.erl:286`) and a refusal reuses the
  existing `send_refusal`/`frame` notification path, so it cannot leave the peer hanging. The blocking
  seam takes `impl FnOnce(&Name)` and the driver a `Box<dyn Fn(&Name) … + Send + Sync>`: the driver is a
  reusable state machine consulted once per connection, so it needs a shared (`Fn`) policy, which must be
  boxed to keep `ConnectionDriver` a single concrete type.
- **There is ONE status→error mapping, shared.** `handshake::status_error(&Status)` is the single
  source; `connection.rs` and `driver.rs` both delegate to it (their private duplicates were deleted).
  It returns `BeamError` values backed by `std::io::ErrorKind` — no new `BeamError` variant and no
  crate-local error enum; classification is a *value* type (`StatusKind`) on the decoded `Status`.
- **A refusing acceptor notifies the peer; a refusing connecter does not.** `dist_util.erl:286` sends
  `status(not_allowed)` only on the `other_started = true` branch, so both the blocking
  `accept_server_with` and the driver's `ServerAwaitName` state frame+flush `Status::not_allowed()`
  (the exact bytes `00 0C 73 6E 6F 74 5F 61 6C 6C 6F 77 65 64` = `snot_allowed`) before returning the
  error; the connecting side just aborts without a status.
- **Driver handshake errors are terminal.** Every error from `step` funnels through one arm that sets
  `DriverState::Failed` and returns the error; a later `step()` returns `Ok(StepOutcome::Failed)`.
  `WouldBlock` is returned only when more bytes are genuinely needed — never to paper over an error.
- **Node visibility is the per-connection `publish_flag` piece, not a change to `preferred_flags()`.**
  `preferred_flags()` stays `DFLAG_DIST_DEFAULT` (mirroring the emulator's `erts_dflags.default`); a
  caller that wants to advertise `DFLAG_PUBLISHED` ORs in `publish_flag(hidden, published)` via the
  opt-in `*_with_flags` seams. The dynamic-name branch (`DFLAG_NAME_ME`) is deliberately **not**
  modelled — this crate speaks only the static OTP-23 `'N'` name.
- **`epmd`'s interactive/admin replies differ from the node protocol.** `KILL_REQ` answers `"OK"` /
  `"NO"` (`KillResp`), while `STOP_REQ` answers **`"STOPPED"`** / `"NOEXIST"` / no-reply (`StopResp`),
  per `epmd_srv.c`. `DUMP`/`NAMES` replies end at **EOF**, not a read timeout (only a successful
  `ALIVE2_REQ` sets epmd's `keep` flag); the dump body is bounded by `MAX_DUMP_RESP_BODY`.
- **An accepted inbound pid must carry the verbatim handshake node name.** `dist.c:2358` requires
  `external_pid_dist_entry(watcher) == dep`, keyed on **sysname only** (`erl_node_tables.c:286-292`),
  and `dist_util.erl:590,:783-793` stores the verbatim `Name` string — i.e. the **full `node@host`** on
  the wire (a bare short name fails). The creation comes from the `ALIVE2_RESP`
  (`Alive2Resp::creation()`) and is *not* part of the check.
- **`ConnectionDriver` is byte-fed and transport-agnostic; its header parse is deferred to
  `next_message`.** The runtime owns the socket; the driver only parses/serializes, which keeps this
  crate free of `mio` and lets `beam_rt` register the link with `beam_io`'s poller. `step` does a cheap
  *structural* check of each packet's leading bytes so garbage is rejected immediately, but the
  distribution header is decoded (and applied to the receive atom cache) in `next_message`, because a
  `NewAtomCacheRef`'s interned atoms must land in the *same* `AtomTable` the payload decodes against
  (otherwise a payload `ATOM_CACHE_REF` could collide with an inline atom's index).
- **The frame-boundary EOF seam is a *defaulted* `Read` method.** `Read::read_exact_or_eof`
  returns `Ok(false)` only for a clean end of stream *before any byte of the buffer was read*, so it
  can be added to the trait without touching a single implementor (the default delegates to
  `read_exact` and can only return `Ok(true)`/an error). `TcpTransport` overrides it (a `0`-byte
  `read` at the start of the buffer is `Ok(false)`; a partial read is `BadArg`) and `MemoryTransport`
  maps an exhausted input to `Ok(false)`. The blocking `Connection`'s established frame read
  (`read_message_body`, via `framing::read_packet_or_eof`) turns that into
  `BeamError::Io(UnexpectedEof)`, distinct from the `BadArg` of a truncated/malformed frame; the
  handshake reads deliberately keep the plain `read_packet` (`BadArg`) behaviour, and the
  non-blocking `ConnectionDriver` path is unchanged (`feed_eof` → `StepOutcome::Closed`).
- **The `epmd` lookups have address-taking forms.** `lookup_at`/`lookup_port_at(addr, name)` are the
  primitives; `lookup`/`lookup_port` are thin wrappers pinned to `EPMD_DEFAULT_ADDR` (no behaviour
  change). This is what lets a name be resolved against a private daemon (`erl -epmd_port N`) and
  what makes the `Io(NotFound)` translation testable against the hermetic `epmd` stub, which does not
  listen on the well-known address.
- **`NamesResp::encode` is lossy** (per-node ports are not retained) — decoding (the client direction)
  is exact.

## Known Issues / Remaining Gaps
- **The crate classifies inbound deliveries but performs no side effects.** `Connection::recv` /
  `ConnectionDriver::next_message` decode any opcode into a [`ControlMessage`], and
  `Delivery::classify` turns it into the full semantic `Delivery` — but nothing in this crate acts on
  it (no link / monitor / spawn / exit bookkeeping). Routing is the caller's job via the `InboundSink`
  seam (`Connection::recv_delivery`/`dispatch_inbound`, `ConnectionDriver::next_delivery`/
  `dispatch_inbound`), whose **production impl over `beam_process`/`Mailbox` is still owed by
  `beam_rt`** (the defaulted impl is `UnsupportedSink`, which never mis-delivers). Typed *outbound*
  helpers exist for `Send(_Sender)`/`RegSend`/`Link`/`Exit`/`MonitorP`/`DemonitorP`; every other opcode
  is sent through the generic `send_control`.
- **Spawn/altact/payload-exit are classified, not executed.** The `Delivery` variants
  (`SpawnRequest`/`SpawnReply`/`AltactSigSend`/`PayloadExit`/`PayloadExit2`/`PayloadMonitorExit`) and
  the in-term bit vocabularies (`DistSpawnFlags`/`AltactSigFlags`) are fully decoded, but the semantics
  (`dist.c:2590-2698`, `dist.c:3090-3146`) are interpreted by neither this crate nor `beam_process`
  yet — that is `beam_process`/`beam_rt` work.
- **`ConnectionDriver` advances the receive atom cache only in `next_message`**, not `step` (see Design
  Decisions): a runtime that never calls `next_message` will not make header/atom progress.
- **`DOP_UNLINK` never routes to a side effect.** The C *rejects* the bare `DOP_UNLINK` opcode
  (`dist.c:2277-2282`), so `classify` leaves it `Other(DOP_UNLINK)` — encodable/decodable, but carrying
  no semantic `Delivery`.
- **`Delivery::classify` performs no per-opcode type or node-identity validation.** It checks term
  presence + arity (via `decode_control`) and the spawn/altact flag-word constraints, but not the C's
  per-opcode checks: `is_external_pid(from)` + `external_pid_dist_entry(from) == dep` (the peer's dist
  entry), `is_ref(ref)`, `is_atom`/`is_internal_pid` on `watched`/`to` (`dist.c:2224-2441`). `Delivery`
  also carries no peer/connection identity, so an `InboundSink` cannot enforce the `dep` check without
  out-of-band context — a peer may name a `from`/`watcher` pid belonging to a *different* node and it is
  classified without complaint.
- **The accept policy can refuse, but there is no net-kernel pending-connection registry.** Both
  acceptors now consult an **opt-in, defaulted** `AcceptDecision` seam
  (`accept_server_with_decision` / `Connection::server_with_decision` /
  `ConnectionDriver::server_with_decision`): `AcceptDecision::Ok` — the default, byte-identical to the
  previous behaviour — proceeds, and `AcceptDecision::Refuse(status)` writes that status
  (`nok`/`alive`/`not_allowed`) to the peer, flushed, before aborting with `status_error(&status)`.
  What is still missing is the *decision source*: the oracle's `mark_pending` (`dist_util.erl:318-368`)
  selects `ok`/`named`/`ok_simultaneous`/`nok`/`alive` from a `net_kernel`-level pending-connection
  registry, so a simultaneous-connect race or a reconnect-alive probe is only answered as a real node
  would once a caller supplies such a policy. The seam also cannot *continue* on a non-`ok` status:
  `named` (dynamic node names) and `ok_simultaneous` are continuations with no `AcceptDecision`
  representation (and this crate has no dynamic-name support).
- **Consumed by `beam_rt` for the ACCEPTING half only.** `beam_rt/src/dist.rs` (`DistNode`, driven from
  `runtime/scheduler.rs::pump_dist`) derives the node name from `-name`/`-sname` (`NodeName`), registers
  with `epmd` (`ALIVE2_REQ`), binds a listener and completes the `'N'` handshake as **server** via this
  crate's `ConnectionDriver`, then routes the inbound **send family** into local mailboxes through the
  `InboundSink` impl (`LocalDeliverySink`, `beam_rt/src/runtime/scheduler.rs`) — **gated by
  `Runtime::allow_inbound_delivery`, which is default-OFF** (so a node handshakes and decodes inbound
  traffic but routes nothing into a mailbox until a caller enables it; `beam_rt/src/runtime.rs`). What is still missing:
  the **connector half** and the `net_kernel` `{setup,…}`/`{nodeup,…}` protocol; every non-send delivery
  is `DeliveryOutcome::Unsupported`; and the node does **not** advertise `DFLAG_PUBLISHED`/visibility at
  startup (no `publish_flag` caller). The value/connection layer plus this accept-only wiring is the
  whole of the current distribution support.
- **Tick *scheduling* is `net_kernel`'s job.** The `net_ticktime` timeout and the resulting
  `{nodedown,Node}` decision belong to `net_kernel` (Erlang, `lib/kernel/src/net_kernel.erl`), not
  this crate's; this crate only answers an inbound tick.
## Notes for Agents
- **A safe pid/port/ref term builder exists.** `beam_codec::term::{make_external_pid,
  make_external_port, make_external_ref}` are safe public wrappers over `beam_term::thing`'s raw
  writers, so a `#![deny(unsafe_code)]` crate can materialize external pids/ports/refs; the live
  tests (`tests/live_interop.rs`, `tests/live_accept.rs`, `tests/live_driver.rs`) use them. The
  hermetic driver tests (`tests/delivery.rs`, `tests/two_node_loopback.rs`) still place small-int
  stand-ins in pid/ref positions, which compare `==` across heaps.
- **`LOCAL_EXT` (`121`) exists in the sibling `beam_codec`**: `Encoder::finish_local(hash: u32)`
  and `pub fn encode_term_local(Eterm, &AtomTable, hash: u32)` emit
  `[VERSION_MAGIC][LOCAL_EXT (121)][node-hash:4 BE][body]` with a **caller-supplied** node hash (the
  codec neither computes nor verifies it), and the decoder consumes that wrapper. This is the
  `DFLAG_LOCAL_EXT` short-circuit used for same-node ("local") distribution; `beam_dist` does **not**
  currently need it (it is a `dist.c`-level local-delivery optimisation, not part of the remote wire),
  so it is not wired in — reach for it if a local-link path is ever added.
- The `Connection` API keeps the *additive* creation parameter additive: `connect_client` and
  `Connection::client` are unchanged and delegate with `CLIENT_CREATION = 0`.
- **Do not run `cargo fmt` on this crate.** It is intentionally not default-rustfmt-clean; a bulk
  `rustfmt` pass rewrites ~700 unrelated lines. Hand-format to match the surrounding style.
- **A remote send needs `{Name, Node}`, not `{Node, Name}`.** `bif.c:2614-2626` matches the tuple
  arguments in that order, so `{Node, Name} ! Msg` silently delivers nothing — a live test once misread
  this as "the sandbox drops name-addressed sends". There is no sandbox drop; the tuple order was wrong.
- **In a live probe, a read timeout is NOT a peer rejection.** `TcpTransport`'s `DEFAULT_READ_TIMEOUT`
  surfaces as `Io(WouldBlock, errno 11)`: the link is still open and nothing arrived — not a refusal.
  An orderly end-of-link is the `Ok(0)`/`Closed` path.
- **A real node can be pointed at a private `epmd`** with `erl -epmd_port <N>` (`erl_epmd:get_epmd_port/0`
  reads `init:get_argument(epmd_port)`; the client connects to `127.0.0.1`). This is how
  `tests/hermetic_epmd.rs` validates the in-process stub against a real ERTS client without a host
  `epmd`; reach for the same trick whenever epmd behaviour must be tested in isolation.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports (incl. `AcceptDecision` and
  `accept_server_with_decision`), `prelude`, `DIST_VERSION`/`EPMD_PORT`
- `src/flags.rs` → `DFLAG_*` + `DistFlags`, the `DOP_*` opcodes + `Dop`, flag sets, the in-term
  `DIST_SPAWN_FLAG_*`/`DOP_ALTACT_SIG_FLG_*` vocabularies + `DistSpawnFlags`/`AltactSigFlags`,
  handshake/epmd tags
- `src/transport.rs` → the abstract `Read`/`Write`/`Transport` seam (incl. the defaulted
  `read_exact_or_eof` frame-boundary method) + `MemoryTransport` test double
- `src/framing.rs` → 2-/4-byte packet-length framing, tick, `PacketHeaderSize`, and the EOF-aware
  `read_length_or_eof`/`read_packet_or_eof`
- `src/handshake.rs` → `Name`/`Status`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement`
  codecs, `preferred_flags`/`check_mandatory`, the `ChallengeDigest`/`ChallengeSource` seams,
  `StatusKind`/`Status::kind`/`error_kind`/`status_error`, the refusal constructors
  `Status::not_allowed()`/`nok()`/`alive()`, the `AcceptDecision` accept-policy type, and
  `publish_flag`/`publish_type`/`PublishKind`/`PeerVisibility`
- `src/auth.rs` → in-tree MD5, `gen_digest`/`verify`/`gen_challenge`, `Md5Digest`/`SystemChallengeSource`
- `src/dist_header.rs` → the `'D'` header (`N`/`Flags`/`LongAtoms`/`AtomCacheRef`) + the `'E'`
  fragmented-header decode (`FragmentedHeader`)
- `src/fragment.rs` → `'E'`/`'F'` codecs, `split` (sender), `FragmentAssembler` (receiver)
- `src/atom_cache.rs` → `SegmentCache`/`ReceiveAtomCache`/`OutboundAtomCache`, `apply_header`, and the
  `beam_codec::AtomCacheWriter` impl on `OutboundAtomCache` (the outbound `intern` hook)
- `src/control.rs` → the `DOP_*` control-message tuple codec (`encode_control`/`encode_control_cached`)
  + `carries_message` (the 16 message-carrying opcodes)
- `src/delivery.rs` → inbound `DOP_*` delivery: `Delivery` (classify a `ControlMessage` — all 30
  opcodes, incl. `SpawnRequest`/`SpawnReply`/`AltactSigSend`/`PayloadExit*`), `DeliveryOutcome`/
  `Dispatched`, the defaulted `InboundSink` seam + `UnsupportedSink` (routing lives in `beam_rt`)
- `src/connection.rs` → `TcpTransport`/`TcpListener`, the handshake driver (client/server + creation,
  plus the `connect_client_with_flags`/`accept_server_with_flags` overrides and the
  `accept_server_with_decision` accept-policy seam + `Connection::server_with_decision`),
  `connect_to_node` (epmd→TCP→handshake), `Connection` (owning the outbound atom cache) with
  `send_control`/`send_message`/`send_tick`/`recv`/`recv_delivery`/`dispatch_inbound` + the typed
  `send_reg_send`/`send_link`/`send_exit`/`send_monitor_p`/`send_demonitor_p` helpers, and the
  `read_message_body` frame read that reports a clean peer EOF as `Io(UnexpectedEof)` (the
  `TcpTransport` `read_exact_or_eof` override)
- `src/connection_tests.rs` → the `connection` module's in-module unit tests (path-included via
  `#[cfg(test)] #[path] mod tests;` so `connection.rs` stays ~1000 lines)
- `src/driver.rs` → the non-blocking `ConnectionDriver` poll/step state machine (`feed`/`step`/
  `take_output`/`next_message`/`next_delivery`/`dispatch_inbound`/`send_control`/`peer_visibility`) +
  `DriverState`/`StepOutcome` + the `server_with_decision` accept-policy constructor
- `src/epmd.rs` → `epmd` request/response codecs (incl. the `DUMP`/`KILL`/`STOP` admin set +
  `MAX_DUMP_RESP_BODY`), `EpmdClient` over `Transport` (`publish`/`port_please`/`names`/`dump`/`kill`/
  `stop`), and the real TCP client (`connect`, the address-taking `lookup_at`/`lookup_port_at`, and the
  `EPMD_DEFAULT_ADDR`-pinned `lookup`/`lookup_port` wrappers)
- `src/bin/dist_probe.rs` → the hermetic two-node loopback CLI (`dist_probe loopback <report>`; 11
  report kinds — ping/nodes/term + badcookie/nodeid/frag/alias/monitor/regsend_missing/handshake/utf8)
  the differential harness drives as an external command; not part of the library API
- `tests/{constants,auth,handshake,framing,control,delivery,dop_roundtrip,epmd,fragment,atom_cache,connection,driver}.rs`
  → seams
- `tests/common/mod.rs` → the shared real-`erl` spawn/harness (`ErlangNode`, `erl_hostname`, `free_port`,
  `resolve_listen_port`) used by the live tests
- `tests/live_interop.rs` → real-`erl` blocking handshake + `DOP_REG_SEND`/`DOP_SEND`/monitor exchange,
  `DOP_EXIT2`, `DOP_UNLINK_ID`, client-originated `DOP_MONITOR_P`, fragmented >`DIST_FRAGMENT_SIZE` both ways
- `tests/live_accept.rs` → real-`erl` server-side `'N'` handshake + Rust→node→Rust `DOP_REG_SEND` round
  trip (full `node@host` + epmd creation) + published-vs-hidden peer visibility
- `tests/live_accept_blocking.rs` → real-`erl` peer vs the **blocking** `accept_server`/`Connection::server`
  (blocking server handshake + the inbound `Send`/`Link`/`Monitor` signals the peer initiates against our pid)
- `tests/live_driver.rs` → real-`erl` non-blocking `ConnectionDriver` handshake + send
- `tests/two_node_loopback.rs` → hermetic in-process two-node loopback (two named `beam_dist`
  endpoints over a real loopback socket: bidirectional `DOP_REG_SEND`/`DOP_SEND_SENDER`, monitor
  round-trip, ticks, >`DIST_FRAGMENT_SIZE` both ways, an `InboundSink` routing a delivery, a real
  non-blocking-socket `ConnectionDriver` pair, clean-close vs abrupt-drop peer death) — no `erl`/`epmd`
- `tests/two_node_session.rs` → hermetic full inbound signal-family loopback (every two-node-session
  family — `Link`/`UnlinkId`/`DemonitorP`/`Exit(2)`/`PayloadExit(2)`/`GroupLeader`/`AliasSend`/
  `SpawnRequest`/`SpawnReply`/`AltactSigSend` + `_TT` — both directions, with a spy `'D'` header pin)
- `tests/hermetic_epmd.rs` → test-local `epmd` registry stub (ALIVE2/PORT2/NAMES/STOP) + the crate
  client and the address-taking `lookup_at`/`lookup_port_at` hermetically + an `erl -epmd_port <stub>`
  oracle cross-check — no host `epmd`
- `tests/dist_probe_cli.rs` → the `dist_probe` bin's CLI contract (exact stdout + exit 0 for every
  report; byte-identity across runs; exit 2 on a usage error), run via `env!("CARGO_BIN_EXE_dist_probe")`
- `../../../erts/emulator/beam/dist.h` · `dist.c` · `external.c` · `external.h` · `packet_parser.c` →
  authoritative flags/opcodes/header/framing/fragmentation (read-only)
- `../../../erts/doc/guides/erl_dist_protocol.md` + `erl_ext_dist.md` → the wire guides (read-only)
- `../../../lib/kernel/src/dist_util.erl` · `../../../lib/erl_interface/src/connect/ei_connect.c` +
  `ei_connect_int.h` → handshake layouts + `gen_digest`/`gen_challenge` (read-only)
- `../../../lib/erl_interface/src/epmd/*.c` · `../../../erts/epmd/src/epmd_srv.c` → epmd wire (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 2;
  consumes `beam_codec`; adds the `connection` TCP adapter/listener and the non-blocking `driver`).
- `../../CONTEXT.md` → workspace contracts (§1 A.5 distribution wire, §2 frozen term layout, §3 error
  strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.5 — the distribution/ETF wire contract.
- `../beam_codec/CONTEXT.md` → the ETF codec this crate reuses (owns all term encoding and the
  `AtomCacheWriter` hook / `LOCAL_EXT` wrapper this crate builds on).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → the frozen foundations this crate builds on.
- `../beam_rt/CONTEXT.md` → the runtime binary that will consume this crate for node startup.
