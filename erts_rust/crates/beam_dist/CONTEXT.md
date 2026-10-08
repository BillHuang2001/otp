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
(the poll/step state machine). `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
beam_dist` is green (**337 tests + 1 doc-test**) and the workspace builds warning-free; the crate is
safe Rust (`#![deny(unsafe_code)]`).

**All 30 `Dop` opcodes round-trip exhaustively.** `tests/dop_roundtrip.rs` drives every variant through
`encode` -> `decode` -> byte-identical re-encode (the arg table is a wildcard-free `match`, so a new
variant is a compile error) and pins a **byte-exact golden vector per opcode**, all 30 cross-checked
against the system `erl` oracle (`term_to_binary/1` of the equivalent tuple, `VERSION_MAGIC` stripped,
hex-compared). Honest wire facts pinned by the suite: there is **no** `DOP_NODEUP`/`DOP_NODEDOWN` in v6
(node up/down are *local* emulator events, `dist.c:6931` `send_nodes_mon_msgs`), and
`DOP_DEMONITOR_ID`/`DOP_UNLINK_MONITOR_ID`/`DOP_DEMONITOR_MONITOR_ID` **do not exist**; `Dop::from_u8`
rejects `0, 5, 9-11, 14-15, 17, >= 38` and accepts exactly 30. `control::carries_message` (now `pub`)
classifies the 16 opcodes carrying a trailing **Message** term (the 9 send family + 5 payload-exit +
`SPAWN_REQUEST`/`SPAWN_REQUEST_TT`); `SPAWN_REPLY`/`_TT` are excluded (`dist.c:1897` sets
`ctx->msg = THE_NON_VALUE`).

**Live interop (blocking `Connection`, `tests/live_interop.rs`).** Against a real `erl -sname` node
whose port is resolved through **`epmd`** (`epmd::lookup_port`, fallback to the pinned port): the OTP-23
`'N'` handshake completes; a `DOP_REG_SEND` is delivered (the node prints `GOT {hello,42}`) and its
reply is decoded back over the same connection; the node echoes its own pid; the client sends a
`DOP_SEND`/`DOP_SEND_SENDER` to that pid (the node prints `GOT {direct,99}`); and a **monitor
round-trip** completes — the node's `DOP_MONITOR_P` is decoded, the client answers
`DOP_MONITOR_P_EXIT`, and the node prints `DOWN … noproc`.

**Live interop (non-blocking `ConnectionDriver`, `tests/live_driver.rs`).** A genuine non-blocking
`TcpStream` is driven manually (`feed`/`step`/`take_output`/`next_message`) against a real `erl` node:
the handshake completes incrementally and a `DOP_REG_SEND` is delivered (the node prints
`GOT {hello,7,…}`).

The **outbound atom-cache path is wired end-to-end**: `OutboundAtomCache` implements
`beam_codec::AtomCacheWriter`, `encode_control_cached` threads the cache through a single `Encoder` per
control message, and both `Connection::send_payload` and `ConnectionDriver::send_control` emit the real
`OutboundAtomCache::take_header()` header — an atom-bearing message writes `ATOM_CACHE_REF` (`82`) +
index on the payload and teaches/re-uses those refs in the header; an atom-free message emits the exact
`131, 68, 0` header.

**`epmd` over real TCP works** (an earlier "blocked in this sandbox" note was a misdiagnosis — see
Design Decisions): `EpmdClient::connect` / `epmd::lookup_port` / `epmd::lookup` talk to a live `epmd`;
a live test publishes a node (`ALIVE2_REQ`), looks its port up over a *second* connection, and observes
de-registration. `connect_to_node(host, node, our_name, cookie)` chains epmd lookup → TCP connect →
handshake.

The `DOP_*` **flag vocabularies** are complete: `DIST_SPAWN_FLAG_LINK`/`_MONITOR` and
`DOP_ALTACT_SIG_FLG_PRIO`/`_TOKEN`/`_ALIAS`/`_NAME`/`_EXIT` (plus the `DistSpawnFlags`/`AltactSigFlags`
typed helpers), value-pinned from `dist.h:185-192`. End-to-end spawn/altact handling remains out of
scope (see Known Issues).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_dist::prelude::*` (see `src/lib.rs`/`src/prelude.rs`).

### `src/lib.rs`
`pub mod {auth, atom_cache, connection, control, dist_header, driver, epmd, flags, fragment, framing,
handshake, prelude, transport}` + `pub mod prelude`, flat `pub use` of the principal items below.
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
  `CHALLENGE_ACK_TAG`, `COMPLEMENT_TAG`, `DIST_VER_HIGH`/`LOW`), epmd tags/node types.

### `src/transport.rs` — the abstract transport seam (safe)
- `trait Read` / `trait Write` (with default big-endian helpers) + blanket `trait Transport`.
- `struct MemoryTransport` — full-duplex in-memory test double.
- Errors: transport failure → `BeamError::Io`; a short read → `BeamError::BadArg`.

### `src/framing.rs` — packet-length framing (safe)
- `enum PacketHeaderSize { Two, Four }`; `read_length`/`read_packet`/`write_packet`/`write_tick`/
  `is_tick`; `MAX_PACKET_BODY` (`u32::MAX`). A zero-length packet is a **tick**; over-ceiling
  advertised length ⇒ `BeamError::SystemLimit`.

### `src/handshake.rs` — connect/challenge handshake messages (safe)
- Body codecs (tag + fields, no length prefix) for `Name` (`'N'`), `Status` (`'s'`), `Challenge`
  (`'N'`), `ChallengeReply` (`'r'`), `ChallengeAck` (`'a'`), `Complement` (`'c'`) — each
  `encode<W: Write>(&self, w)` / `decode(body)`.
- `preferred_flags()` (= `DFLAG_DIST_DEFAULT`), `check_mandatory(flags)` (the `MANDATORY_25_DIGEST`
  rule + mandatory-set check).
- The authentication seam: `trait ChallengeDigest { fn digest(&self, cookie, challenge) -> [u8;16] }`,
  `trait ChallengeSource { fn next_challenge(&self) -> u32 }`, `verify_digest(got, our_challenge,
  cookie, d)`. The concrete MD5 provider is [`crate::auth::Md5Digest`].

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
- `encode_control_cached(dop, terms, atoms, Option<&mut dyn beam_codec::AtomCacheWriter>) ->
  Result<Vec<u8>>`; `encode_control(dop, terms, atoms) -> Result<Vec<u8>>` (a thin wrapper over the
  cache-less case). The opcode + control args + trailing message are encoded through **one**
  `Encoder::new_body(atoms)` so a supplied cache sees every atom in payload order — which is exactly
  the header ref order.
- `decode_control(body, &mut Heap, &mut AtomTable, Option<&AtomCache>) -> Result<ControlMessage>`.
  Unknown opcode / wrong arity / malformed tuple ⇒ `BeamError::BadArg`. Term encoding is **only**
  `beam_codec`.

### `src/epmd.rs` — `epmd` port-mapper client (safe)
2-byte-BE-framed **requests** (`ALIVE2_REQ`/`RESP`/`X_RESP`, `PORT2_REQ`/`RESP`, `NAMES`) +
`struct EpmdClient<T: Transport>` (`publish`/`port_please`/`names`). **Replies are NOT length-framed**
(`epmd` writes the body straight to the socket) — the client reads them unframed/self-delimiting (see
Design Decisions).
- `EpmdClient<TcpTransport>::connect(addr) -> Result<EpmdClient<TcpTransport>>` and the free
  `epmd::connect(addr)` — a real TCP client to `epmd` (sets `DEFAULT_READ_TIMEOUT` + `TCP_NODELAY`).
- `epmd::lookup(name) -> Result<Port2Resp>` (raw) and `epmd::lookup_port(name) -> Result<u16>`
  (`Port2Resp::Error` ⇒ `BeamError::Io(NotFound)`, never a silent `0`); both connect to
  `EPMD_DEFAULT_ADDR` (`"127.0.0.1:4369"`, pinned to `EPMD_PORT`).
- `NamesResp::encode` is lossy on per-node ports (only `epmd_port` + names are retained).

### `src/connection.rs` — the blocking connection-level API + `TcpStream` adapter (safe)
- `struct TcpTransport` (impls `Read`/`Write`); `connect(addr)` sets a read timeout + `TCP_NODELAY`;
  `from_stream`; `set_read_timeout`/`set_nodelay`/`stream`/`into_inner`; `DEFAULT_READ_TIMEOUT` (10 s).
- `struct TcpListener` (wrapping `std::net::TcpListener`): `bind(addr)`/`local_addr`/`accept` (yields a
  `TcpTransport` with the read timeout + `TCP_NODELAY`)/`set_nonblocking`/`into_inner` — the server side.
- Handshake driver (2-byte framing): `connect_client(t, our_name, cookie) -> Result<Handshake>`,
  `connect_client_with_creation(t, our_name, cookie, creation) -> Result<Handshake>` and
  `accept_server(t, our_name, cookie, creation) -> Result<Handshake>`, where
  `struct Handshake { client_flags, server_flags, negotiated, peer_creation, peer_name, buffered }`.
  `pub const CLIENT_CREATION: u32 = 0` is the default client creation.
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
  via `take_header()`.

### `src/delivery.rs` — inbound `DOP_*` delivery classification + the `InboundSink` seam (safe)
Turns a decoded [`ControlMessage`] into its **semantic** delivery and routes it to the local recipient.
- `enum Delivery { Send{from,to,message,token}, RegSend{from,cookie,name,message,token},
  AliasSend{..}, Link{..}, Exit{..}, Exit2{..}, UnlinkId{id,from,to}, UnlinkIdAck{..},
  Monitor{watcher,watched,reference}, Demonitor{..}, MonitorExit{..}, GroupLeader{from,to},
  Other(Dop) }` + `Delivery::dop()` and `Delivery::classify(&ControlMessage) -> Result<Delivery>`
  (argument order from `dist.c`; a recognised-but-unclassified opcode — spawn/payload-exit/
  altact-sig/`DOP_UNLINK` — is `Other(dop)`, never a substituted send).
- `enum DeliveryOutcome { Delivered, NoSuchProcess, Unsupported, Dropped }`,
  `struct Dispatched { delivery, outcome }`.
- `trait InboundSink { fn deliver(&mut self, &Delivery) -> Result<DeliveryOutcome> }` — **defaulted**
  to `Unsupported` (a runtime with no process registry never mis-delivers); `struct UnsupportedSink`
  is the do-nothing impl. The production impl belongs to `beam_rt` (over `beam_process`/`Mailbox`) —
  keeping the value layer free of the message/process crates (dependency DAG).
- `ConnectionDriver::next_delivery`/`dispatch_inbound` (non-blocking) and
  `Connection::recv_delivery`/`dispatch_inbound` (blocking) are the two entry points.

### `src/driver.rs` — the non-blocking poll/step state machine (safe)
`struct ConnectionDriver` + `enum DriverState { Handshaking, Established, Closed, Failed }` +
`enum StepOutcome { WouldBlock, Handshaking, Established, Progress, Closed, Failed }`. **Byte-fed**: the
runtime owns the socket and does the syscalls; the driver only parses/serializes.
- `client(our_name, cookie, creation)` / `server(our_name, cookie, creation)` (+ `*_with_challenge`),
  `is_client`.
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
  `handshake()` / `ticks_answered()` / `is_eof()`.
- Runtime drive loop: `read → feed → loop{step until WouldBlock} → take_output → write → while
  next_message{ handle }`; `Closed`/`Failed` mean drop the link.

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
- Files target ~1000 lines; split by responsibility. Licensing: Apache-2.0.

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
    `[82, index]` with one.
  - `tests/fragment.rs` — byte-exact `'E'`/`'F'` prefixes, split/reassemble round-trips, negatives.
  - `tests/atom_cache.rs` — segment addressing, `New`/`Cached` classification, header apply, ref
    resolution through `beam_codec`, and `encode_control_cached` driven by a real `OutboundAtomCache`
    (the header it yields resolves the payload the encoder wrote).
  - `tests/connection.rs` — hermetic loopback (real `TcpListener` on `127.0.0.1:0`, server on a
    thread): both handshake roles, a `DOP_*` round-trip, a >`DIST_FRAGMENT_SIZE` message, a tick, a
    wrong-cookie failure that is not a hang, and the **outbound atom cache over a real link** (one
    `New` ref per atom, `82 + index` payload, the peer resolves the names). A spy transport asserts
    `'E'` frames really crossed; the new `TcpListener` path is exercised (`accept` on a thread, plus a
    non-blocking `accept` → `WouldBlock`), and the typed `send_reg_send`/`send_link`/`send_exit`/
    `send_monitor_p` helpers round-trip.
  - `tests/epmd.rs` — the epmd codecs plus **live epmd**: `publish` an `ALIVE2_REQ` on one connection,
    `port_please`/`lookup_port` it over a second, observe de-registration on close, and `connect_to_node`
    end-to-end; the `PORT2_REQ` framing pin (frame length = `1 + name.len()`, the name is the packet
    remainder). Skips loudly when epmd is unreachable.
  - `tests/driver.rs` — the `ConnectionDriver`, hermetic: two drivers shuttling `take_output()` bytes
    (whole-buffer **and** byte-at-a-time), negotiated flags/peer names, an atom-cache round trip
    (`New` then `Cached`), the exact empty header `131,68,0`, a >`DIST_FRAGMENT_SIZE` message both ways
    (`'E'` verified), a tick answered, and malformed/truncated/EOF/send-before-established errors.
  - `tests/live_interop.rs` (blocking) + `tests/live_driver.rs` (non-blocking) — **live interop with a
    real `erl` node.** Each starts `erl -sname <uniq> -setcookie <c> -kernel inet_dist_listen_min P
    inet_dist_listen_max P` (P a free loopback port), resolves P through **`epmd::lookup_port`**
    (falling back to P if epmd is unreachable), and completes the handshake. `live_interop.rs` then
    sends a `DOP_REG_SEND` (the node prints `GOT {hello,42}`), receives the node's `DOP_SEND_SENDER`
    reply, sends a `DOP_SEND` to the node pid the node echoed back (the node prints `GOT {direct,99}`),
    and completes a monitor round-trip (`DOP_MONITOR_P` decoded → `DOP_MONITOR_P_EXIT` → the node prints
    `DOWN … noproc`). `live_driver.rs` drives the `ConnectionDriver` over a genuine non-blocking
    `TcpStream`. The shared spawn/harness lives in `tests/common/mod.rs`. Both skip honestly (clear
    `eprintln`) only when `erl` is absent on `PATH`; never fake a pass.

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
- **epmd frames its requests but not its replies.** A client *request* is 2-byte length-framed
  (`epmd_port.c`), but `epmd` writes every *reply* body straight to the socket with no length prefix
  (`epmd_srv.c` `reply()`); the client reads it unframed/self-delimiting — by tag for `'y'`/`'v'`/`'w'`,
  and "read until the transport goes quiet" for the open-ended `NAMES_RESP` line list. An earlier note
  here that "epmd over raw TCP is blocked in this sandbox" was a misdiagnosis: the old client wrongly
  re-read the reply through `read_packet`, so the first two reply bytes were parsed as a bogus ~30 KB
  length. Requests are unchanged; only the three response readers changed.
- **`ConnectionDriver` is byte-fed and transport-agnostic; its header parse is deferred to
  `next_message`.** The runtime owns the socket; the driver only parses/serializes, which keeps this
  crate free of `mio` and lets `beam_rt` register the link with `beam_io`'s poller. `step` does a cheap
  *structural* check of each packet's leading bytes so garbage is rejected immediately, but the
  distribution header is decoded (and applied to the receive atom cache) in `next_message`, because a
  `NewAtomCacheRef`'s interned atoms must land in the *same* `AtomTable` the payload decodes against
  (otherwise a payload `ATOM_CACHE_REF` could collide with an inline atom's index).
- **`NamesResp::encode` is lossy** (per-node ports are not retained) — decoding (the client direction)
  is exact.

## Known Issues / Remaining Gaps
- **No inbound `DOP_*` has semantic dispatch.** `Connection::recv` and `ConnectionDriver::next_message`
  decode *any* opcode into a [`ControlMessage`] and never act on it — the crate performs no link /
  monitor / spawn / exit side effects, so a runtime must interpret the returned opcode. The only
  opcode-selecting logic is the outbound `DFLAG_SEND_SENDER` → `DOP_SEND_SENDER` vs `DOP_SEND` choice;
  typed send helpers exist for `Send(_Sender)`/`RegSend`/`Link`/`Exit`/`MonitorP`/`DemonitorP` only,
  and every other opcode is reachable solely through the generic `send_control` (all 30 `Dop` variants
  are encodable/decodable; encoding is opcode-agnostic).
- **Stale in-source doc comment (no behavior impact).** `connection.rs::accept_server`'s doc says "an
  outbound `Complement` is sent when `DFLAG_HANDSHAKE_23` was negotiated", but the code correctly sends
  **none** (the module doc and `connect_client_core` are right: the `'c'` `Complement` exists only for
  the legacy `'n'` name, which this crate does not speak).
- **Spawn/payload/altact control opcodes are recognised but not executed.** The full `DOP_*`
  vocabulary and the in-term bit vocabularies (`DistSpawnFlags`/`AltactSigFlags`) are present, and
  `DOP_ALTACT_SIG_SEND`'s variable arity is tolerated at decode, but there is **no** end-to-end
  spawn/altact/payload-exit handling here — that is `beam_process`/`beam_rt` work, and the
  `ERTS_DIST_SPAWN_FLAG_*` / `ERTS_DOP_ALTACT_SIG_FLG_*` semantics (`dist.c:2590-2698`,
  `dist.c:3090-3146`) are documented, not interpreted.
- **`ConnectionDriver` advances the receive atom cache only in `next_message`**, not `step` (see Design
  Decisions): a runtime that never calls `next_message` will not make header/atom progress.
- **Not yet consumed by `beam_rt`**: node startup (`-name`/`-sname`) does not yet wire this crate into
  the runtime binary. This crate is the value/connection layer only.

## Notes for Agents
- **`LOCAL_EXT` (`121`) now exists in the sibling `beam_codec`**: `Encoder::finish_local(hash: u32)`
  and `pub fn encode_term_local(Eterm, &AtomTable, hash: u32)` emit
  `[VERSION_MAGIC][LOCAL_EXT (121)][node-hash:4 BE][body]` with a **caller-supplied** node hash (the
  codec neither computes nor verifies it), and the decoder consumes that wrapper. This is the
  `DFLAG_LOCAL_EXT` short-circuit used for same-node ("local") distribution; `beam_dist` does **not**
  currently need it (it is a `dist.c`-level local-delivery optimisation, not part of the remote wire),
  so it is not wired in — reach for it if a local-link path is ever added.
- The `Connection` API keeps the *additive* creation parameter additive: `connect_client` and
  `Connection::client` are unchanged and delegate with `CLIENT_CREATION = 0`.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`, `DIST_VERSION`/`EPMD_PORT`
- `src/flags.rs` → `DFLAG_*` + `DistFlags`, the `DOP_*` opcodes + `Dop`, flag sets, the in-term
  `DIST_SPAWN_FLAG_*`/`DOP_ALTACT_SIG_FLG_*` vocabularies + `DistSpawnFlags`/`AltactSigFlags`,
  handshake/epmd tags
- `src/transport.rs` → the abstract `Read`/`Write`/`Transport` seam + `MemoryTransport` test double
- `src/framing.rs` → 2-/4-byte packet-length framing, tick, `PacketHeaderSize`
- `src/handshake.rs` → `Name`/`Status`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement`
  codecs, `preferred_flags`/`check_mandatory`, the `ChallengeDigest`/`ChallengeSource` seams
- `src/auth.rs` → in-tree MD5, `gen_digest`/`verify`/`gen_challenge`, `Md5Digest`/`SystemChallengeSource`
- `src/dist_header.rs` → the `'D'` header (`N`/`Flags`/`LongAtoms`/`AtomCacheRef`) + the `'E'`
  fragmented-header decode (`FragmentedHeader`)
- `src/fragment.rs` → `'E'`/`'F'` codecs, `split` (sender), `FragmentAssembler` (receiver)
- `src/atom_cache.rs` → `SegmentCache`/`ReceiveAtomCache`/`OutboundAtomCache`, `apply_header`, and the
  `beam_codec::AtomCacheWriter` impl on `OutboundAtomCache` (the outbound `intern` hook)
- `src/control.rs` → the `DOP_*` control-message tuple codec (`encode_control`/`encode_control_cached`)
- `src/delivery.rs` → inbound `DOP_*` delivery: `Delivery` (classify a `ControlMessage`), the defaulted `InboundSink` seam + `UnsupportedSink` (routing lives in `beam_rt`)
- `src/connection.rs` → `TcpTransport`/`TcpListener`, the handshake driver (client/server + creation),
  `connect_to_node` (epmd→TCP→handshake), `Connection` (owning the outbound atom cache) with
  `send_control`/`send_message`/`send_tick`/`recv` + the typed `send_reg_send`/`send_link`/`send_exit`/
  `send_monitor_p`/`send_demonitor_p` helpers
- `src/connection_tests.rs` → the `connection` module's in-module unit tests (path-included via
  `#[cfg(test)] #[path] mod tests;` so `connection.rs` stays ~1000 lines)
- `src/driver.rs` → the non-blocking `ConnectionDriver` poll/step state machine (`feed`/`step`/
  `take_output`/`next_message`/`send_control`) + `DriverState`/`StepOutcome`
- `src/epmd.rs` → `epmd` request/response codecs, `EpmdClient` over `Transport`, and the real TCP client
  (`connect`/`lookup`/`lookup_port`, `EPMD_DEFAULT_ADDR`)
- `tests/{constants,auth,handshake,framing,control,epmd,fragment,atom_cache,connection,driver}.rs` →
  seams
- `tests/common/mod.rs` → the shared real-`erl` spawn/harness (`ErlangNode`, `erl_hostname`, `free_port`,
  `resolve_listen_port`) used by the live tests
- `tests/live_interop.rs` → real-`erl` blocking handshake + `DOP_REG_SEND`/`DOP_SEND`/monitor exchange
- `tests/live_driver.rs` → real-`erl` non-blocking `ConnectionDriver` handshake + send
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
