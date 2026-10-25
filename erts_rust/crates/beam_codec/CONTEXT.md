# `beam_codec` — External Term Format (ETF) Codec

## Intent
Owns the **External Term Format**: `term_to_binary`/`binary_to_term` encode+decode, the
distribution payload/control-term format and the ETS wire format — the in-memory half of contract
**A.5** (workspace `../../CONTEXT.md` §1, Part I §A.5; Stage 0 of Part II §H).
Dependents: `beam_dist` (handshake/control/payload terms), `beam_message` (terms across the copy
boundary), `beam_ets` (`DFLAG_ETS_COMPRESSED` term blobs), `beam_rt` (`term_to_binary/1`,
`binary_to_term/1`).
Decoding is **allocation-aware**: terms go into a caller-supplied `beam_alloc::Heap` and atoms
resolve through a caller-supplied `beam_term::AtomTable`, so *one* path serves distribution, ETS and
`term_to_binary`.
**Optional host seams** keep it wire-compatible without owning runtime state — `AtomCacheWriter`
(§API Surface), `OffHeapBinaries` (`src/decode/CONTEXT.md` §Off-heap binaries),
`Decoder::with_local_node`/`with_local_node_hash` (same file, §Local node identity) and `ExportGate`
(same file, §`[safe]` decode) — inert when nothing is installed.
Ground truth = the legacy C `../../../erts/emulator/beam/external.c` (`erts_encode_ext`/
`erts_decode_ext`, `enc_term`/`dec_term`) plus `external.h` (tags), `erl_term.h`/`erl_map.h`
(layouts) and `../../../erts/doc/guides/erl_ext_dist.md`; every tag value and encoding is
**wire-frozen** — read the C, never modify it.

## Status
**Implemented.** Every module below carries real bodies: the wire layer (`tag`, `reader`, `writer`,
`atom_cache`, `off_heap`, `compress`, `ref_wire`, `error`), the two state machines (`encode/`,
`decode/`) and the `term/` materialization layer.
Test shape: in-module unit suites + **34 integration binaries** + a doc-test; the per-binary
inventory is [`tests/CONTEXT.md`](./tests/CONTEXT.md)'s — re-run the gate command for the live count.
Differential gate: **no case disagrees with the oracle**; the sole allowlisted capability gap is
`wire/new_fun_ext_encode` (§Known Issues). `../../tools/differential/gate.sh` and its `CONTEXT.md` are
the authority for live per-contract counts (sibling — read-only).

## API Surface (FROZEN — dependents route here)
Signatures only; the wire rules live in `src/decode/CONTEXT.md` / `src/encode/CONTEXT.md`. All
fallible calls return `beam_common::Result<_>`; a violated precondition is `BadArg` (§Constraints).
Re-exported at the crate root and via `beam_codec::prelude::*`.
`src/lib.rs` — modules `tag`, `reader`, `writer`, `atom_cache`, `off_heap`, `compress`, `encode`,
`decode`, `error`, `term`, `ref_wire`, `prelude`; lints `#![deny(unsafe_code)]` (with exactly one
`#[allow(unsafe_code)] pub mod term;`) and `#![warn(missing_docs)]`.
- `tag.rs` — `pub const VERSION_MAGIC: u8 = 131` (re-exported at the crate root), one
  `pub const <NAME>_EXT: u8` per wire-frozen tag (values from `external.h`), and a `pub enum Tag` of
  **37** variants over `from_byte(u8) -> Option<Tag>`, `as_byte()`, `name()`,
  `covered(self) -> bool` (true for **34** ⇔ the term layer supports it; false for `Fun` `117`,
  `NewCache` `78`, `DistHeader` `68`). The code is the authority for the value↔name list.
- `ref_wire.rs` — **public** cross-crate API, the one authority for the internal-ref wire mapping:
  `ref_id_to_numbers(u64) -> [u32; REF_NUMBERS]`, `ref_numbers_to_id(&[u32]) -> u64`,
  `representable_ref_id(&[u32]) -> Option<u64>` (the "fits the internal id slot" decode gate),
  `representable_ref_numbers(u64) -> Result<[u32; REF_NUMBERS]>` (the checked packer —
  `BeamError::SystemLimit` past the id slot) and `REF_MASK`/`REF1_NUM_MASK`/`REF_NUMBERS`. Its bit
  layout, its C citations and the consequences of the 60-bit id slot are in
  `src/decode/CONTEXT.md` §Local node identity.
- `reader.rs`/`writer.rs` — `Reader<'a>`, a bounds-checked big-endian cursor (`position`,
  `remaining`, `is_empty`, `u8`/`u16be`/`u24be`/`u32be`/`u64be`, `bytes(n)`, `rest`, `peek_u8`;
  `u24be` is the 3-byte C `get_int24` used by `ATOM_INTERNAL_REF3`); `Writer`, a growable sink
  (`new`, `with_capacity`, `u8`/`u16be`/`u32be`/`u64be`, `bytes`, `len`, `is_empty`, `as_slice`,
  `into_vec`, `clear`) plus two crate-internal backward writes: the bounds-checked
  `patch_u32be(pos, value) -> Result<()>` (patching the `NEW_FUN_EXT` size field) and
  `mask_last_byte(mask)` (clearing a bit-string tail's unused low bits in place). Both are safe
  Rust.
- `atom_cache.rs` — read `AtomCache` (8 segments × 256 = 2048 slots; `set(index, atom)`,
  `get(index) -> Option<Atom>`, `clear`), filled by `beam_dist` and used to resolve
  `ATOM_CACHE_REF` (`82`; unset index ⇒ `BadArg`); write `pub trait AtomCacheWriter { fn intern(&mut
  self, atom: Atom, atoms: &AtomTable) -> Option<u8>; }` (installed with `Encoder::with_atom_cache`;
  C `enc_atom`, `external.c:2877`; implemented by `beam_dist::OutboundAtomCache`). The distribution
  header (`68`/`78`) is `beam_dist`'s.
- `off_heap.rs` — `pub trait OffHeapBinaries { fn alloc_refc_payload(&mut self, bytes: &[u8]) ->
  Result<*mut Eterm>; }`, the host hook that puts a decoded large binary's payload off the process
  heap.
- `compress.rs` — a delegation layer over **`beam_deflate`** (the workspace's safe-Rust port of the
  zlib 1.3.2 the repository vendors at `../../../erts/emulator/zlib/`, the version the oracle's
  `beam.smp` links), so every `COMPRESSED` (`80`) stream is **byte-identical to the oracle's** for
  the same body and level. It owns only the RFC-1950 plumbing: `compress(data, level: u8)` (level
  `MIN_LEVEL`..=`MAX_LEVEL` = `0..=9`, else `BadArg` — the C's `0 =< Level < 10`; the C's
  `Z_DEFAULT_COMPRESSION` (`-1`) is mapped to level 6 by the caller, byte-identical because
  `deflateInit(-1)` and `deflateInit(6)` emit the same stream); `decompress(data)` (any zlib error ⇒
  `BadArg`; output capped at `MAX_UNCOMPRESSED` = 32 MiB — the *declaration-free* helper, **not** the
  decode path's entry point); `decompress_bounded(data, limit)` (refuses to inflate past `limit`;
  the decode path passes the image's declared inner size, C `binary2term_uncomp_size`,
  `external.c:1768-1781`); `decompress_bounded_used(data, limit) -> Result<(Vec<u8>, usize)>` (the
  same bound plus the **input** bytes consumed, trailing junk excluded); `MIN_LEVEL`/`MAX_LEVEL`/
  `MAX_UNCOMPRESSED` and `reject_bad_level`.
  **The `COMPRESSED` wrapper rule** (the encode side's standing contract,
  `Encoder::finish_compressed`/`compress_if_smaller`, C `external.c:2276-2298`): with `body` = the
  plain image's length − 1 (the `VERSION_MAGIC` byte excluded) and `Z` = the zlib stream length over
  exactly those `body` bytes, the image is `[131][80][body:4 BE][zlib]` — `6 + Z` bytes — **iff
  `Z <= body - 5`**. Equality is COMPRESSED (the C compresses into a `body_len - 5`-byte destination
  and keeps the plain image only on `Z_BUF_ERROR`, which equality does not raise); `body < 5` can
  never fit the wrapper and stays plain; and **level 0 never compresses** (`compress_if_smaller`
  returns `None` without calling the compressor, so `[{compressed,0}]` yields the plain image
  exactly). The wrapper is top-level only and `encode_term` is never compressed; the derive-by-test
  witnesses are `tests/compressed_bytes.rs` (§Test Strategy).
- `encode/` (`mod.rs` + `map.rs` + `fun.rs` + `record.rs` + `float.rs`) — `pub struct Encoder<'a>`:
  `new(&'a AtomTable)` (emits `VERSION_MAGIC`), `new_body` (no version byte — dist payload/ETS),
  `set_atom_encoding`/`with_minor_version`, `with_atom_cache`, `with_local_node(node: Atom,
  creation: u32)`, `with_local_ext()`/`set_local_ext(bool)` (the `[local]`/ETS internal-node marker
  form), `with_deterministic()`/`set_deterministic(bool)` (full-term-order-sorted map keys),
  `with_fun_identity`, `encode(Eterm) -> Result<()>`, `as_bytes`/`len`/`is_empty`,
  `finish() -> Vec<u8>`, `finish_compressed(level)`, `finish_local(hash: u32)`.
  `pub enum AtomEncoding { Utf8, Latin1 }` (default `Utf8`; `Latin1` for nodes without
  `DFLAG_UTF8_ATOMS`; ignored under a non-`V2` flavour) and `pub enum MinorVersion { V0, V1, V2 }`
  (default `V2`). `pub struct FunIdentity { module: Atom, uniq: [u8; 16], old_index: u32, old_uniq:
  u32, creator: FunPid, function: Option<Atom> }` (+ `new`/`named`/`with_function`) — the
  `ErlFunEntry` fields a `make_fun3` fun lacks but `NEW_FUN_EXT` needs; `function` is
  introspection-only and never encoded. Convenience: `encode_term` (with `131`) / `encode_term_body`
  (without), both **uncompressed**, plus `encode_term_compressed`, `encode_term_body_compressed`,
  `encode_term_local(term, atoms, hash)` and `encode_term_with_minor_version`.
  Per-format detail (flavours, floats, integer widths, internal ids, funs, records, map order):
  `src/encode/CONTEXT.md`.
- `decode/` (`mod.rs` + `fun.rs` + `record.rs` + `local_node.rs` + `options.rs` + `export_gate.rs`) —
  `pub struct Decoder<'a>`: `new(&'a [u8]) -> Result<Decoder>` (requires+consumes `VERSION_MAGIC`),
  `new_body`, `with_atom_cache`, `with_off_heap(host)`, `with_local_node(node, creation)` /
  `with_local_node_hash(hash)`, `with_safe()`/`set_safe(bool)`, `with_export_gate(&dyn ExportGate)`,
  `decode(&mut Heap, &mut AtomTable) -> Result<Eterm>`, `position`, `remaining`,
  `finish() -> Result<()>` (trailing bytes ⇒ `BadArg`). Free entry points: `decode_term` (identity-
  free/inert), `decode_term_with_local_node(bytes, heap, atoms, node, creation, local_hash:
  Option<u32>)` (identity-carrying on the plain **and** `COMPRESSED` path), the `[used]`-count twins
  `decode_term_used` / `decode_term_with_local_node_used` (they do **not** call `finish()`, so
  trailing bytes are tolerated), `is_compressed_image` / `is_compressed_body`, the body-form twins
  `decode_term_body` / `decode_term_body_used`, and `DecodeOptions { safe, local_node, local_hash }`
  with `decode_term_with_options[_used]` / `decode_term_with_options_and_gate[_used]`.
  `pub trait ExportGate { fn is_export_active(&self, module: &str, function: &str, arity: u32) ->
  bool; }`. `MAX_DECODE_DEPTH`. Per-rule detail: `src/decode/CONTEXT.md` (the C's two-pass bound
  contract, legacy identifier forms, scalar rules, the depth bound, D1–D7 identity rules, `[safe]`,
  `[used]`, off-heap binaries, map canonicalization).
- `term/` (`mod.rs` + `fun.rs` + `binary.rs` + `record.rs` + `ref.rs`) — the materialization layer
  and the **only** `unsafe` subtree: heap-agnostic writers/readers forwarding to
  `beam_term::{thing,fun,bitstring,refc,record,hashing}`, plus the guard constants
  (`BIG_ARITY_MAX`, `EXTERNAL_THING_HEAD_SIZE`, `U32_PER_WORD`). The crate root re-exports the fun
  accessors (`fun_arity_term`/`fun_true_arity_term`/`fun_env`/`fun_module`/`fun_index`/
  `fun_creator_pid`/`fun_kind_term`/… , `FunKind`, `FunPid`, `FUN_DESC_WORDS_EXTERNAL`,
  `FUN_MAX_FIELD`, `MAX_ARG`), the internal-ref pair `make_internal_ref`/`internal_ref_id`, and the
  `beam_term::fun` helpers. Detail: `src/term/CONTEXT.md`.
- `error.rs` — the crate-internal `bad_arg()`/`system_limit()` constructors over
  `beam_common::BeamError`, plus `UNSUPPORTED_REASON = "unsupported_external_term"` with
  `unsupported(tag)`/`is_unsupported`, and the distinct
  `LOCAL_FUN_REASON = "unsupported_local_fun"` with
  `unsupported_local_fun()`/`is_unsupported_local_fun` (kept separate so a caller can tell "no
  identity to encode" from "tag not implemented").

## The tag set: producible, decode-only, rejected
**Producible (encode + decode + byte-exact round-trip)** — the whole set the term layer supports:
integers `97`/`98`/`110`/`111`; floats `70` (`99` only under `MinorVersion::V0`); atoms
`100`/`115`/`118`/`119`; references `90`/`101`/`114`; ports `120` (encode always `V4_PORT_EXT`;
`102`/`89` decode-only); pids `88` (encode always `NEW_PID_EXT`; `103` decode-only); tuples
`104`/`105`; `106` NIL; lists `107`/`108`; binaries/bit-binaries `109`/`77`; maps `116` of any pair
count; atom-cache refs `82`; `80` `COMPRESSED` (wrapper rule above); `121` `LOCAL_EXT`; `67`
`RECORD_EXT`; `112`/`113` functions.
**No recognized tag is left `unsupported`** — `unsupported` remains only for a *term kind* the wire
cannot carry: an unrepresentable atom name, a `FunKind::Local` on **encode** with no `FunIdentity`,
and an internal pid/port/ref immediate with no local node identity.
**Decode-only, never emitted here**: `ATOM_INTERNAL_REF2` (`73`) / `ATOM_INTERNAL_REF3` (`75`) — the
C writes them only in its ETS/distribution internal-tags mode (`beam_ets`/`beam_dist`).
**Rejected as malformed `BadArg`**, exactly as the C: `68 DIST_HEADER`, `117 FUN` (removed OTP 23),
`78 NEW_CACHE` (dead `#if 0`).
The internal-ref family (`74`/`76`) and the transport tags (`69`/`70`/`72`) are **not modelled
here** — they belong to `beam_dist`/`beam_ets`, and a plain image is `BadArg`.
`src/decode/CONTEXT.md` §Tag-byte dispositions carries the per-byte reasoning, the collisions
(`'F'`=70, `'N'`=78, `'C'`=67, `'p'`=112 each mean two things positionally) and the C citations;
`src/tag.rs` is the authority for the values themselves.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum, and **never
  panic, never `unwrap()` runtime data, never silently truncate or mis-encode**. The class depends
  on the direction:
  - **Decode** (`binary_to_term`): malformed/truncated input, an unknown/obsolete tag, every input
    rule the children state, and **every length/arity/count bound** (`LARGE_TUPLE` arity >
    `MAX_ARITYVAL`, `LARGE_BIG` byte count > `BIG_ARITY_MAX * 8`, `NEWER_REFERENCE` id count >
    `MAX_REF_IDS`, `RECORD_EXT` field count, a `MAP_EXT` bit-31 count or duplicate key, a lying
    `STRING`/`LIST`/`BINARY` length) ⇒ `BeamError::BadArg` — the C routes every decode failure,
    bounds included, to `B2TBadArg` (`external.c:1989-1991`). The **only** decode-side
    `SystemLimit` is a nesting depth past `MAX_DECODE_DEPTH`.
  - **Encode** (`term_to_binary`): a value beyond a wire bound (an atom name > 255 characters, a
    tuple/flatmap arity past the encodable field, an internal id that does not fit its field, a
    `>u32` image length) ⇒ `BeamError::SystemLimit`.
  - Either direction: heap exhaustion ⇒ `BeamError::Alloc` (from `Heap`); not-yet-supported ⇒
    `error::unsupported(tag)` (`BeamError::Raise(Raise::error(ExitReason::atom(
    UNSUPPORTED_REASON)))`, detectable via `error::is_unsupported`).
- **Deps:** `beam_common`, `beam_term`, `beam_alloc`, **`beam_deflate`** (which declares nothing
  beyond `beam_common`) and `libc` — so the crate carries **no third-party crate** in its dependency
  tree beyond `libc`, and its `COMPRESSED` stream is the oracle's byte for byte. Any *new*
  dependency edge requires reporting to the crate-layer node (`../`).
- **No wire-format duplication**: tag values and field widths come from `tag.rs` only, and the term
  layer never re-derives a tag from a term layout.
- **`Encoder::new_body` pre-reserves `DEFAULT_IMAGE_CAPACITY = 128` bytes** (`src/encode/mod.rs`) —
  an upper bound only, so it can never change a byte; the cost is that a tiny image's `Vec` carries
  unused capacity. The byte-aligned bit-string path appends straight into the sink (no scratch
  `Vec`).
- Files target ~1000 lines with one responsibility each; licensing Apache-2.0; every `*.rs` file
  carries the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block.

## Design Decisions
- **Two layers, one `unsafe` module.** The *wire layer* (`tag`, `reader`, `writer`, `atom_cache`,
  `off_heap`, `compress`, `ref_wire`, every byte-level `encode`/`decode` step) is safe Rust; the
  *term layer* (`src/term/`) forwards to `beam_term` with `unsafe` limited to `beam_alloc`
  allocation calls, the `beam_term` accessors (an `Eterm`'s validity is a caller invariant) and the
  minimal type/size guards that turn a wrong-kind term into a `BeamError` instead of UB — enforced
  mechanically by the lint pair.
- **This crate does not own the term layout — `beam_term` does.** The decoder writes straight into
  the caller's `Heap` + `AtomTable` (no trait, no intermediate value tree), adding only the wire
  rules plus the checks ERTS applies at the same seam.
- **Decode canonicalizes, encode does not.** A decoded map is rebuilt through the canonical ERTS
  constructor, so it re-encodes byte-identically; the encoder never deduplicates, re-canonicalizes or
  sorts a map unless `[deterministic]` is asked, because ERTS emits the representation's own order.
- **The returned `Eterm` belongs to the caller's heap — the codec keeps no term** (no static/thread-
  local/`RefCell` store), so a decoded word stays valid exactly as long as the caller's `Heap` — a
  caller-side lifetime contract, not a codec behaviour.
- **Every host seam is optional and belongs to the host, not the codec** — a decoded term outlives
  the decoder, an off-heap payload's reclamation follows the owning process heap, and node identity
  is the runtime's: the same "the codec owns the wire, the host owns the state" split for
  `AtomCacheWriter`, `OffHeapBinaries`, `Decoder::with_local_node[_hash]` and `ExportGate`.
- **Nesting is bounded; length is not** (`MAX_DECODE_DEPTH`, `src/decode/CONTEXT.md`).
- **The interop harness compares terms structurally, not with `beam_term::compare::eq`** (whose
  fallback for maps/identifiers compares the raw heap word): `terms_equal` (`tests/interop.rs`,
  `tests/property_support/compare.rs`) walks the structure through the safe `beam_codec::term`
  accessors (atoms by name, floats by bits, bitstrings bit-by-bit) and never compares bytes across
  the oracle boundary, since map key order and the `FLOAT_EXT`→`NEW_FLOAT_EXT` rewrite are not
  byte-canonical.
- **Codec cost is pinned deterministically, not by wall clock.** `tests/alloc_count.rs` asserts the
  exact allocation counts for the bench payload; `examples/etf_attrib.rs` prints timings for a human
  only. The codec is ≈ 28 % of the `etf_roundtrip` op; `ETF_COST.md` decomposes the rest.

## Test Strategy
- **Unit:** `#[cfg(test)] mod tests` in every source module — `tag` (values vs `external.h`,
  `from_byte`/`as_byte`, `covered`), `reader`/`writer` (BE round-trips, every truncation ⇒
  `BadArg`), `atom_cache`, `off_heap`, `compress` (levels 0..=9, out-of-range, bad
  header/truncation/corrupt trailer, the bounded/consumed-count rules, with/without trailing junk),
  `ref_wire` (C goldens, word boundaries, the typed refusal), `local_node` (D2/D4/D6 + the
  `MAX_INTERNAL_*` constants), `term` (raw-word layout assertions over the forwarded writers); the
  state-machine suites are `src/encode/tests.rs`/`src/decode/tests.rs`, exposing the `pub(crate)`
  helpers (`new_heap`, `decode`, `assert_bad_arg`, `assert_system_limit`) the `fun`/`record`
  submodules reuse.
- **Integration:** 34 binaries in six roles — public-API/re-export (`ref_wire_public`),
  property/fuzz (`property_roundtrip`/`property_oracle` + `property_support/`, splitmix64 seed
  `0x5EED_F00D_1234_5678`), hardening (`decode_hostile`, `decode_depth`), identity/`[safe]` seams
  (`local_node`, `local_identity_roundtrip`, `export_gate`, `export_arity_oracle`), deterministic
  attribution (`alloc_count` + `attrib_support/`: **1** allocation per encode, **13** fresh / **3**
  warm per decode, image 101 bytes — counts, never timings), per-tag suites (`tag_coverage`'s 41-row
  coverage + 66-row exact-`BeamError` tables, `tag_space_audit`'s exhaustive 256-row disposition
  sweep + no-panic `[131,byte]` sweep, and the roundtrip/scalar/legacy/record/map/`COMPRESSED`
  families) — plus `interop`, the live differential harness. Inventory and per-suite roles:
  [`tests/CONTEXT.md`](./tests/CONTEXT.md).
- Oracle-dependent cases resolve `$ERTS_ORACLE` else `erl` on `$PATH` and **skip loudly** (a stderr
  note, never a silent pass).
- `examples/etf_attrib.rs` — the manually-run codec-**alone** attribution binary; only
  `alloc_count.rs` pins numbers.
- Gate (must stay GREEN): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p
  beam_codec --no-fail-fast`, and `... build --workspace`. Always go through
  `erts_rust/scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch `../../tools/`.
- **Remaining test gaps:** `BeamError::Alloc` (heap exhaustion) is not asserted as a distinct class;
  the write-side `[deterministic]`/`[local]` flags and the read-side `[safe]` flag have no
  BIF-level suite here (`beam_bifs` covers them — sibling node).

## Known Issues / Remaining gaps
- **A `FunKind::Local` (`make_fun3`) fun encodes only with a caller-supplied `FunIdentity`** —
  without one the encoder returns the typed `error::unsupported_local_fun`, never a mis-encoded
  term. This is a **host** property, not a missing wire rule: a genuine module closure whose module
  is loaded from a `.beam` encodes byte-identically (`tests/local_fun_encode.rs`), and so does
  `erl_eval`'s own closure when the real `erl_eval` creates it. The one term no identity derivation
  can rescue is a closure the **runtime synthesized itself** (no module declares its
  `(uniq, index)`) — the `-eval` lambda behind the gate's single allowlisted `unsupported` case
  `wire/new_fun_ext_encode`, whose identity/`num_free`/`env` are `erl_eval` internals only a full
  `erl_eval`/`erl_parse` port could derive faithfully (fabricating them is forbidden).
- **The > 32-pair *flat*-map writer is crate-local** (`src/term/mod.rs::write_large_flatmap`) and
  stays outside `beam_term` because `beam_term::thing::write_flatmap` asserts
  `MAP_SMALL_MAP_LIMIT` and `beam_term::map::write_map` would produce a *hash* map (a different term
  kind), so no LCA producer exists for a > 32-pair flat map. It is pinned word-for-word to
  `beam_term::thing::write_flatmap` by `oversized_flatmap_layout_matches_beam_term`, preserves pair
  order and does not detect duplicate keys (the decoder does). Detail: `src/term/CONTEXT.md`.
- **A foreign local reference past the M1 id slot keeps its external form** — the frozen internal
  reference stores its id in one 60-bit small, so an arbitrary oracle `make_ref()` (whose packed id
  can exceed `MAX_SMALL`) is demoted only when representable; otherwise it stays the faithful
  external reference (never truncated, never an error). Widening the slot is a `beam_term` layout
  decision. Detail + the `REF1_NUM_MASK` bit loss: `src/decode/CONTEXT.md` §Known Issues.
- **`NEW_FUN_EXT` `old_index`/`old_uniq` are kept verbatim from the wire**, where the C re-emits its
  fun-entry stub's fields (observable only through a re-encode of such a fun). **OWNER: the runtime
  fun-entry table** (`beam_term`/the compiling runtime). Acceptance is not affected — the C never
  requires the module to be loaded or the index to exist, and neither side does.
- **Off-heap binary decode is opt-in**: `beam_bifs`'s `decode_term_bif` installs no
  `OffHeapBinaries` host, so a `binary_to_term/1` result is always on-heap; the host that owns
  `RefcBinary` reclamation (`beam_rt` + `beam_gc::OffHeap`) still has to install it.
- **The `LOCAL_EXT` node hash is installed by no host** (`DecodeOptions.local_hash` stays `None`),
  so the D4 hash check is inert at runtime — a `beam_rt`/`beam_bifs` call.
- **`decode_term`'s *plain* path is deliberately stricter than the oracle on trailing bytes** (it
  calls `finish()`), while `binary_to_term/1,2` (via `Decoder::decode` in `beam_bifs`) and
  `decode_term`'s *compressed* path tolerate them — a crate-local convenience contract, not
  observable through the BIF.
- **`node` occupies the sysname-atom slot and `creation` rides C's off-heap `next` link** (a
  documented M1 substitution that keeps C's exact word positions/arities while preserving
  `creation`'s round trip); pointer semantics land when a node-identity type exists. Detail:
  `src/decode/CONTEXT.md` §Known Issues.

## Routing Table
- `src/` → the implementation node (crate wiring, `tag.rs`, `reader.rs`/`writer.rs`, `atom_cache.rs`,
  `off_heap.rs`, `compress.rs`, `error.rs`, `ref_wire.rs`, `prelude.rs`); per-file scope and the
  source-side constraints in [`src/CONTEXT.md`](./src/CONTEXT.md)
- `src/decode/` → the `Decoder` state machine: tag arms, input bounds, `[used]` count, the
  local-node identity rules D1–D7, `[safe]` + `ExportGate`, off-heap binaries, map canonicalization,
  depth bound — [`src/decode/CONTEXT.md`](./src/decode/CONTEXT.md)
- `src/encode/` → the `Encoder` state machine: minor-version flavours, floats, integer widths,
  internal ids, funs + `FunIdentity`, records, map key order, the `COMPRESSED` wrapper —
  [`src/encode/CONTEXT.md`](./src/encode/CONTEXT.md)
- `src/term/` → the only `unsafe` subtree: heap-agnostic writers/readers over `beam_term`, the
  record definition hash, the oversized-flatmap writer — [`src/term/CONTEXT.md`](./src/term/CONTEXT.md)
- `tests/` → the 34 integration binaries, their roles and the shared support modules —
  [`tests/CONTEXT.md`](./tests/CONTEXT.md)
- `examples/` → `etf_attrib.rs` (the codec-alone attribution binary; §Test Strategy)
- `Cargo.toml` → the crate manifest (the five dependency edges §Constraints pins; `license.workspace
  = true` → Apache-2.0, `[lints] workspace = true`)
- `ETF_COST.md` → the measured decomposition of the `copy_bound/etf_roundtrip` case
- `../../../erts/emulator/beam/external.c`/`external.h` → the authoritative encode/decode + tag values
  (read-only), cited inline in the children; `erl_zlib.c` → the C zlib wrapper;
  `../../../erts/emulator/zlib/` → the vendored zlib 1.3.2 whose output `beam_deflate` reproduces byte
  for byte; `../../../erts/emulator/sys/unix/sys_float.c` → the `FLOAT_EXT` text grammar;
  `erl_term.h`/`erl_map.h`/`erl_record.h` → term layouts + the legacy identifier bounds;
  `../../../erts/doc/guides/erl_ext_dist.md` → the external term format spec
- `../beam_term/`, `../beam_alloc/`, `../beam_deflate/` → the frozen foundations and the compressor
  this crate delegates to (siblings within `crates/`)
- `../beam_dist/`, `../beam_message/`, `../beam_ets/`, `../beam_rt/`, `../beam_bifs/` → the
  dependents (siblings — read-only, escalate writes to `..`)
- `../../tools/differential/`, `../../tools/bench/` → the compatibility gate and the benchmark driver
  (siblings — read-only, escalate writes to `..`)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 1;
  consumes wave 0).
- `../../CONTEXT.md` → workspace contracts (§2 frozen term layout, §3 error strategy, §6 test gate).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.5 → the distribution/ETF wire contract.
- `../beam_dist/CONTEXT.md` → the distribution consumer (dist header, `DFLAG_*`, handshake).
