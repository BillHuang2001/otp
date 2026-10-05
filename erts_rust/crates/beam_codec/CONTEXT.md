# `beam_codec` — External Term Format (ETF) Codec

## Intent
Owns the **External Term Format**: encoding and decoding `term_to_binary`/`binary_to_term`, the
distribution payload/control-term format, and the ETS wire format — i.e. the in-memory half of
compatibility contract **A.5** (workspace `../CONTEXT.md` §1, Part I §A.5; Stage 0 of Part II §H).
Dependents: `beam_dist` (handshake + control/payload terms), `beam_message` (terms crossing the copy
boundary), `beam_ets` (`DFLAG_ETS_COMPRESSED`-style term blobs), `beam_rt` (`erlang:term_to_binary/1`,
`binary_to_term/1`).

Decoding is **allocation-aware**: it writes decoded terms into a caller-supplied
[`beam_alloc::Heap`] and resolves every atom name through a caller-supplied
[`beam_term::AtomTable`], so *one* code path serves distribution, ETS and `term_to_binary`.

Ground truth = legacy C `../../erts/emulator/beam/external.c` (`erts_encode_ext`/`erts_decode_ext`
`enc_term`/`dec_term`), `../../erts/emulator/beam/external.h` (tag values),
`../../erts/emulator/beam/erl_term.h` + `erl_map.h` (term layouts), and the in-tree spec
`../../erts/doc/guides/erl_ext_dist.md`. All tag values and encodings are **wire-frozen**. Read the C;
never modify it.

## Status
**Implemented (Milestone 1).** Every module carries real bodies: the wire-frozen
`tag` constants and the `Tag` enum (`from_byte`/`as_byte`/`name`/`covered`), the
`reader`/`writer` cursors, the distribution `atom_cache`, the `encode`/`decode`
loops, the `term` forwarding layer (thin wrappers over `beam_term::thing` for
float/bignum/flatmap/external identifiers) and the `error` constructors. The required core tag set round-trips (see "Tag
coverage" below); the documented unsupported set (binaries/bit-binary, `COMPRESSED`,
funs, records, `V4_PORT`, `LOCAL`, the distribution header, maps > 32 pairs) returns
`error::unsupported` rather than a wrong term. `src/lib.rs` carries only the
permanent lints: `#![deny(unsafe_code)]`, `#![warn(missing_docs)]` and the single
`#[allow(unsafe_code)] mod term;`. In-module `#[cfg(test)]` tests cover the wire,
term and codec layers; `tests/roundtrip.rs` holds the exhaustive encode→decode
corpus plus the negative/truncation cases, and `tests/ground_truth.rs` holds the
byte-exact `term_to_binary` oracle vectors, each asserted in both directions.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_codec::prelude::*`.

### `src/lib.rs`
`pub mod {tag, reader, writer, atom_cache, encode, decode, error, term}` + `pub mod prelude`,
`pub const VERSION_MAGIC: u8 = 131`.
Crate lints: `#![deny(unsafe_code)]` with exactly **one** `#[allow(unsafe_code)] mod term;` (§ unsafe).

### `src/tag.rs` — wire-frozen tag constants (values from `external.h`)
`VERSION_MAGIC = 131` plus one `pub const <NAME>_EXT: u8` per tag: `SMALL_INTEGER=97`, `INTEGER=98`,
`FLOAT=99`, `ATOM=100`, `REFERENCE=101`, `PORT=102`, `PID=103`, `SMALL_TUPLE=104`, `LARGE_TUPLE=105`,
`NIL=106`, `STRING=107`, `LIST=108`, `BINARY=109`, `SMALL_BIG=110`, `LARGE_BIG=111`, `NEW_FLOAT=70`,
`NEW_REFERENCE=114`, `SMALL_ATOM=115`, `MAP=116`, `NEW_PID=88`, `NEW_PORT=89`, `NEWER_REFERENCE=90`,
`ATOM_UTF8=118`, `SMALL_ATOM_UTF8=119`, `COMPRESSED=80`, `BIT_BINARY=77`, `NEW_FUN=112`, `EXPORT=113`,
`FUN=117`, `V4_PORT=120`, `LOCAL=121`, `ATOM_CACHE_REF=82`, `DIST_HEADER=68`, `NEW_CACHE=78`,
`RECORD=67`.
- `pub enum Tag { … }` — every tag above; `Tag::from_byte(u8) -> Option<Tag>`, `Tag::as_byte() -> u8`,
  `Tag::name() -> &'static str`, `Tag::covered(self) -> bool` (true ⇔ the M1 term layer supports it).

### `src/reader.rs` / `src/writer.rs` — the wire seam
- `pub struct Reader<'a>` — bounds-checked BE cursor: `new(&'a [u8])`, `position()`, `remaining()`,
  `is_empty()`, `u8()`, `u16be()`, `u32be()`, `u64be()`, `bytes(n) -> Result<&'a [u8]>`, `rest()`,
  `peek_u8()`. Any short read ⇒ `BeamError::BadArg` (never a panic, never a silent truncation).
- `pub struct Writer` — growable sink: `new()`, `with_capacity(n)`, `u8(v)`, `u16be(v)`, `u32be(v)`,
  `u64be(v)`, `bytes(&[u8])`, `len()`, `is_empty()`, `as_slice()`, `into_vec()`, `clear()`.

### `src/atom_cache.rs` — distribution atom cache (`erl_ext_dist.md` §Distribution Header)
`pub struct AtomCache` — 8 segments × 256 = 2048 slots: `new()`, `set(index: u8, atom)`,
`get(index: u8) -> Option<Atom>`, `clear()`. The *distribution header* itself (`68`/`78`) is
`beam_dist`'s; `beam_dist` fills this cache and the `Decoder` resolves `ATOM_CACHE_REF` (`82`)
through it (an unset index ⇒ `BadArg`). M1 never *emits* `82`/`78`.

### `src/encode.rs` — the `Encoder` seam
- `pub struct Encoder<'a>`: `new(&'a AtomTable)` (emits `VERSION_MAGIC` first),
  `new_body(&'a AtomTable)` (no version byte — dist payload / ETS), `set_atom_encoding(AtomEncoding)`,
  `encode(Eterm) -> Result<()>`, `as_bytes() -> &[u8]`, `len()`, `is_empty()`, `finish() -> Vec<u8>`.
- `pub enum AtomEncoding { Utf8, Latin1 }` — default `Utf8`: emit `SMALL_ATOM_UTF8_EXT`/`ATOM_UTF8_EXT`
  (exactly what `term_to_binary` emits). `Latin1` emits `SMALL_ATOM_EXT`/`ATOM_EXT` for nodes that did
  not negotiate `DFLAG_UTF8_ATOMS` (used by `beam_dist`).
- Convenience: `pub fn encode_term(Eterm, &AtomTable) -> Result<Vec<u8>>` (with `131`) and
  `pub fn encode_term_body(Eterm, &AtomTable) -> Result<Vec<u8>>` (without).

### `src/decode.rs` — the `Decoder` seam
- `pub struct Decoder<'a>`: `new(&'a [u8]) -> Result<Decoder<'a>>` (requires and consumes
  `VERSION_MAGIC`), `new_body(&'a [u8]) -> Decoder<'a>` (no version byte), `with_atom_cache(&'a AtomCache)`,
  `decode(&mut self, &mut Heap, &mut AtomTable) -> Result<Eterm>`, `position()`, `remaining()`,
  `finish() -> Result<()>` (trailing bytes ⇒ `BadArg`).
- Convenience: `pub fn decode_term(&[u8], &mut Heap, &mut AtomTable) -> Result<Eterm>`.

### `src/term.rs` — thin forwarding layer (the ONLY `unsafe` module; see Design Decisions)
Term inspection and boxed sub-term materialization over `beam_alloc::Heap` that **forwards to**
`beam_term::thing` (float/bignum/flatmap/external-identifier constructors + readers) and
`beam_term::{tuple_size, tuple_element, car, cdr}` (tuple/cons accessors); the boxed interior layout
is owned by `beam_term` (workspace `CONTEXT.md` §2). This module only allocates from
`beam_alloc::Heap`, validates its type/bounds preconditions (mapping a violation to `BeamError`), and
calls the `beam_term` accessor:
- `make_float(&mut Heap, f64) -> Result<Eterm>` / `float_value(Eterm) -> Result<f64>`;
- `make_big(&mut Heap, negative: bool, &[u64]) -> Result<Eterm>` /
  `big_parts(Eterm) -> Result<(bool, &[u64])>` — the slice elements are `ErtsDigit`s (`u64`, one per
  word), least significant first;
- `make_flatmap` / `flatmap_parts`;
- `make_external_pid(&mut Heap, node, number: u32, serial: u32, creation: u32) -> Result<Eterm>` /
  `external_pid_parts -> (node, number, serial, creation)`;
- `make_external_port(&mut Heap, node, number: u64, creation: u32) -> Result<Eterm>` /
  `external_port_parts -> (node, number, creation)`;
- `make_external_ref(&mut Heap, node, ids: &[u32], creation: u32) -> Result<Eterm>` /
  `external_ref_parts -> (node, &[u32] ids, creation)`;
- `pub use beam_term::thing::MAX_REF_IDS` (`ERTS_MAX_REF_NUMBERS`), plus the tuple/tuple-element and
  list accessors the encoder needs.

**Public but not part of the cross-crate contract**: dependents (`beam_bifs`, `beam_ets`,
`beam_message`, `beam_rt`) take their float/bignum/map/identifier constructors and accessors from
`beam_term::thing` directly; this module keeps the codec-internal convenience seam and the guard
constants only.

## Tag coverage (M1)
**Encode + decode + byte-exact round-trip** (the required core set):
`97, 98, 110, 111` (integers; a big that fits a 60-bit small decodes to a small, as in C),
`99 FLOAT_EXT` / `70 NEW_FLOAT_EXT` (decode both; **encode emits `70`**, matching `term_to_binary`),
`100, 115, 118, 119` (all four atom tags decode; encode emits UTF-8 per §AtomEncoding),
`101, 114, 90` (references), `102, 89` (ports), `103, 88` (pids), `104, 105` (tuples), `106` (NIL),
`107` (strings; decode ⇒ list of smalls, encode ⇒ `107` when the list is a byte list of length
≤ 65535, else `108`), `108` (lists, including improper tails), `116` (maps, ≤ `MAP_SMALL_MAP_LIMIT`
= 32 pairs ⇒ flatmap), `82` (atom cache refs, decode only).
**Explicit, documented `unsupported` error (never a wrong encoding):** `80 COMPRESSED` (both
directions), `109 BINARY_EXT` / `77 BIT_BINARY_EXT`, `112 NEW_FUN` / `113 EXPORT` / `117 FUN`,
`67 RECORD`, `120 V4_PORT`, `121 LOCAL`, `68`/`78` (distribution header — `beam_dist`'s), maps with
> 32 pairs (needs the HAMT builder), and any atom name that is not representable in the emitted
encoding.

## Term-layout ownership
The boxed interior layout of floats, bignums, flatmaps and the external pid/port/ref blocks is owned
by `beam_term` (`src/thing.rs`, documented in `../beam_term/CONTEXT.md`); this crate's `term.rs` only
forwards to it. The codec's own layout-relevant policy is the **wire** form of a bignum magnitude —
base-256 bytes (`big_to_bytes`/`bytes_to_big`, `big.c:3405-3490`): full eight bytes per non-leading
digit, the leading digit's significant bytes only; `SMALL_BIG_EXT` carries the byte count while it is
< 256, `LARGE_BIG_EXT` otherwise, and the wire bound is `BIG_ARITY_MAX * sizeof(ErtsDigit)` bytes
(`external.c:6146`).
- `VERSION_MAGIC` is **not** part of a distribution payload that follows a distribution header.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum.
  Malformed/truncated/bad tag/invalid UTF-8 atom/missing cache entry ⇒ `BeamError::BadArg`;
  a size/arity/length bound (`MAX_ARITYVAL`, atom > 255 chars, `BIG_ARITY_MAX`, list/map length
  beyond the encodable field) ⇒ `BeamError::SystemLimit`; heap exhaustion ⇒ `BeamError::Alloc`
  (propagated from `Heap`); **not-yet-supported feature ⇒ `error::unsupported(tag)`**, which returns
  `BeamError::Raise(Raise::error(ExitReason::atom(error::UNSUPPORTED_REASON)))`
  (`UNSUPPORTED_REASON = "unsupported_external_term"`, detectable with `error::is_unsupported`).
  **Never panic, never `unwrap()` runtime data, never silently truncate or mis-encode.**
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `libc` — nothing else.
  Adding a dependency edge requires reporting to the crate-layer node (`../`).
- **No wire-format duplication**: tag values and field widths come from `tag.rs` only; the term layer
  never re-derives a tag from a term layout.
- Files target ~1000 lines; one responsibility per file.
- Licensing Apache-2.0.

## Design Decisions
- **Two layers, one `unsafe` module.** The *wire layer* (`tag`, `reader`, `writer`, `atom_cache`,
  and every byte-level step of `encode`/`decode`) is safe Rust and is where the entire mandated tag
  set is parsed and emitted. The *term layer* (`term.rs`) is a thin forwarding layer whose `unsafe`
  is limited to the `beam_alloc` allocation calls, the `beam_term` accessors (which `beam_term` marks
  `unsafe fn` because the validity of an `Eterm` is a caller invariant and there is no safe way to
  read/write a heap word) and the minimal type/size guards it needs to turn a wrong kind of term into
  a `BeamError` instead of UB. Enforcement is mechanical — `#![deny(unsafe_code)]` plus the single
  `#[allow(unsafe_code)] mod term;` — so the deviation from the crate-layer note "The *safe
  periphery* (ETF codec …) must be safe Rust" is auditable rather than diffuse. That note is accurate
  for the wire layer only and should be narrowed upstream (reported to the parent node).
- **The decoder targets `beam_alloc::Heap` + `beam_term::AtomTable` directly** (no trait, no
  intermediate value tree): the objective fixes this seam and downstream crates need real `Eterm`s.
- **This crate does not own the term layout — `beam_term` (`src/thing.rs`) does.** `term.rs` forwards
  to the heap-agnostic `beam_term::thing` writers/readers and `beam_term::{tuple_size, tuple_element,
  car, cdr}`; it keeps only what it needs for allocation sizing (`beam_term::thing::*_size_words`)
  and the guard constants (`BIG_ARITY_MAX`, `MAP_SMALL_MAP_LIMIT`, `EXTERNAL_THING_HEAD_SIZE`,
  `U32_PER_WORD`). The forwarded layout stays asserted against raw-word tests and the
  `term_to_binary` fixtures.
- **Integer width policy mirrors C** (`external.c:3542-3599`): `< 256 ⇒ 97`, then `98` when the value
  fits a signed 32-bit, else `110` when the magnitude's significant base-256 **byte** count is < 256,
  else `111` — the count is derived from the `ErtsDigit`s exactly as `big_to_bytes` does (full eight
  bytes per non-leading digit, only the leading digit's significant bytes, at least one byte), and
  high zero bytes are stripped first. `110`/`111` decode to a small int whenever the value fits a
  60-bit small, else to a bignum.
- **Atoms are interned, never compared by name**: the decoder interns into the caller's
  `AtomTable` (insertion order = wire order) and the encoder renders names from it. Atom length is
  limited to 255 *characters* (the OTP limit) — longer ⇒ `SystemLimit`.
- **Strings**: `decode` of `107` yields a list of smalls (byte-list semantics); `encode` re-emits
  `107` for a proper byte list of length ≤ 65535. `108` is used once an element is not a byte.
- **Redundant identifier forms decode to one representation**: `101/114/90`, `102/89`, `103/88` all
  decode to the *external* thing layouts; the encoder always emits the modern form
  (`90`/`89`/`88`) with `Len`/`Creation` widths per spec. Mapping a same-node identifier to a local
  internal pid/port is a `beam_dist`/`beam_rt` concern (they own node identity).
- **`VERSION_MAGIC` is handled by the constructor**, not the term loop: `Decoder::new` requires and
  consumes it, `new_body` does not; the same split exists on the `Encoder`. This is what lets the
  same code path serve `term_to_binary` and the distribution payload.

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `tag` (values against `external.h`, `from_byte`/`as_byte`
  round-trip), `reader`/`writer` (BE round-trips, every truncation ⇒ `BadArg`, no panic),
  `atom_cache`, `term` (raw-word assertions that the forwarded `beam_term::thing` layout is
  byte-exact — float, bignum sign/digits, flatmap head/size/keys/vals, external id fields — plus the
  guard/error behaviour), `encode`/`decode` (per-tag round-trips + the
  error mapping table + unsupported tags).
- `tests/roundtrip.rs` — exhaustive `encode → decode → compare` over a property-style corpus
  (boundary integers, nested/improper lists, tuples of every arity boundary, every atom tag, floats
  incl. ±0/±inf/NaN bit patterns, maps of 0/1/32 pairs, pids/ports/refs, bignums of 1/8/9/16/32/...
  `ErtsDigit`s), plus negative cases (truncation at every offset, bad tag, over-long atom, `LARGE_BIG`
  byte count beyond `BIG_ARITY_MAX * 8`, trailing bytes).
- `tests/ground_truth.rs` — **byte-exact ground-truth vectors** produced by the system oracle
  (OTP 29.1.1 / erts-17.1 on `PATH`). Regenerate with:
  `erl -noshell -eval 'io:format("~w~n", [term_to_binary(T)])' -s init stop` — one term per vector;
  paste the printed decimal byte list. Each vector asserts **both** that our decoder yields the
  expected term and that our encoder reproduces the exact bytes. Seeds (verified in-tree):
  `{ok,17}` → `131,104,2,119,2,111,107,97,17`; `[1,2,3]` → `131,107,0,3,1,2,3`;
  `"abc"` → `131,107,0,3,97,98,99`; `#{a=>1}` → `131,116,0,0,0,1,119,1,97,97,1`;
  `3.14` → `131,70,64,9,30,184,81,235,133,31`; `true` → `131,119,4,116,114,117,101`;
  `[]` → `131,106`; `12345678901234567890123` → `131,110,10,0,203,68,66,113,118,78,182,66,157,2`.
  The bignum vectors pin the `ErtsDigit`-per-word layout (both big tags put the
  **count before the sign byte**): `2^32` → `131,110,5,0,0,0,0,0,1`
  (decodes back to a small), `2^64 + 17` → `131,110,9,0,17,0,0,0,0,0,0,0,1` (the low digit keeps
  all eight bytes), `-2^63` → `131,110,8,1,0,0,0,0,0,0,0,128`, and `1 bsl 3000` →
  `131,111,0,0,1,120,0,<375 zero bytes>,1` — a `LARGE_BIG` whose 4-byte count is 376 and whose
  magnitude is 375 zero bytes plus a leading `0x01`.
  (`<<"hi">>` → `131,109,0,0,0,2,104,105` is kept as the documented *unsupported* case.)
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_codec`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch `../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../tools/differential/` harness
  (the compatibility gate), not here.

## Known Issues / Intentional M1 gaps
- **`BINARY_EXT`/`BIT_BINARY_EXT` are not decoded into terms.** A binary term is either an off-heap
  ref-counted `Binary`/`BinRef` (owned by `beam_gc`, a wave-1 sibling the wave DAG forbids this crate
  from depending on) or an on-heap `ErlHeapBits` block; neither constructor exists in the Stage-0
  foundation. The decoder returns `unsupported` rather than a wrong term. **This is the single
  biggest M1 gap and blocks binary payloads in `beam_dist`/`beam_message`/`term_to_binary`** —
  escalated to the parent node (needs a `beam_term`/`beam_gc` binary constructor).
- **Maps are flatmaps only, ≤ 32 pairs** (`MAP_SMALL_MAP_LIMIT`, exactly ERTS's own switchover):
  larger maps need the HAMT builder plus Erlang term hashing (owned by `beam_ets`/`beam_term`), so
  they return `unsupported`. Duplicate keys are **not** detected in M1 (that needs term comparison,
  not available yet) — the wire order is preserved as-is.
- **`COMPRESSED` (80) is unsupported in both directions** (no zlib dependency is available, and a
  wrong encoding must never be emitted). A future `miniz_oxide`/`flate2` (Apache-2.0/MIT) behind a
  feature can fill this in without an API change.
- Funs (`112`/`113`/`117`), records (`67`), `V4_PORT_EXT` (`120`), `LOCAL_EXT` (`121`) and the
  distribution header (`68`/`78`) are out of M1 scope; `beam_dist` owns the distribution header.
- Identifiers always decode as *external* things (no local-node identity in the codec yet).
- **`node` occupies the sysname-atom slot; `creation` is carried in C's off-heap `next` link
  (a documented M1 substitution).** In C `ExternalThing`'s first data word is a pointer to a
  *registered node* (carrying `sysname` + `creation`) and the second is the off-heap `next` link
  (`etp->node = erts_find_or_insert_node(sysname, cre, …)`). This crate has no node registry, so the
  `node` slot holds the sysname atom and the unused `next` slot holds `creation` — yielding **exactly
  C's word positions/arities** (head 3 words, pid/port 4, ref `EXTERNAL_THING_HEAD_SIZE + ref_words/2 + 1`)
  while preserving the round-trip of `creation`. The pointer semantics land when a node-identity type
  exists (owned by `beam_term`/`beam_dist`); only then can the local-node fast path replace the
  external forms.
- **`FLOAT_EXT` (99) is decoded via its `%.20e` string form**; encode always emits `70` (as
  `term_to_binary` does), so a decode↔encode round-trip of an `99`-encoded float is
  representation-preserving, not byte-identical.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`, `VERSION_MAGIC`
- `src/tag.rs` → wire-frozen tag constants + `Tag` enum (`from_byte`/`as_byte`/`name`/`covered`)
- `src/reader.rs` → `Reader<'a>`, the bounds-checked BE cursor (safe)
- `src/writer.rs` → `Writer`, the growable byte sink (safe)
- `src/atom_cache.rs` → `AtomCache` (2048-slot distribution atom cache)
- `src/encode.rs` → `Encoder`, `AtomEncoding`, `encode_term`/`encode_term_body`
- `src/decode.rs` → `Decoder`, `decode_term`
- `src/error.rs` → error constructors only (`BadArg`/`SystemLimit` mapping), `unsupported`,
  `is_unsupported`, `UNSUPPORTED_REASON`
- `src/term.rs` → **thin forwarding layer (the only `unsafe` module)**: allocates from
  `beam_alloc::Heap` and forwards to `beam_term::thing` / `beam_term::{tuple_size, tuple_element,
  car, cdr}` for float/bignum/flatmap/external-identifier construction and inspection
- `tests/ground_truth.rs` → byte-exact `term_to_binary` fixture vectors (oracle-regenerated)
- `tests/roundtrip.rs` → exhaustive encode→decode round-trips + negative/truncation cases
- `../../erts/emulator/beam/external.c` / `external.h` → the authoritative encode/decode + tag values (read-only)
- `../../erts/emulator/beam/erl_term.h` / `erl_map.h` → term layouts for the boxed sub-terms (read-only)
- `../../erts/doc/guides/erl_ext_dist.md` → the in-tree external term format specification (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 1;
  consumes wave 0 only).
- `../../CONTEXT.md` → workspace contracts (§2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.5 — distribution/ETF wire contract.
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → the frozen foundations this crate builds on.
- `../beam_dist/CONTEXT.md` → the distribution consumer (dist header, `DFLAG_*`, handshake).
