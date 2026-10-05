# `beam_dist` — Distribution Protocol (v6) + `epmd` Client

## Intent
Owns compatibility contract **A.5** (*distribution/ETF wire*) at the *protocol/value* layer plus the
Stage-0 foundations of Part II §H: the connect/challenge handshake and its MD5 authentication,
capability (`DFLAG_*`) negotiation, control-message (`DOP_*`) framing + term payloads, message
fragmentation/reassembly, the distribution atom cache, the on-wire packet-length framing, the
distribution-header envelope, and the `epmd` port-mapper client protocol.

The crate is primarily **pure protocol/value code over the abstract [`transport`] seam** — the codecs
never open a socket. The [`connection`] module adds a concrete `std::net::TcpStream` adapter
([`connection::TcpTransport`]) and drives a **live distribution link end-to-end** (handshake → `DOP_*`
exchange), which is what the runtime drives; a live test exercises it against a real `erl` node (see
Test Strategy). The runtime's production transport remains `beam_io`'s job.

Term encoding/decoding is **never re-implemented here**: every term crosses the wire through
`beam_codec` (ETF). This crate owns only the *distribution-specific* wire: the flag/opcode vocabulary,
the handshake state machine, the length framing, the header envelope, fragmentation, and the atom
cache.

Dependents: `beam_rt` (node startup `-name`/`-sname`, `system_info(dist)`), `beam_io` (transport).

Ground truth (read-only, never modified):
- `erts/emulator/beam/dist.h` — authoritative `DFLAG_*` (capability bits) + `enum dop` (control
  opcodes) + the mandatory/default/rejectable flag sets; `ERTS_DIST_FRAGMENT_SIZE`/
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
**Implemented.** `DFLAG_*`/`DOP_*` vocabulary, the abstract transport seam, 2-/4-byte framing,
handshake message codecs + MD5 authentication, the byte-exact distribution header, message
fragmentation/reassembly, the distribution atom cache, the control-message codec, the `epmd` client,
and a `Connection` API that drives a **live handshake + message exchange against a real `erl` node**.
`cargo test -p beam_dist` is green and warning-free. The crate is safe Rust (`#![deny(unsafe_code)]`).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_dist::prelude::*` (see `src/lib.rs`/`src/prelude.rs`).

### `src/lib.rs`
`pub mod {auth, atom_cache, connection, control, dist_header, epmd, flags, fragment, framing,
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

### `src/control.rs` — `DOP_*` control-message codec (safe, wire-exact)
The control message is an ETF **tuple** `SMALL_TUPLE_EXT`/`LARGE_TUPLE_EXT + arity + element_1 …`,
where `element_1` is the opcode as a `SMALL_INTEGER_EXT`; for message-carrying opcodes a single
trailing **Message** term follows the tuple in the same body. No leading `VERSION_MAGIC` (the enclosing
distribution header carries it).
- `struct ControlMessage { dop: Dop, terms: Vec<Eterm> }` (flattened: control args, then the trailing
  message when present).
- `encode_control(dop, terms, atoms) -> Result<Vec<u8>>`; `decode_control(body, &mut Heap,
  &mut AtomTable, Option<&AtomCache>) -> Result<ControlMessage>`. Unknown opcode / wrong arity /
  malformed tuple ⇒ `BeamError::BadArg`. Term encoding is **only** `beam_codec`.

### `src/epmd.rs` — `epmd` port-mapper client (safe)
2-byte-BE-framed `ALIVE2_REQ`/`RESP`/`X_RESP`, `PORT2_REQ`/`RESP`, `NAMES` codecs +
`struct EpmdClient<T: Transport>` (`publish`/`port_please`/`names`).
`NamesResp::encode` is lossy on per-node ports (only `epmd_port` + names are retained).

### `src/connection.rs` — the connection-level API + `TcpStream` adapter (safe)
- `struct TcpTransport` (impls `Read`/`Write`); `connect(addr)` sets a read timeout + `TCP_NODELAY`;
  `from_stream`; `set_read_timeout`/`set_nodelay`/`stream`/`into_inner`; `DEFAULT_READ_TIMEOUT` (10 s).
- Handshake driver (2-byte framing): `connect_client(t, our_name, cookie) -> Result<Handshake>` and
  `accept_server(t, our_name, cookie, creation) -> Result<Handshake>`, where
  `struct Handshake { client_flags, server_flags, negotiated, peer_creation, peer_name, buffered }`.
- Established link (4-byte framing): `struct Connection<T: Transport>` —
  `client(t, name, cookie)` / `server(t, name, cookie, creation)`; `negotiated`/`client_flags`/
  `server_flags`/`peer_name`/`peer_creation`/`handshake`/`ticks_answered`/`into_inner`;
  `send_control(dop, terms, atoms)`, `send_message(from, to, msg, atoms)`, `send_tick()`,
  `recv(&mut Heap, &mut AtomTable) -> Result<ControlMessage>`. `MAX_ESTABLISHED_PACKET_BODY`.

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
- Files target ~1000 lines; split by responsibility (`connection.rs` is ~1000 incl. its in-module
  tests). Licensing: Apache-2.0.

## Test Strategy
- In-module `#[cfg(test)]` unit tests per module + `tests/` integration tests for cross-module seams.
- **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_dist` green;
  plus `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Coverage highlights:
  - `tests/constants.rs` — exact-value pins for `DFLAG_*`/`DOP_*` and the handshake/epmd tags.
  - `tests/auth.rs` + in-module — RFC 1321 KATs; `gen_digest` known answers derived from the system
    oracle (`erl -noshell -eval 'io:format("~s",[binary:encode_hex(erlang:md5(<<"COOKIE",...>>))])'`).
  - `tests/handshake.rs`/`framing.rs`/`control.rs`/`epmd.rs` — round-trips + byte-exact vectors +
    negatives; `control.rs` pins the wire tuple bytes (`DOP_SEND` args `[1,2]` ⇒ `[104,3,97,2,97,1,97,2]`).
  - `tests/fragment.rs` — byte-exact `'E'`/`'F'` prefixes, split/reassemble round-trips, negatives.
  - `tests/atom_cache.rs` — segment addressing, `New`/`Cached` classification, header apply, ref
    resolution through `beam_codec`.
  - `tests/connection.rs` — hermetic loopback (real `TcpListener` on `127.0.0.1:0`, server on a
    thread): both handshake roles, a `DOP_*` round-trip, a >`DIST_FRAGMENT_SIZE` message, a tick, and
    a wrong-cookie failure that is not a hang. A spy transport asserts `'E'` frames really crossed.
  - `tests/live_interop.rs` — **live interop with a real `erl` node.** Starts
    `erl -sname <uniq> -setcookie <c> -kernel inet_dist_listen_min P inet_dist_listen_max P` (P a free
    loopback port; epmd is NOT used — raw TCP to epmd is blocked in this sandbox), connects with
    `Connection::client`, completes the handshake, sends a `DOP_REG_SEND` that the node prints
    (`GOT {hello,42}`), and receives the node's `DOP_SEND_SENDER` reply (`{echo,42}`). Skips honestly
    (clear `eprintln`) only when `erl` is absent on `PATH`; never fakes a pass.

## Design Decisions
- **MD5 is implemented in-tree, not via a dependency.** The digest is one 16-byte hash; `md-5` would
  pull in `digest`/`generic-array`/`typenum`/`block-buffer` and churn the shared `Cargo.lock`. A
  self-contained RFC 1321 implementation (KAT-verified) keeps the crate dependency-light. The
  `ChallengeDigest` seam stays so a future provider can replace it.
- **The digest is `md5(cookie ++ decimal(challenge))` — cookie first.** Ground truth: `dist_util.erl`
  `gen_digest/2` = `erlang:md5([atom_to_list(Cookie)|integer_to_list(Challenge)])` and `ei_connect.c`
  `gen_digest` = `md5(cookie)` then `sprintf("%u", challenge)`. The challenge is rendered as its
  **unsigned decimal** string (no leading zeros / no length prefix / cookie has no NUL). (Note: the
  completing objective phrased this as `md5(Challenge ++ Cookie)` — that is not the on-wire formula;
  the C/Erlang ground truth wins and is what is implemented.)
- **`DistFlags` newtype + raw `u64` consts** (ergonomic `contains`/bitops while value pins stay
  `u64`); chosen over `bitflags` (no new dep).
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
- **Handshake framing is 2-byte; established framing is 4-byte.** The frame read after
  `ChallengeAck` is treated as the OTP-23 `Complement` only when its tag is `'c'`; otherwise it is
  buffered and delivered by the next `recv`.
- **`NamesResp::encode` is lossy** (per-node ports are not retained) — decoding (the client direction)
  is exact.

## Known Issues / Remaining Gaps
- **Outbound payload `ATOM_CACHE_REF` emission is blocked on a sibling `beam_codec` change.**
  `beam_dist` fully classifies/addresses outbound atoms and can produce the payload bytes
  (`OutboundAtomCache::payload_ref_for` → `[82, index]`) and the header refs, but the **encoder** lives
  in `beam_codec` (owned by another agent), whose `Encoder` still writes every atom inline. The
  required hook (escalated to the parent node):
  ```rust
  // in beam_codec:
  pub trait AtomCacheWriter { fn intern(&mut self, atom: Atom, atoms: &AtomTable) -> Option<u8>; }
  impl<'a> Encoder<'a> { pub fn with_atom_cache(self, cache: &'a mut dyn AtomCacheWriter) -> Encoder<'a>; }
  ```
  With it, `Encoder::encode` writes `ATOM_CACHE_REF` (82) + the returned index for cached atoms instead
  of `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT`; `OutboundAtomCache` is the concrete `AtomCacheWriter`.
  Tag `78` (`NEW_CACHE`) is not needed — ERTS's encoder never emits it (new atoms travel in the
  header; the payload only ever writes `82`). The **receive** side is complete regardless.
- **`Connection` advertises client creation `0`** (there is no local node container here). If a peer
  rejects creation 0, a client-creation parameter must be added.
- **epmd over raw TCP is blocked in this sandbox** — verified via CLI (`epmd -names` works) but raw
  `NAMES_REQ`/`PORT2_REQ` over a socket return empty. The `EpmdClient` codecs are exercised against
  `MemoryTransport`; live epmd use is untested here. Use a fixed `inet_dist_listen_*` port instead.
- **Spawn/payload/altact control opcodes** are recognised by value/name/arity but end-to-end handling
  (and the `ERTS_DOP_ALTACT_SIG_FLG_*` / `ERTS_DIST_SPAWN_FLAG_*` bit vocabularies) is not implemented.
- **Not yet consumed by `beam_rt`**: node startup (`-name`/`-sname`) does not yet wire this crate into
  the runtime binary. This crate is the value/connection layer only.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`, `DIST_VERSION`/`EPMD_PORT`
- `src/flags.rs` → `DFLAG_*` + `DistFlags`, the `DOP_*` opcodes + `Dop`, flag sets, handshake/epmd tags
- `src/transport.rs` → the abstract `Read`/`Write`/`Transport` seam + `MemoryTransport` test double
- `src/framing.rs` → 2-/4-byte packet-length framing, tick, `PacketHeaderSize`
- `src/handshake.rs` → `Name`/`Status`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement`
  codecs, `preferred_flags`/`check_mandatory`, the `ChallengeDigest`/`ChallengeSource` seams
- `src/auth.rs` → in-tree MD5, `gen_digest`/`verify`/`gen_challenge`, `Md5Digest`/`SystemChallengeSource`
- `src/dist_header.rs` → the `'D'` header (`N`/`Flags`/`LongAtoms`/`AtomCacheRef`) + the `'E'`
  fragmented-header decode (`FragmentedHeader`)
- `src/fragment.rs` → `'E'`/`'F'` codecs, `split` (sender), `FragmentAssembler` (receiver)
- `src/atom_cache.rs` → `SegmentCache`/`ReceiveAtomCache`/`OutboundAtomCache`, `apply_header`
- `src/control.rs` → the `DOP_*` control-message tuple codec
- `src/connection.rs` → `TcpTransport`, the handshake driver, `Connection`, `send_control`/`recv`
- `src/epmd.rs` → `epmd` request/response codecs + `EpmdClient` over `Transport`
- `tests/{constants,auth,handshake,framing,control,epmd,fragment,atom_cache,connection}.rs` → seams
- `tests/live_interop.rs` → real-`erl` handshake + message exchange (the interop evidence)
- `../../../erts/emulator/beam/dist.h` · `dist.c` · `external.c` · `external.h` · `packet_parser.c` →
  authoritative flags/opcodes/header/framing/fragmentation (read-only)
- `../../../erts/doc/guides/erl_dist_protocol.md` + `erl_ext_dist.md` → the wire guides (read-only)
- `../../../lib/kernel/src/dist_util.erl` · `../../../lib/erl_interface/src/connect/ei_connect.c` +
  `ei_connect_int.h` → handshake layouts + `gen_digest`/`gen_challenge` (read-only)
- `../../../lib/erl_interface/src/epmd/*.c` · `../../../erts/epmd/src/epmd_srv.c` → epmd wire (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 2;
  consumes `beam_codec`; adds the `connection` TCP adapter).
- `../../CONTEXT.md` → workspace contracts (§1 A.5 distribution wire, §2 frozen term layout, §3 error
  strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.5 — the distribution/ETF wire contract.
- `../beam_codec/CONTEXT.md` → the ETF codec this crate reuses (owns all term encoding; the pending
  `AtomCacheWriter` hook lives there).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → the frozen foundations this crate builds on.
- `../beam_rt/CONTEXT.md` → the runtime binary that will consume this crate for node startup.
