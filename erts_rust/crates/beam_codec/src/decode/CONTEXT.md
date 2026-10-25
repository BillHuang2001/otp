# `beam_codec::decode` — the `Decoder` state machine (wire bytes → term)

## Intent
The **decode direction** of the ETF codec: one recursive term walker over a bounds-checked
`Reader`, writing terms straight into the caller's `beam_alloc::Heap` with atoms interned in the
caller's `beam_term::AtomTable` — no trait, no intermediate value tree.
Owns the decode-only tag arms, every decode-side input bound, the `[used]` byte count, the
`LOCAL_EXT`/local-node identity checks, the `[safe]` flag and the optional host seams
(`OffHeapBinaries`, `ExportGate`, `AtomCache`).
Crate-level intent, the frozen `Encoder`/`Decoder` surface and the tag inventory are
[`../../CONTEXT.md`](../../CONTEXT.md)'s — do not restate, obey.
Ground truth: `../../../../../erts/emulator/beam/external.c` (`dec_term` `:4307`,
`decoded_size` `:6040`).

## API Surface
- `Decoder<'a>`: `new(&'a [u8]) -> Result<Decoder>` (requires+consumes `VERSION_MAGIC`),
  `new_body` (version-less: a distribution payload / ETS body), then the self-consuming builders
  `with_atom_cache(&'a AtomCache)`, `with_off_heap(&'a mut dyn OffHeapBinaries)`,
  `with_export_gate(&'a dyn ExportGate)`, `with_local_node(Atom, creation: u32)` /
  `with_local_node_hash(u32)` (in `local_node.rs`), `with_safe()`/`set_safe(bool)`; the remaining
  decode options (`safe`, `local_node`, `local_hash`) travel in `DecodeOptions` (`options.rs`).
- `Decoder::decode(&mut Heap, &mut AtomTable) -> Result<Eterm>`, `position()`, `remaining()`,
  `finish() -> Result<()>` (trailing bytes ⇒ `BadArg`).
- Free entry points (re-exported at the crate root and in `prelude`): `decode_term`,
  `decode_term_used`, `decode_term_with_local_node`, `decode_term_with_local_node_used`,
  `decode_term_body`, `decode_term_body_used`, `decode_term_with_options`,
  `decode_term_with_options_used`, `decode_term_with_options_and_gate`,
  `decode_term_with_options_and_gate_used`, `is_compressed_image`, `is_compressed_body`.
- `pub trait ExportGate { fn is_export_active(&self, module: &str, function: &str, arity: u32) -> bool; }`
  (`export_gate.rs`) — a **trait object**, not a `fn` pointer: the predicate reads the host's
  borrowed code-server view, which a `fn` pointer cannot capture and a global would put runtime
  state back inside the codec (same shape as `OffHeapBinaries`/`AtomCache`).
- `DecodeOptions { safe, local_node, local_hash }` (`options.rs`) — a plain `Copy` configuration
  value; the gate is decoder state instead because it borrows live host state.
- `MAX_DECODE_DEPTH: usize = 1500` (`mod.rs`), `MAX_ATOM_CHARS`-class bounds from `beam_term`/
  `big` constants, and the `MAX_INTERNAL_{PID_NUMBER,PID_SERIAL,PORT_NUMBER}` limits in
  `local_node.rs`.

## The C's two-pass contract — why every bound is `BadArg`
`decoded_size` (`external.c:6040`) validates and `dec_term` (`:4307`) decodes with **no**
end-pointer checks; both switches accept the same tag set, but every bound (atom char/byte length,
`LARGE_BIG_EXT` digit count, `LARGE_TUPLE_EXT`/`MAP_EXT` size, `NEW_FUN_EXT` `num_free` ≤
`MAX_ARG`, subterm-count overflow) is enforced **only** in `decoded_size`, which routes every
failure to `B2TBadArg` (`external.c:1989-1991`).
This crate enforces the same bounds inline and reports the same class — `BadArg`, never
`SystemLimit`; the one exception is the depth bound below.
The C has **no** nesting-depth limit: `decoded_size` bounds only the total subterm count
(`Uint32 terms` + `ADDTERMS`, `external.c:6083-6088`), and `dec_term` is an explicit-worklist loop
whose only recursion is `EXPORT_EXT`'s arity term (`:5021`) and `NEW_FUN_EXT`'s
index/uniq/creator terms (`:5136,5145,5154`).

## Decode depth bound (`MAX_DECODE_DEPTH = 1500`)
`Decoder::decode` is recursive, so `<<131,(binary:copy(<<104,1>>,27500))/binary,106>>` (~55 KB of
nested 1-tuples) would exhaust the native stack and **abort the process** (a Rust stack overflow →
`SIGABRT`, never a typed error).
`decode` increments a `depth` counter on entry and decrements it on every exit path (the counter
lives in `decode`, not `decode_inner`, so no early `return` can leak it), returning
`BeamError::SystemLimit` at the bound — a **resource bound**, not malformed input.
Only **depth** is bounded, never length: the list spine and a tuple's elements decode iteratively
(a loop feeding a `Vec`), so a 1,000,000-element list and a 100,000-element tuple still decode.
The bound is measurement-driven: a nesting level costs ~340 bytes of native stack in dev, so the
2 MiB thread `tests/decode_depth.rs` uses overflows at ~6200; the bound sits at ~1/4 of that cliff.
C `dec_term` has no explicit depth limit (it recurses on the emulator's growable process stack), so
this is a **documented divergence** — the alternative was turning hostile input into a process
abort (same test).

## Tag-byte dispositions (the wire inventory is `../tag.rs`)
**Decode-only, never emitted here — `ATOM_INTERNAL_REF2` (`73`) / `ATOM_INTERNAL_REF3` (`75`)**:
`[73][index:u16 BE]` / `[75][index:u24 BE]` name the atom already at that `AtomTable` index, resolved
through `Decoder::indexed_atom`; an index at/past the table length is `BadArg` (probed live:
`<<131,73,255,255>>` ⇒ `badarg`).
Unlike `74`/`76`/`78` these two are **not** `internal_tags`-gated, so the C accepts them
unconditionally in a plain image (`external.c:5177-5192`, `dec_atom` `:3085-3100`, sizing
`:6405-6412`), and they are **exempt from `[safe]`** because they name an existing atom.
**Rejected as malformed `BadArg`**, exactly as the C: `68 DIST_HEADER` (`external.c:6464`),
`117 FUN` (removed OTP 23, `:6399-6404`), `78 NEW_CACHE` (dead `#if 0`, `external.h:77-81`).
**Not this crate's** — the internal-ref family `BINARY_INTERNAL_REF 'J'` (`74`),
`BITSTRING_INTERNAL_REF 'L'` (`76`), `MAGIC_REF_INTERNAL_REF 'N'` (`78`) plus the transport
`DIST_FRAG_HEADER 'E'` (`69`), `DIST_FRAG_CONT 'F'` (`70`) and `HOPEFUL_DATA 'H'` (`72`): the C
reaches `74`/`76`/`78` only in its `internal_tags` mode (a distribution/ETS context,
`external.c:2885-2898`, `:3633`, `:3874-3900`, `:5177-5255`, `:6405-6451`), so they belong to
`beam_dist`/`beam_ets` and a plain image is `BadArg` on all three (probed live).
Tag-byte collisions to keep in mind when reading the C or a captured image: `'F'` = `70` is
`NEW_FLOAT_EXT` **and** `DIST_FRAG_CONT`; `'N'` = `78` is `MAGIC_REF_INTERNAL_REF` **and** the
retired `NEW_CACHE`; `'C'` = `67` is `RECORD_EXT` **and** the retired `CACHED_ATOM`; `'p'` = `112`
is `NEW_FUN_EXT` **and** the internal-only `PASS_THROUGH` dist-header placeholder
(`external.c:56`). Disambiguation is positional in every case.

## Legacy identifier forms (`101`/`102`/`103`/`114`)
The pre-`DFLAG_BIG_CREATION` identifier tags decode with their full C widths and legacy validation
and re-encode to the modern forms the C always emits (`90`/`88`/`120`) — pinned byte-for-byte
against `term_to_binary(binary_to_term(Img))` by `tests/port_ext_legacy.rs`.
`PORT_EXT` (`102`) is `[node][id:4 BE u32][creation:1]` (`external.c:4685-4716`; the sizing pass
skips 5 extra bytes, `:6210-6211`), `PID_EXT` (`103`) is
`[node][number:u32][serial:u32][creation:1]` (`dec_pid`, `:3141-3197`), `REFERENCE_EXT` (`101`) is
`[node][id:4 BE u32][creation:1]` and `NEW_REFERENCE_EXT` (`114`) is
`[len:2][node][creation:1][ids:len×u32]` (`:4743-4784`).
All three reference tags read their first id word **unconditionally** (before the creation byte for
`101`, after it for `114`/`90`; `external.c:4752-4763`/`:4780`/`:4795`), so exactly `max(1, len)`
id words follow the creation field — a `len == 0` image stays aligned and decodes to a **zero-id
external ref** (the first word is consumed for alignment but **no** ids are stored), re-encoding
byte-exactly as the 11-byte `[90][0][node][creation]`; only the pure 11-byte *input* form that omits
the unconditional first id word is `BadArg`.
**Tiny creation**: the 8-bit creation of `101`/`114`/`102`/`103` must be ≤ 3
(`is_tiny_creation`, `external.c:71-72`) else `BadArg`.
**`MAX_REFERENCE`** (`1 << 18`, `erl_term.h:771`) is an **unsigned** bound checked *before* the node
is resolved, so it holds for a remote node too: the first id of `101`/`114` must be below it
(`:4757`/`:4782`) else `BadArg`, while `90` has no *pre-node* bound but the C re-checks
`r0 >= MAX_REFERENCE` in its **local-node** branch (`:4807`), which this decoder applies whenever
the caller installed the local node.
**`MAX_REF_IDS`**: a `90` id count above `ERTS_MAX_REF_NUMBERS` (5) is `BadArg`.

## Scalar wire rules (`SMALL_BIG`/`LARGE_BIG`, `FLOAT`/`NEW_FLOAT`, atoms)
Atoms are **interned, never compared by name**: the decoder interns into the caller's `AtomTable`
in wire order (redundant identifier forms and strings likewise decode to one canonical
representation).
A bignum's sign byte is a **boolean**: `neg = get_int8(ep)` is only tested for zero-ness
(`big.c:3446-3448`), so **any** non-zero sign byte means negative (`2` and `1` both negate) while a
zero magnitude is `0` whatever the sign byte is (`external.c:4484-4486`); leading zero bytes are
stripped first, so a magnitude fitting a 60-bit small decodes to a small
(`tests/scalar_ext_decode.rs`).
`NEW_FLOAT_EXT` (`70`) must be **finite** (`external.c:4667-4669`) ⇒ an infinity or NaN is `BadArg`.
`FLOAT_EXT` (`99`) is 31 bytes of a NUL-terminated `%.20e` rendering, but the decoder does **not**
simply `strtod` it: `sys_chars_to_double`
(`../../../../../erts/emulator/sys/unix/sys_float.c:72-140`) accepts exactly
`[+-]? DIGIT+ [.,] DIGIT+ ([eE] [+-]? DIGIT+)?` and requires a **finite** result, so
`inf`/`-inf`/`nan`/`infinity`/`1e5` (no radix)/`1.`/`.5`/` 1.0`/hex floats/… are `BadArg` (Rust's
`parse::<f64>()` would accept several); a `,` radix is accepted (`sys_float.c:115-124`). Pinned by
`tests/legacy_scalar_oracle.rs`.
An atom name longer than `MAX_ATOM_CHARS` (255 **characters**, not bytes) is malformed
(`atom.c:258-274`/`:392-418`, `external.c:4529-4539`) ⇒ `BadArg` on **decode** (`SystemLimit` on
**encode**); invalid UTF-8 under a UTF-8 atom tag is `BadArg`.
Encode emits `70` by default and `99` only under `MinorVersion::V0` (`../encode/CONTEXT.md`), so a
default-flavour decode↔encode of a `99`-encoded float is representation-preserving rather than
byte-identical (under `V0` the 31-byte text form is reproduced exactly).

## `Atom` positions are `dec_atom`, node fields are `dec_sysname`
`Decoder::decode_atom_strict` is C `dec_atom` (`external.c:3010-3106`): it decodes a term and
requires an atom, with **no** `NIL_EXT` (106) case — `default:` is `error`.
`Decoder::decode_node_atom` is C `dec_sysname` (`:3114-3122`): it additionally accepts the
internal-node marker under `LOCAL_EXT` mode.
`EXPORT_EXT`'s module/function and `RECORD_EXT`'s module/name/key atoms use the strict helper
(`:5008-5013`, `:5301-5321`); identifier node fields and `decode_creator_pid` keep the
marker-tolerant one.
Reusing the node helper in an atom position built a fun/record from `atom_val(NIL)` — a bogus atom,
i.e. a wrong value (pinned by `fun.rs`/`record.rs` tests).

## Local node identity (`local_node.rs`)
The C decoder is not purely wire-driven: several checks first ask *"is this identifier's node the
node I am running on?"* (`dec_is_this_node`, `external.c:3123-3129`), then apply rules that never
apply to a remote node.
This codec owns no node registry, so those rules live behind an **additive, all-optional** seam;
with `local_node`/`local_hash` both `None` every check is inert and decoding is byte-for-byte
unchanged.
`Decoder::with_local_node(node: Atom, creation: u32)` installs the local node's `sysname` atom **as
interned in the caller's `AtomTable`** plus its `creation`; `Decoder::with_local_node_hash(hash: u32)`
installs the expected `LOCAL_EXT` node hash. Five identity-gated rules are closed (each the C rule,
verified against erts-17.1):
- **D1 — local identifier demotion**: an identifier whose node/creation is the local node takes the
  **internal** form rather than an external thing — pid ⇒ `make_internal_pid(make_pid_data(serial,
  number))`, port ⇒ `make_internal_port(make_port_data(number))` (`dec_pid`, `external.c:3175-3182`;
  port arm `:4713-4721`), reference ⇒ an **M1 internal ref**
  (`[header(3,REF_SUBTAG), small(0), small(0), small(id)]`; `write_ref_thing` + `make_internal_ref`,
  `:4821-4862`). Every identifier wire form of every kind is covered, so a decoded local pid is
  `=:= self()`. A **reference** demotes only when its packed id fits the internal form's single-word
  id slot, else keeps the faithful **external** reference (`ref_wire::representable_ref_id`); a
  *remote* node keeps the external thing (`tests/local_identity_roundtrip.rs`).
- **D2 — local reference first number**: a local `NEWER_REFERENCE_EXT` (`90`) must have
  `r0 < MAX_REFERENCE` (`1 << 18`), else `BadArg` (`external.c:4804-4813`); a remote node keeps any
  `r0` (`check_local_ref_first_number`).
- **D4 — the `LOCAL_EXT` node hash**: a `LOCAL_EXT` (`121`) whose 4-byte hash differs from the
  installed one is `BadArg` (`check_local_node_hash`; C `decoded_size`, `:6485`/`:6505-6511`) —
  top-level, nested, or wrapping a creator pid.
- **D6 — local pid/port bounds**: a local pid needs `number <= ERTS_MAX_INTERNAL_PID_NUMBER`
  (`2^28 - 1`) and `serial <= ERTS_MAX_INTERNAL_PID_SERIAL` (`2^32 - 1`); a local port
  `id <= ERTS_MAX_INTERNAL_PORT_NUMBER` (`2^60 - 1`) (`dec_pid`, `:3175-3179`; port arm `:4718-4721`)
  — for every wire form of the kind and the `NEW_FUN_EXT` creator pid.
- **D7 — a `LOCAL_EXT`-wrapped creator pid**: `decode_creator_pid` (`fun.rs`) accepts `PID_EXT`
  (`103`), `NEW_PID_EXT` (`88`) and a `LOCAL_EXT` (`121`) wrapper around either, since the C decodes
  the creator via the generic `dec_term` + `is_pid` (`:5153-5160`); such a pid is a valid creator,
  its hash checked when one is installed.
The hash is a **caller-supplied value, not an algorithm**: the C's `local_node_hash` is a
**per-VM-start** value (hostname + OS pid + time offset + monotonic time, `external.c:136-199`) and
the 4 bytes carry `erts_block_hash(<inner term bytes>, local_node_hash)`; the codec owns neither
seed nor identity, so — like the encoder side — the seam takes the **expected hash value**.
`LOCAL_EXT` (`121`) is decoded inline past its 4-byte hash; `DIST_HEADER` (`68`) and a mismatched
hash are `BadArg`.
**Internal-ref wire mapping** is `../ref_wire.rs`'s — the one authority, shared by decode, encode
and every dependent crate: the C's `erts_set_ref_numbers`/`erts_get_ref_numbers_value` bit layout
(`erl_bif_unique.h:117-131`/`:158-167`, masks `erl_term.h:768-773`) —
`ids = [(id & 0x3FFFF), (id & 0xFFFC_0000), (id >> 32)]`, inverse
`id = (ids[0] & 0x3FFFF) | (ids[1] & 0xFFFC_0000) | ((ids[2] as u64) << 32)` — a shorter number
list zero-padded to 3, a longer one (len 4/5) using only its first 3 (C `external.c:4821-4839`);
`num[0]` stays `< 2^18`, so the emitted local ref is oracle-valid and any M1 id up to the 60-bit
small round-trips exactly, including ETS-handle-scale ids ≥ `2^40`.
A caller writing a reference image **must** use exactly that packing.
`representable_ref_numbers` is the checked form for an id that has to come back as an *internal*
reference (it refuses beyond `MAX_SMALL` with `BeamError::SystemLimit`, rather than putting an id on
the wire that `representable_ref_id` answers `None` for). Nothing in the module truncates an id.
Tests: `../ref_wire.rs`'s unit suite + `tests/ref_wire_public.rs` (literal C goldens that fail on a
word swap, every word boundary, the typed refusal past the id slot, the decode-demotion/encode
coupling) + `tests/local_node.rs` / `tests/local_identity_roundtrip.rs`.

## `[safe]` decode (`binary_to_term/2`'s flag)
Implemented via `Decoder::with_safe`/`set_safe`, `DecodeOptions.safe` and the
`decode_term_with_options*` entry points: a wire atom **name** is resolved by *lookup* only and a
miss is `BadArg` (`external.c:4530-4539`, `:3073-3082` — C `erts_atom_get`, never `erts_atom_put`),
so a hostile image cannot grow the node's atom table.
Index-carrying forms (`ATOM_CACHE_REF` `82`, `ATOM_INTERNAL_REF2`/`_REF3`) name an existing atom and
are exempt; the flag is off by default, so every pre-existing entry point is byte-for-byte unchanged.
The active-export half is a host seam: C's `EXPORT_EXT` arm requires
`erts_active_export_entry(mod, name, arity)` under the flag (`external.c:5032-5036`), a lookup in
the running node's active export table, so the arity is the low 32 bits (the truncation
`decode_export` already performs).
`decode_export` consults the gate **immediately after** the 32-bit arity truncation and **only**
under `self.safe`; a `false` answer is `BadArg` with nothing materialized, and a module/function
atom whose name is not valid UTF-8 is `false`.
With **no gate installed the check is skipped**; the distribution decode (`flags = 0`) and
`NEW_FUN_EXT` never consult it — exactly as in the C.
Wiring: `beam_bifs::binconv::decode_term_bif` calls
`decode_term_with_options_and_gate_used`, and `beam_rt`'s `ExportGateBridge`
(`beam_rt/src/context/export_gate.rs`, installed on every `binary_to_term/2` dispatch) answers it
from the loaded-module cache plus the registered BIFs — so a live `binary_to_term(B, [safe])` does
both halves the C does.

## `[used]` count
`binary_to_term/2`'s `used` option reports how many source bytes the term occupied; the C inflates
a `COMPRESSED` payload and then measures the consumed input length (`external.c:1797-1801`).
A non-compressed image reports `Decoder::position()`; a `COMPRESSED` image reports the whole
wrapper `6 + <zlib stream length>` (the 6 = version byte + tag + 4-byte size); the body-form twins
report the 5-byte `[tag][size]` header plus the stream.
`is_compressed_image` is the `[VERSION_MAGIC][COMPRESSED_EXT][size:4 BE]`-with-≥6-bytes predicate
(`external.c:1754-1759`), shared by the decode path and the distribution reader;
`is_compressed_body` is its version-less twin.
The count stops at the stream end and at the term for a plain image, so **trailing bytes are
neither read nor counted** — hence, unlike `decode_term`, the `_used` entry points do **not** call
`finish()`.

## Off-heap binaries (`../off_heap.rs` + `decode_bitstring` + `../term/binary.rs`)
`OffHeapBinaries::alloc_refc_payload(bytes) -> Result<*mut Eterm>` lets a decoded large binary's
**payload** live off the process heap (mirrors `AtomCacheWriter`): the host allocates + **retains**
the payload (typically a `beam_alloc::RefcBinary`) and returns the raw pointer to its **first data
byte**, i.e. the `BinRef.val` value.
Contract: the pointer must stay valid/unmoved/readable at least as long as any term built from it
(the host's store *is* the ownership record — the decoder keeps none, so releasing early dangles),
the payload holds `bytes`, the first byte must be **word-aligned** (the low
`BITSTRING_POINTER_FLAG_MASK` bits of `ErlSubBits::base_flags` are flags; `RefcBinary` guarantees
it), it is never mutated afterwards, and failure (e.g. OOM) is return-based.
`decode_bitstring` routes by the **bit** size: `size_in_bits > ERL_ONHEAP_BITS_LIMIT` (512 bits =
64 bytes) **and** an installed host ⇒ `../term/binary.rs::make_refc_binary` for **both**
`BINARY_EXT` and `BIT_BINARY_EXT`, else `make_bitstring` (on-heap, identical to the no-host
behaviour; C `external.c:4893-4980`).
`make_refc_binary` allocates `ERL_REFC_BITS_SIZE` (8) words, guards a `bytes` slice too short for
the `offset_bits + size_bits` window, an overflowing window and a null/misaligned host pointer
(⇒ `BadArg`, nothing written — the pair is allocated only after the payload exists) and passes
`next = null` (the host store is the ownership record).
The wire is `[len:4 BE][data]` for a binary and `[len:4 BE][bits:1][data]` for a bit binary — the
length field comes **before** the trailing-bit count and `len` is checked against the bytes left in
the cursor *before* any allocation (a lying length ⇒ `BadArg`); for `BIT_BINARY_EXT` the
trailing-bit count must be `1..=8` and `len * 8 >= 8`, with
`size_in_bits = len * 8 - (8 - bits)`.
The C clears the last byte's unused trailing bits before copying and `make_bitstring`/
`make_refc_binary` zero them (`tests/refc_decode.rs`).

## Map decode (ERTS canonicalization)
The `Tag::Map` arm reads the 4-byte pair count — rejecting a count whose bit 31 is set
(`external.c:6311-6315`) — then the interleaved `K1,V1,…,Kn,Vn` pairs, and materializes the map
through `../term/mod.rs::make_map`, the canonical ERTS constructor; that is exactly the
`external.c:5438-5456` fixup (`erts_validate_and_sort_flatmap`, `erl_map.c:3187`;
`erts_hashmap_from_array(..., 1)`, `erl_map.c:644`), so a decoded map carries ERTS's storage order
and re-encodes byte-identically.
`beam_term::map::write_map` **collapses** duplicate keys (last value wins), so the arm detects a
duplicate by comparing the produced `map_size` against the wire count and returns `BadArg` on a
mismatch — ERTS **rejects** a duplicate-keyed image in both representations (`external.c:5448`),
since only `maps:from_list/1` collapses duplicates, never `binary_to_term/1`; keys `1` and `1.0`
are `=:=`-distinct and both survive (`tests/map_ext_decode.rs`).

## Constraints
- Inherited from [`../../CONTEXT.md`](../../CONTEXT.md): `beam_common::{Result, BeamError}` only,
  `BadArg` for every malformed input and every bound, never panic / `unwrap()` runtime data /
  silently truncate.
- Safe Rust only: `#![deny(unsafe_code)]` crate-wide holds here; raw heap words are reached solely
  through `../term/`'s safe wrappers.
- Every identity-gated or gate-gated behaviour must stay **additive and inert** when nothing is
  installed — the distribution path (`flags = 0`, no local node) is untouched.
- The node registry, the `local_node_hash` seed, the active-export registry and off-heap payload
  ownership are the host's (`beam_rt`, `beam_bifs`), never this module's.

## Known Issues
- **A foreign local reference can exceed the M1 id space.** The packed id is 18+14+32 bits
  (`../ref_wire.rs`), but the frozen M1 internal-reference thing stores it in one 60-bit small, so
  only ids `< 2^59` are representable. A reference **this** runtime produced always fits and
  round-trips exactly; an arbitrary *oracle* `make_ref()` carries a nonzero scheduler `thr_id` in
  `num[1]` bits 0..15 (dropped by the `REF1_NUM_MASK` unpack) and a 30-bit `num[1]` field, so its
  packed id can exceed the small range. The demotion is **representability-gated**
  (`ref_wire::representable_ref_id`): a local reference past the small range keeps the faithful
  **external** reference form (never a truncated id, never a wrong term, never an error), so even
  such an image round-trips byte-exactly — only the *internal* shape is out of reach; and a
  representable foreign id loses the `num[1]` `thr_id`/marker bits the unpack masks, so its internal
  re-encode need not be byte-identical. Widening the id slot is a `beam_term` layout decision
  (**owner: `beam_term`**). Honest limitation, not a bug.
- **A `LOCAL_EXT` node hash is installed by no host** (`DecodeOptions.local_hash` stays `None` in
  `beam_bifs`), so D4 is unchecked at runtime — a `beam_rt`/`beam_bifs` call.
- **`decode_term`'s *plain* path is deliberately stricter than the oracle on trailing bytes** (it
  calls `finish()`), whereas `binary_to_term/1,2` (via `Decoder::decode` in `beam_bifs`) and
  `decode_term`'s *compressed* path tolerate them — a crate-local convenience contract, not
  observable through the BIF (`tests/compressed_decode.rs`).
- **`node` occupies the sysname-atom slot; `creation` rides C's off-heap `next` link** (a
  documented M1 substitution). C's `ExternalThing` first data word points to a *registered node*
  (carrying `sysname` + `creation`), the second is the off-heap `next` link; with no node registry
  here the sysname atom fills the `node` slot and `creation` the unused `next` slot, yielding
  **exactly C's word positions/arities** (head 3 words, pid/port 4, ref
  `EXTERNAL_THING_HEAD_SIZE + ref_words/2 + 1`) while preserving `creation`'s round trip; pointer
  semantics land when a node-identity type exists (`beam_term`/`beam_dist`).
- **`NEW_FUN_EXT` `old_index`/`old_uniq` are kept verbatim from the wire** while the C re-emits
  them from the runtime's fun-entry stub: C keys `erts_fun_entry_get_or_make_stub` by
  `(module, MD5, index, arity)` and re-emits the *stub's* fields (`external.c:3987-3988`,
  `:5162`/`:5164`), so a wire value differing from an already-loaded module's entry is discarded
  there but preserved through this crate's inlined descriptor (`fun.rs::decode_new_fun`).
  Observable only via `term_to_binary/1` of such a fun. **OWNER: the runtime fun-entry table**
  (`beam_term`/the compiling runtime). The **acceptance** side has no such gap: the C never
  requires the module to be loaded or the index to exist (`erts_fun_entry_get_or_make_stub`
  *makes* a stub, `erl_fun.c:299-309`, unchecked at `external.c:5162`), so a structurally valid
  `NEW_FUN_EXT` — loaded or not, real index or not — decodes on both sides
  (`tests/tag_space_audit.rs` pins the unloaded-module image; verified live: `binary_to_term` of
  our `m`-module row is `{ok,#Fun<m.0.0>}` on erts-17.1).

## Test Strategy
- `mod.rs`'s in-module `#[cfg(test)] mod tests` plus the shared `tests.rs` suite (the `pub(crate)`
  helpers `new_heap`, `decode`, `assert_bad_arg`, `assert_system_limit` also serve `fun.rs`/
  `record.rs`).
- `local_node.rs`: the predicate + D2/D4/D6 + the `MAX_INTERNAL_*` constants.
- `options.rs`: the `[safe]` flag plumbing; `export_gate.rs`: the gate check site.
- Integration binaries covering this node (see `tests/CONTEXT.md` for the inventory):
  `decode_hostile.rs` (every valid image truncated at every offset and every byte flipped, plus
  targeted length/count/identifier corruption and `COMPRESSED` attacks — a typed `Err`, never a
  panic or over-read), `decode_depth.rs`, `decode_bound_oracle.rs`, `scalar_ext_decode.rs`,
  `legacy_scalar_oracle.rs`, `port_ext_legacy.rs`, `ref_ext_zero_and_arity.rs`,
  `refc_decode.rs`, `map_ext_decode.rs`, `local_node.rs`, `local_identity_roundtrip.rs`,
  `ref_wire_public.rs`, `export_gate.rs`, `export_arity_oracle.rs`, `tag_coverage.rs`,
  `tag_space_audit.rs` (the exhaustive 256-row disposition sweep — the one place a *byte's*
  disposition is decided), `compressed_decode.rs`/`compressed_bytes.rs`/`compressed_used.rs`,
  `property_roundtrip.rs`, `interop.rs`.
- Oracle-dependent cases resolve `$ERTS_ORACLE` else `erl` on `$PATH` and **skip loudly** (a stderr
  note, never a silent pass).

## Routing Table
- `mod.rs` → `Decoder`, `decode_term`, `decode_term_used`/`decode_term_with_local_node_used`, the
  body-form twins `decode_term_body`/`decode_term_body_used` + `is_compressed_body`,
  `decompress_wrapper`/`decompress_body_wrapper` + the shared `inflate_compressed`,
  `is_compressed_image`, `MAX_DECODE_DEPTH` + the `depth` bound, `atom_term`/`indexed_atom`, the
  `ATOM_INTERNAL_REF2`/`_REF3` arms and the `match tag` arms; ~1485 lines (one state machine — the
  tag dispatch plus its per-tag readers)
- `local_node.rs` → the identity seam: the two builders, `is_local_node`, D2/D4/D6 + the
  `MAX_INTERNAL_*` constants
- `options.rs` → `DecodeOptions` + `with_safe`/`set_safe` + `decode_term_with_options[_used]` + the
  private `apply`
- `export_gate.rs` → the `ExportGate` host trait
- `fun.rs` → `decode_export`/`decode_new_fun`/`decode_small_u32`/`decode_creator_pid`
- `record.rs` → `Decoder::decode_record`
- `tests.rs` → the decode unit suite + the `pub(crate)` helpers
- `../encode/` → the encode direction (sibling: `../encode/CONTEXT.md`)
- `../term/` → the materialization wrappers this module writes through
- `../tag.rs` → the tag values and the `Tag` enum (the wire inventory)
- `../../tests/` → the integration binaries (sibling — read-only, escalate writes to `../..`)
- `../../../../../erts/emulator/beam/external.c` → authoritative ground truth (read-only)
