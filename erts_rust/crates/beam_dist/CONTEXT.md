# `beam_dist` — Distribution Protocol (v6) + `epmd` Client

## Intent
Owns compatibility contract **A.5** (*distribution/ETF wire*) at the *protocol/value* layer plus the
Stage-0 foundations of Part II §H: the connect/challenge handshake, capability (`DFLAG_*`)
negotiation, control-message (`DOP_*`) framing + term payloads, the on-wire packet-length framing,
the distribution-header envelope, and the `epmd` port-mapper client protocol.

The crate is **pure protocol/value code over an abstract transport** — it never opens a socket.
Actual socket I/O is `beam_io`'s job; `beam_rt` drives the node startup (`-name`/`-sname`) and
supplies a concrete `Transport`. Term encoding/decoding is **never re-implemented here**: every term
crosses the wire through `beam_codec` (ETF). This crate only owns the *distribution-specific* wire:
the flag/opcode vocabulary, the handshake state machine, the length framing and the header envelope.

Dependents: `beam_rt` (node startup / `-name`/`-sname`, `system_info(dist)`), `beam_io` (transport).

Ground truth (read-only, never modified):
- `../../../erts/emulator/beam/dist.h` — authoritative `DFLAG_*` (capability bits) + `enum dop`
  (control opcodes) + the mandatory/default/rejectable flag sets.
- `../../../lib/erl_interface/src/connect/ei_connect.c` + `ei_connect_int.h` — the clean, self-contained
  handshake message layouts (`send_name`/`send_challenge`/`send_challenge_reply`/`send_challenge_ack`/
  `recv_*`, `gen_digest`, `preferred_flags`, the 2-/4-byte handshake framing).
- `../../../lib/erl_interface/src/epmd/epmd_publish.c` + `epmd_port.c` + `epmd/ei_epmd.h` — the `epmd`
  client wire (`ALIVE2_REQ`/`RESP`/`X_RESP`, `PORT2_REQ`/`RESP`, version/protocol constants).
- `../../../erts/epmd/src/epmd_srv.c` + `epmd_int.h` — the authoritative `epmd` server side (request
  layouts, node-type bytes: `77 = 'M'` normal Erlang node, `72 = 'H'` hidden/c-node).
- `../../../erts/emulator/beam/packet_parser.c` + `dist.c` (tick = zero-length packet) — length framing.
- `../../../erts/emulator/beam/external.h` — the distribution-header tags `'D'`/`'E'`/`'F'`/`'H'`/`'R'`/`'N'`.

All byte values, tag bytes and field orders below are **wire-frozen** (A.5): reproduce them exactly.

## Status
**Implemented (Milestone 1).** Every module ships a real, tested core: the frozen
`DFLAG_*`/`DOP_*`/handshake/epmd constant vocabulary, the abstract `Transport` seam with an
in-memory test double, the 2-/4-byte packet framing (with tick = empty packet), the handshake
message codecs (`Name`/`Status`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement`) with a
pluggable digest hook, the `DOP_*` control-message codec (opcode + concatenated `beam_codec` term
bodies), the distribution-header envelope, and the `epmd` request/response codecs + client driver.
The crate is **safe Rust** (`#![deny(unsafe_code)]`). `cargo test -p beam_dist` is green.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_dist::prelude::*`.

### `src/lib.rs`
`pub mod {flags, transport, framing, handshake, dist_header, control, epmd}` + `pub mod prelude`,
flat `pub use` of the items below. Crate lints: `#![deny(unsafe_code)]`, `#![warn(missing_docs)]`.
Crate constants: `pub const DIST_VERSION: u16 = 6`, `pub const DIST_VERSION_5: u16 = 5`,
`pub const EPMD_PORT: u16 = 4369`, `pub const VERSION_MAGIC: u8 = 131` (re-exported alias).

### `src/flags.rs` — wire-frozen vocabulary (values from `dist.h`, `ei_connect_int.h`)
- `pub type DistFlags = u64;` plus a `#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub struct
  DistFlags`… *(the newtype wrapper — see Design Decisions)*: `DistFlags::EMPTY`, `bits() -> u64`,
  `from_bits(u64)`, `contains(DistFlags) -> bool`, `insert`/`remove`, `impl BitOr/BitAnd/BitXor/Not`.
- **Capability bits** (`u64`, one `pub const DFLAG_*`):
  `PUBLISHED=0x01`, `ATOM_CACHE=0x02`, `EXTENDED_REFERENCES=0x04`, `DIST_MONITOR=0x08`,
  `FUN_TAGS=0x10`, `DIST_MONITOR_NAME=0x20`, `HIDDEN_ATOM_CACHE=0x40`, `NEW_FUN_TAGS=0x80`,
  `EXTENDED_PIDS_PORTS=0x100`, `EXPORT_PTR_TAG=0x200`, `BIT_BINARIES=0x400`, `NEW_FLOATS=0x800`,
  `UNICODE_IO=0x1000`, `DIST_HDR_ATOM_CACHE=0x2000`, `SMALL_ATOM_TAGS=0x4000`,
  `ETS_COMPRESSED=0x8000`, `UTF8_ATOMS=0x10000`, `MAP_TAG=0x20000`, `BIG_CREATION=0x40000`,
  `SEND_SENDER=0x80000`, `BIG_SEQTRACE_LABELS=0x100000`, `PENDING_CONNECT=0x200000`,
  `EXIT_PAYLOAD=0x400000`, `FRAGMENTS=0x800000`, `HANDSHAKE_23=0x1000000`, `UNLINK_ID=0x2000000`,
  `MANDATORY_25_DIGEST=0x4000000`, `RESERVED=0xf8000000`, and the high word
  `SPAWN=0x1_0000_0000`, `NAME_ME=0x2_0000_0000`, `V4_NC=0x4_0000_0000`, `ALIAS=0x8_0000_0000`,
  `LOCAL_EXT=0x10_0000_0000`, `ALTACT_SIG=0x20_0000_0000`, `NATIVE_RECORDS=0x40_0000_0000`.
  `DFLAG_DETERMINISTIC = DFLAG_ATOM_CACHE`.
- **Derived sets** (exact ORs from `dist.h`): `DFLAG_DIST_MANDATORY_25`, `DFLAG_DIST_MANDATORY_26`,
  `DFLAG_DIST_MANDATORY`, `MANDATORY_DIST_FLAGS := DFLAG_DIST_MANDATORY`, `DFLAG_DIST_HOPEFULLY`,
  `DFLAG_DIST_DEFAULT`, `DFLAG_DIST_ADDABLE`, `DFLAG_DIST_REJECTABLE`, `DFLAG_DIST_STRICT_ORDER`,
  `TERM_TO_BINARY_DFLAGS`.
- **DOP opcodes**: `pub const DOP_*: u8` for every value in `enum dop` (`LINK=1`, `SEND=2`, `EXIT=3`,
  `UNLINK=4`, `REG_SEND=6`, `GROUP_LEADER=7`, `EXIT2=8`, `SEND_TT=12`, `EXIT_TT=13`, `REG_SEND_TT=16`,
  `EXIT2_TT=18`, `MONITOR_P=19`, `DEMONITOR_P=20`, `MONITOR_P_EXIT=21`, `SEND_SENDER=22`,
  `SEND_SENDER_TT=23`, `PAYLOAD_EXIT=24`, `PAYLOAD_EXIT_TT=25`, `PAYLOAD_EXIT2=26`,
  `PAYLOAD_EXIT2_TT=27`, `PAYLOAD_MONITOR_P_EXIT=28`, `SPAWN_REQUEST=29`, `SPAWN_REQUEST_TT=30`,
  `SPAWN_REPLY=31`, `SPAWN_REPLY_TT=32`, `ALIAS_SEND=33`, `ALIAS_SEND_TT=34`, `UNLINK_ID=35`,
  `UNLINK_ID_ACK=36`, `ALTACT_SIG_SEND=37`) + `pub enum Dop { … }` with
  `Dop::from_u8(u8) -> Option<Dop>`, `as_u8()`, `name() -> &'static str` (from `erts_dop_to_string`),
  `is_tt() -> bool`, `arity() -> usize` (term count after the opcode, from the `{DOP_*, …}` shapes in
  `dist.c`).
- **Handshake/version tags**: `NAME_TAG=b'N'(78)`, `CHALLENGE_TAG=b'N'(78)`, `STATUS_TAG=b's'(115)`,
  `CHALLENGE_REPLY_TAG=b'r'(114)`, `CHALLENGE_ACK_TAG=b'a'(97)`, `COMPLEMENT_TAG=b'c'(99)`;
  `DIST_VER_HIGH=6`, `DIST_VER_LOW=6` (OTP 23+ handshake version, `EI_DIST_6`).
- **epmd tags**: `EPMD_ALIVE2_REQ=b'x'(120)`, `EPMD_ALIVE2_RESP=b'y'(121)`,
  `EPMD_ALIVE2_X_RESP=b'v'(118)`, `EPMD_PORT2_REQ=b'z'(122)`, `EPMD_PORT2_RESP=b'w'(119)`,
  `EPMD_NAMES_REQ=b'n'(110)`; node types `EPMD_NODE_NORMAL=b'M'(77)`, `EPMD_NODE_HIDDEN=b'H'(72)`.
  (`'d'`/`'k'`/`'s'` interactive-client codes are out of scope.)

### `src/transport.rs` — the abstract transport seam (safe)
- `pub trait Read { fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>; fn read_u8(&mut self)
  -> Result<u8>; fn read_be_u16/u32/u64(&mut self) -> Result<u16/u32/u64>; }` (the BE readers have
  default bodies over `read_exact`).
- `pub trait Write { fn write_all(&mut self, buf: &[u8]) -> Result<()>; fn flush(&mut self)
  -> Result<()>; fn write_u8/u16be/u32be/u64be(&mut self, …) -> Result<()>; }` (the BE writers have
  default bodies).
- `pub trait Transport: Read + Write {}` with a blanket `impl<T: Read + Write + ?Sized> Transport for T`.
- `pub struct MemoryTransport` — a full-duplex in-memory test double: `new()`,
  `with_input(Vec<u8>)`, `written() -> &[u8]`, `into_written() -> Vec<u8>`, `input_remaining()`.
  Short reads yield `BeamError::BadArg`.
- **Errors**: transport failure → `BeamError::Io`; a short read / end-of-buffer → `BeamError::BadArg`.

### `src/framing.rs` — packet-length framing (safe)
- `pub enum PacketHeaderSize { Two, Four }` (`bytes() -> usize`, `read`/`write` of the BE length).
- `pub const MAX_PACKET_BODY: usize` — the 4-byte ceiling `u32::MAX`; the 2-byte ceiling `0xffff`.
  An advertised length beyond the negotiated ceiling ⇒ `BeamError::SystemLimit` (never a blind alloc).
- `pub fn read_length<R: Read>(r, hdr) -> Result<usize>`; `pub fn read_packet<R: Read>(r, hdr)
  -> Result<Vec<u8>>`; `pub fn write_packet<W: Write>(w, hdr, body) -> Result<()>`;
  `pub fn write_tick<W: Write>(w, hdr) -> Result<()>`; `pub fn is_tick(body: &[u8]) -> bool`.
- **Tick/ack semantics** (`dist.c`: a tick is a *zero-length* packet): a received empty packet must
  be answered with an empty packet; `read_packet` returns an empty `Vec`, `write_tick` emits the
  length prefix `0`.

### `src/handshake.rs` — the connect/challenge handshake messages (safe)
Each message is a **body codec** (tag + fields), big-endian, framed by the caller via `framing`.
- `pub struct Name { flags: DistFlags, creation: u32, name: Vec<u8> }` — tag `'N'`,
  layout `'N' + flags(8) + creation(4) + namelen(2) + name`.
- `pub enum Status { Ok, Simultaneous, Named { name: Vec<u8>, creation: u32 }, Error(Vec<u8>) }` —
  tag `'s'` + text (`"ok"`/`"sok"`/`"sok_simultaneous"`/`"nok…"`/`"snamed:"+namelen(2)+name+creation(4)`),
  parsed by prefix.
- `pub struct Challenge { flags: DistFlags, challenge: u32, creation: u32, name: Vec<u8> }` — tag `'N'`,
  layout `'N' + flags(8) + challenge(4) + creation(4) + namelen(2) + name`.
- `pub struct ChallengeReply { challenge: u32, digest: [u8; 16] }` — tag `'r'` + challenge(4) + digest(16).
- `pub struct ChallengeAck { digest: [u8; 16] }` — tag `'a'` + digest(16).
- `pub struct Complement { flags_high: u32, creation: u32 }` — tag `'c'` + flags-high(4) + creation(4)
  (the OTP-23 high-flags complement; `(recv_complement)` only validates these fields).
- Every struct: `pub fn encode<W: Write>(&self, w: &mut W) -> Result<()>` and
  `pub fn decode(body: &[u8]) -> Result<Self>`; a wrong/short tag ⇒ `BeamError::BadArg`.
- **Negotiation helpers**: `pub fn preferred_flags() -> DistFlags` (the `DFLAG_*` set a node offers);
  `pub fn check_mandatory(flags: DistFlags) -> Result<DistFlags>` (applies the `MANDATORY_25_DIGEST ⇒
  DIST_MANDATORY_25` rule and rejects a peer missing any of `MANDATORY_DIST_FLAGS`, exactly as
  `recv_challenge` does).
- **Digest hook** (see Known Issues — no crypto dep): `pub trait ChallengeDigest { fn digest(&self,
  cookie: &[u8], challenge: u32) -> [u8; 16]; }`, `pub fn verify_digest(got: &[u8; 16],
  our_challenge: u32, cookie: &[u8], d: &impl ChallengeDigest) -> bool`, and
  `pub trait ChallengeSource { fn next_challenge(&self) -> u32; }` (the challenge nonce generator).
  Message *shapes* are exact and complete; only the MD5 value is pluggable.

### `src/dist_header.rs` — distribution-header envelope (safe)
- Tags: `DIST_HEADER=b'D'(68)`, `DIST_FRAG_HEADER=b'E'(69)`, `DIST_FRAG_CONT=b'F'(70)`,
  `HOPEFUL_DATA=b'H'(72)`, `ATOM_CACHE_REF=b'R'(82)`, `NEW_CACHE=b'N'(78)`;
  header flag `DIST_HEADER_FLAG_ATOM_CACHE=0x01`.
- `pub struct AtomCacheRef { segment: u8, internal: bool, atom: Atom }` —
  `'R' + (segment<<1 | internal) + atom_index`.
- `pub struct DistHeader { flags: u8, cache_refs: Vec<AtomCacheRef> }` — `'D' + len(4) + flags(1) +
  refs`; `encode`/`decode`. (`flags` bit `0x01` = the atom cache is enabled and refs follow.)
- `pub enum DistFragment { Header, Continue }` recognising `'E'`/`'F'` (assembly deferred — see gaps).

### `src/control.rs` — `DOP_*` control-message codec (safe)
- `pub struct ControlMessage { dop: Dop, terms: Vec<Eterm> }` — the decoded form.
- `pub fn encode_control(dop, terms: &[Eterm], atoms: &AtomTable) -> Result<Vec<u8>>` — the control
  body: `dop` byte + the concatenated **body-form** `beam_codec` encodings of each term (no
  `VERSION_MAGIC`; each argument is a `beam_codec::Encoder::new_body(atoms)` image, as a distribution
  control message is a tuple whose version byte is carried once by the enclosing header).
- `pub fn decode_control(body: &[u8], heap: &mut Heap, atoms: &mut AtomTable, cache: Option<&AtomCache>)
  -> Result<ControlMessage>` — reads the opcode, then decodes successive `beam_codec::Decoder::new_body`
  terms into `heap`, resolving `ATOM_CACHE_REF` through `cache` when supplied. Unknown opcode ⇒
  `BeamError::BadArg`; a term-count that disagrees with `Dop::arity()` ⇒ `BeamError::BadArg`.
- Term encoding is **only** `beam_codec` (no duplicated codec). `Heap` is `beam_alloc::Heap` (see the
  Design Decisions dependency edge).

### `src/epmd.rs` — the `epmd` port-mapper client protocol (safe)
All messages are 2-byte-BE length-framed (`framing::PacketHeaderSize::Two`).
- `pub struct Alive2Req { port: u16, node_type: u8, protocol: u8, high_vsn: u16, low_vsn: u16,
  name: Vec<u8>, extra: Vec<u8> }` — `'x' + port(2) + node_type(1) + protocol(1) + high_vsn(2) +
  low_vsn(2) + namelen(2) + name + extralen(2) + extra`. `ALIVE2_REQ` *body* `protocol=0` (tcp/ip),
  `node_type=EPMD_NODE_NORMAL`.
- `pub enum Alive2Resp { Legacy { ok: bool, creation: u16 }, Extended { ok: bool, creation: u32 } }` —
  `'y'(len 4) / 'v'(len 6)`: `tag + result(1) + creation(2|4)`. `EPMD_ALIVE2_X_RESP` ⇒ `Extended`.
- `pub struct Port2Req { name: Vec<u8> }` — `'z' + name`.
- `pub enum Port2Resp { Ok { port: u16, node_type: u8, protocol: u8, high_vsn: u16, low_vsn: u16,
  name: Vec<u8>, extra: Vec<u8> }, Error }` — `'w' + result(1)`; on `result=0` the fields follow
  (`port(2) node_type(1) protocol(1) high_vsn(2) low_vsn(2) namelen(2) name extralen(2) extra`),
  else `Error`.
- `pub struct NamesResp { epmd_port: u32, names: Vec<String> }` — the `NAMES_REQ` reply: 4-byte epmd
  port then `"name <name> at port <port>\n"` lines (parsed to `names`).
- `pub struct EpmdClient<T: Transport> { … }` with `connect`-free constructors over a supplied
  `Transport` and request/response drivers: `publish(&mut self, req: &Alive2Req) -> Result<Alive2Resp>`,
  `port_please(&mut self, name: &[u8]) -> Result<Port2Resp>`,
  `names(&mut self) -> Result<NamesResp>`. Each writes the framed request then reads the framed reply
  through the `Transport`, so the whole client is exercised against `MemoryTransport`.
- Module may be split into `epmd/mod.rs` + `epmd/messages.rs` + `epmd/client.rs` if it approaches
  ~1000 lines.

### `src/prelude.rs`
`pub use` of `flags::*`, `framing::{PacketHeaderSize, read_packet, write_packet, write_tick, is_tick}`,
`handshake::*`, `control::{ControlMessage, encode_control, decode_control}`, `epmd::*`, `DistFlags`,
`Dop`, `Transport`, `MemoryTransport`, `dist_header::*`. (`transport::Read`/`Write` are left explicit —
not glob-exported — to avoid shadowing `std::io::{Read, Write}`.)

## Constraints
- **Safe Rust only** — `#![deny(unsafe_code)]`. No `unsafe` is expected; if one is ever genuinely
  needed it must be confined to a marked module with a `// SAFETY:` rationale (report such a need to
  the parent node first).
- **Errors**: only `beam_common::{Result, BeamError}`. Malformed/short/unknown packets and failed
  handshake validation ⇒ `BadArg`; an advertised length/size beyond a wire ceiling ⇒ `SystemLimit`;
  transport failure ⇒ `Io`. **Never** panic on wire data, never `unwrap()` on decoded bytes.
- **Big-endian** for every multi-byte field. Field order/exact byte counts are wire-frozen.
- **No duplicated term codec** — all term encode/decode goes through `beam_codec`.
- **Bounded allocation**: validate an advertised length against the negotiated header ceiling before
  allocating; reject oversize lengths with `SystemLimit`.
- Files target ~1000 lines; split by responsibility (`flags`, `framing`, `handshake`, `control`,
  `dist_header`, `epmd`) before growing past it.
- Licensing: Apache-2.0; no new external dependencies.

## Test Strategy
- In-module `#[cfg(test)]` unit tests per module; `tests/` integration tests for cross-module seams.
- **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_dist` green,
  plus `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Required test coverage (all hermetic — no sockets, no running `epmd`):
  - `tests/constants.rs` — exact-value pins for a representative (and the derived) `DFLAG_*`/`DOP_*`
    constants, the mandatory/default/rejectable sets, and the handshake/epmd tag bytes.
  - `tests/handshake.rs` — `Name`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement`/`Status`
    encode→decode round-trips and negative (wrong tag / truncated) cases.
  - `tests/framing.rs` — 2-/4-byte framing + the zero-length tick round-trip over `MemoryTransport`.
  - `tests/control.rs` — `DOP_*` control encode→decode round-trips (arity-checked) over a `beam_alloc`
    heap + `AtomTable`; unknown-opcode + truncated-payload negatives.
  - `tests/epmd.rs` — `ALIVE2_REQ`/`ALIVE2_RESP`/`ALIVE2_X_RESP`/`PORT2_REQ`/`PORT2_RESP`/`NAMES`
    byte-exact codec round-trips and `EpmdClient` request/response over `MemoryTransport`.
- The differential harness (`../../tools/differential/`) is the eventual compatibility gate for live
  cross-node behaviour; it is out of scope here (this crate is the *value* layer).

## Design Decisions
- **`DistFlags` newtype + raw `u64` consts.** The frozen representation is a 64-bit word; a newtype
  gives ergonomic `contains`/`BitOr` while the `DFLAG_*` consts stay `u64` so exact value pins are
  trivial. Chosen over `bitflags` (no new dep).
- **Digest is a trait hook, not a dependency.** The v6 challenge digest is `MD5(cookie ++
  decimal(challenge))` (`erts_md5`, `gen_digest`). No crypto crate is available/allowed in M1, so the
  message codecs carry the 16-byte digest **opaquely** and comparison goes through `ChallengeDigest`.
  This keeps the frozen *shape* exact and lets a future `md5`/`sha` dep (or `beam_rt`) fill the value.
- **Transport is a pair of tiny traits (`Read` + `Write`) + a blanket `Transport`.** Simpler and more
  testable than one fat trait; `beam_io`'s socket wrapper implements both. `MemoryTransport` is the
  in-memory full-duplex double used by every test.
- **`beam_dist` depends on `beam_alloc` (a downward, wave-0 edge).** Decoding a control-message term
  requires `beam_codec::Decoder::decode(&mut beam_alloc::Heap, &mut AtomTable)`; naming `Heap` in the
  public `decode_control` signature requires the direct dependency. This is a legitimate downward edge
  (wave 2 → wave 0, acyclic) and avoids duplicating any heap logic. **Reported to the parent node** as
  a new dependency edge.
- **Handshake message codecs are body-only; framing is a separate layer.** Every message type exposes
  `encode(w)`/`decode(body)` with no length prefix; the caller wraps `framing::write_packet`/
  `read_packet`. This keeps the codecs pure and lets the handshake driver choose 2- vs 4-byte framing.
- **Atom-cache emission is deferred.** `beam_codec` M1 never emits `ATOM_CACHE_REF`/`NEW_CACHE`, so
  control messages encode atoms inline; the *decoder* still resolves `ATOM_CACHE_REF` through a
  supplied `AtomCache`, and `dist_header` carries the `'D'` envelope + `'R'` refs.

## Known Issues / Intentional Gaps (M1)
- **Live authentication is not possible end-to-end**: the challenge digest has no in-tree MD5, so the
  handshake cannot authenticate against a real node until a `ChallengeDigest` provider is supplied.
  Message shape/parse/negotiation are exact and fully tested.
- **Atom-cache (segment) encoding deferred**: the `NEW_CACHE` (`'N'`) segment emission and
  `ATOM_CACHE_REF` emission are not implemented (upstream `beam_codec` M1 limitation); decoding refs
  is supported when a cache is present. Documented, not silently wrong.
- **Distribution fragments** (`DFLAG_FRAGMENTS`): the `'E'`/`'F'` tags are recognised; reassembly of
  fragmented payloads is out of M1.
- **Spawn/payload/altact control opcodes** are recognised by value/name/arity but their end-to-end
  handling (and the `ERTS_DOP_ALTACT_SIG_FLG_*` / `ERTS_DIST_SPAWN_FLAG_*` bit vocabularies) is out of
  M1 scope.
- **No socket/epmd-server code** — `beam_io` owns transports; this crate is transport-agnostic.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`, `DIST_VERSION`/`EPMD_PORT`
- `src/flags.rs` → `DFLAG_*` capability bits + `DistFlags`, the `DOP_*` opcodes + `Dop`, mandatory/
  default/rejectable flag sets, handshake + epmd tag bytes, version constants
- `src/transport.rs` → the abstract `Read`/`Write`/`Transport` seam + `MemoryTransport` test double
- `src/framing.rs` → 2-/4-byte packet-length framing, tick (empty packet), `PacketHeaderSize`
- `src/handshake.rs` → `Name`/`Status`/`Challenge`/`ChallengeReply`/`ChallengeAck`/`Complement` codecs,
  `preferred_flags`/`check_mandatory`, the `ChallengeDigest`/`ChallengeSource` hooks
- `src/dist_header.rs` → distribution-header (`'D'`) envelope, `AtomCacheRef`, fragment tags
- `src/control.rs` → `DOP_*` control-message encode/decode (opcode + `beam_codec` term payloads)
- `src/epmd.rs` → `epmd` request/response codecs + `EpmdClient` over `Transport`
- `tests/constants.rs` → exact-value constant pins (oracle-derived)
- `tests/handshake.rs` / `tests/framing.rs` / `tests/control.rs` / `tests/epmd.rs` → seam round-trips
- `../../../erts/emulator/beam/dist.h` · `dist.c` · `packet_parser.c` · `external.h` → authoritative
  flags/opcodes/framing/header tags (read-only)
- `../../../lib/erl_interface/src/connect/ei_connect.c` + `ei_connect_int.h` → handshake message
  layouts + digest + preferred flags (read-only)
- `../../../lib/erl_interface/src/epmd/*.c` + `ei_epmd.h` · `../../../erts/epmd/src/epmd_srv.c` +
  `epmd_int.h` → epmd client/server wire + node-type bytes (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 2;
  consumes `beam_codec`).
- `../../CONTEXT.md` → workspace contracts (§1 A.5 distribution wire, §2 frozen term layout, §3 error
  strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.5 — the distribution/ETF wire contract.
- `../beam_codec/CONTEXT.md` → the ETF codec this crate reuses (owns all term encoding; owns the
  distribution *payload* forms, this crate owns the *header* envelope).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → the frozen foundations this crate builds on.
- `../beam_rt/CONTEXT.md` (once it lands) → consumes this crate for node startup (`-name`/`-sname`)
  and supplies the node identity; `../beam_io/` supplies the concrete `Transport`.
