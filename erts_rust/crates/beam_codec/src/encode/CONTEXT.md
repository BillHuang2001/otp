# `beam_codec::encode` — the `Encoder` state machine (term → wire bytes)

## Intent
The **encode direction** of the ETF codec: one recursive term walker emitting into a growable
`Writer`, plus the per-kind emitters (integers, floats, atoms, identifiers, funs, records, maps)
and the image finishers (plain, `COMPRESSED`, `[local]`).
It never deduplicates, never re-canonicalizes and never sorts unless asked: the order it emits is
whatever the term's own representation stores, because that is what ERTS does.
Crate-level intent, the frozen surface and the tag inventory are
[`../../CONTEXT.md`](../../CONTEXT.md)'s — do not restate, obey.
Ground truth: `../../../../../erts/emulator/beam/external.c` (`enc_term`, `encode_*` emitters).

## API Surface
- `pub struct Encoder<'a>`: `new(&'a AtomTable)` (emits `VERSION_MAGIC`), `new_body` (no version
  byte — dist payload/ETS; pre-reserves `DEFAULT_IMAGE_CAPACITY = 128` bytes, an upper bound only),
  the self-consuming builders `with_atom_cache`, `with_local_node(node: Atom, creation: u32)`,
  `with_local_ext()`, `with_deterministic()`, `with_fun_identity`, `with_minor_version`, plus the
  `set_*` twins (`set_atom_encoding`, `set_local_ext`, `set_deterministic`, `set_minor_version`),
  `encode(Eterm) -> Result<()>`, `as_bytes`/`len`/`is_empty`, `finish() -> Vec<u8>`,
  `finish_compressed(level)`, `finish_local(hash: u32)`.
- `pub enum AtomEncoding { Utf8, Latin1 }` (default `Utf8`) and
  `pub enum MinorVersion { V0, V1, V2 }` (default `V2`).
- `pub struct FunIdentity { module: Atom, uniq: [u8; 16], old_index: u32, old_uniq: u32,
  creator: FunPid, function: Option<Atom> }` (+ `new`/`named`/`with_function`) — the `ErlFunEntry`
  fields (`../../../../../erts/emulator/beam/erl_fun.h:31-59`) a `make_fun3` fun lacks but
  `NEW_FUN_EXT` needs.
- Free entry points (crate root + `prelude`): `encode_term` (with `131`), `encode_term_body`
  (without), both **uncompressed**; `encode_term_compressed`, `encode_term_body_compressed`,
  `encode_term_local(term, atoms, hash)`, `encode_term_with_minor_version`.
- `Writer` (`../writer.rs`) plus two crate-internal backward writes: the bounds-checked
  `patch_u32be(pos, value) -> Result<()>` (the `NEW_FUN_EXT` size field) and `mask_last_byte(mask)`
  (clearing a bit-string tail's unused low bits in place).

## Minor-version flavours (`{minor_version, N}`)
`parse_t2b_opts` (`external.c:1501-1516`) maps `{minor_version,N}` onto dflags;
`TERM_TO_BINARY_DFLAGS` (`dist.h:138-139`) is `DFLAG_NEW_FLOATS | DFLAG_UTF8_ATOMS` with **no**
`DFLAG_SMALL_ATOM_TAGS`:

| `MinorVersion` | atoms | floats |
|---|---|---|
| `V0` | `ATOM_EXT` (`100`) for a Latin-1 name (never `115`, any length), UTF-8 tags for a non-Latin-1 name | `FLOAT_EXT` (`99`), 31-byte text |
| `V1` | same as `V0` | `NEW_FLOAT_EXT` (`70`) |
| `V2` (default) | `AtomEncoding` (UTF-8 by default) | `NEW_FLOAT_EXT` (`70`) |

The private `AtomFlavour` projects the C `enc_atom` decision (`external.c:2906-2950`):
`AtomEncoding::Latin1` (`115`/`100`, `unsupported` for a non-Latin-1 name) is consulted only under
`V2`, while under `V0`/`V1` the `Latin1WideOnly` flavour always emits `ATOM_EXT` for a Latin-1 name
and **falls back to UTF-8 tags** for a non-Latin-1 one (`external.c:2910`) — never `unsupported`.
An atom name beyond `MAX_ATOM_CHARS` (255 **characters**) is a `SystemLimit` here (it is `BadArg` on
decode).

## Floats (`float.rs`)
`NEW_FLOAT_EXT` + 8 IEEE-754 bytes under `DFLAG_NEW_FLOATS` (`V1`/`V2`); else `FLOAT_EXT` + a
**fixed 31-byte** field (`sys_double_to_chars` = `snprintf(buf, 31, "%.20e", f)`,
`SYS_DEFAULT_FLOAT_DECIMALS 20`, then zero-fill; `external.c:3840-3847`).
`format_float_ext`/`normalize_exponent` match glibc's rounding but rewrite Rust's exponent tail
(`e0`/`e-5`/`e20`) to C's always-signed ≥2-digit `e+00`/`e-05`/`e+20`, and render non-finites as
glibc's `inf`/`-inf`/`nan`; a >31-byte render is a `SystemLimit` (unreachable for a finite `f64`).
A non-finite float has **no oracle-verifiable** `FLOAT_EXT` golden — the oracle rejects a NaN/Inf
`NEW_FLOAT_EXT` with `badarg`, so such a term never reaches `term_to_binary/2 {minor_version,0}`.

## Integer width policy (mirrors C, `external.c:3542-3599`)
`< 256` ⇒ `SMALL_INTEGER_EXT` (`97`); `INTEGER_EXT` (`98`) when the value fits a signed 32-bit;
else `SMALL_BIG_EXT` (`110`) when the magnitude's significant base-256 **byte** count < 256, else
`LARGE_BIG_EXT` (`111`).
The count is derived from the `ErtsDigit`s as `big_to_bytes` does (`big.c:3405-3490`): full eight
bytes per non-leading digit, only the leading digit's significant bytes, high zero bytes stripped
first; the encode-side wire bound is `BIG_ARITY_MAX * sizeof(ErtsDigit)` bytes.
`110`/`111` decode to a small int whenever the value fits a 60-bit small, else a bignum; a sign byte
is a boolean on the wire (`2` and `1` both negate) — see `../decode/CONTEXT.md`.

## Internal ids — `NEW_PID_EXT` / `V4_PORT_EXT` from internal immediates
A runtime pid/port (`self()`, a spawned process, a port) is an **internal 60-bit immediate**
(`beam_term::immediate`), *not* a boxed **external** id thing, so the boxed probes
(`external_pid_parts`/`external_port_parts`/`external_ref_parts`) do not match it.
`Encoder::with_local_node` supplies the **local node identity** (the `sysname` atom + `creation`;
node identity is `beam_rt`/`beam_dist`'s, so the caller passes it), and with it set `encode` emits
internally-tagged ids byte-identically to the oracle: an internal pid ⇒ `NEW_PID_EXT` (`88`)
`[node atom][number:u32 BE][serial:u32 BE][creation:u32 BE]` (e.g. `term_to_binary(self())` on
`nonode@nohost`, id 10: `131,88,119,13,"nonode@nohost",0,0,0,10,0,0,0,0,0,0,0,0`), and an internal
port ⇒ `V4_PORT_EXT` (`120`) `[node atom][number:u64 BE][creation:u32 BE]` (the C always emits the
v4 form for a port).
Without a configured local node an internal id stays **`unsupported`** (never a wrong image) — the
codec cannot invent a sysname, and a value merely *containing* one fails the same way.
An internal **reference** (`make_ref/0`'s M1 4-word `[make_header(3,REF_SUBTAG),0,0,id]` thing) is
encoded the same way: with `with_local_node` set it emits `NEWER_REFERENCE_EXT`
`[90][len=3][node][creation:u32][numbers…]` through `../ref_wire.rs`; **without** it, `unsupported`.
Every decode form of every identifier is accepted (`../decode/CONTEXT.md`).

## Fun encodings (`EXPORT_EXT` `113` / `NEW_FUN_EXT` `112`)
`encode` probes the fun family with `is_any_fun` (via `fun_kind_term`) **before** the boxed-family
probes (a fun header's distinct subtag makes the classification exact) and routes to `fun.rs`
(`encode_fun`/`encode_fun_external`/`encode_fun_imported`/`encode_fun_local`).
- **`EXPORT_EXT` (`113`) ⇒ `FunKind::External`** — `[113][module atom][function atom][arity term]`
  (`external.c:5006-5040`). The two atoms emit in descriptor order; the arity is a **normal term**
  and is **truncated to the C's 32-bit field** (the 64-bit `Sint` of `external.c:5028` narrows to
  `ErtsCodeMFA.arity`, a `Uint`), so only its low 32 bits survive (`2^32` ⇒ `0`, `2^32 + 7` ⇒ `7`,
  `3 * 2^31` ⇒ `0x8000_0000`), byte-identical to the oracle. The term stores the **true** arity
  (descriptor word 3) while the 8-bit header field carries `arity & FUN_MAX_FIELD`, so
  `fun_arity_term` masks and `fun_true_arity_term` is exact (the oracle's split between
  `fun_info(F, arity)` and `term_to_binary/1`); encode emits `[113][module][function][<true arity>]`.
- **`NEW_FUN_EXT` (`112`) ⇒ `FunKind::Imported`** —
  `[size:4 BE][arity:1][uniq:16][index:4 BE][num_free:4 BE][module atom][old_index term]
  [old_uniq term][creator pid term][env…]` (`external.c:5102-5176`). `size` is **read and ignored**
  on decode (since OTP 23 the C may write a wrong total size, `:5115`); on encode it is
  `4 + payload_len`, the bytes from the size field's first byte through the end of the fun (C
  `ep - size_p`, `:3363`, tag byte excluded) — `ftest:make3("aa","bb","cc")` →
  `131,112,0,0,0,86,…` (82 = 86 − 4). `old_index`/`old_uniq` decode as **smalls** (C `is_small`) and
  are stored verbatim as the descriptor's `u32`s; `num_free > MAX_ARG` (255) cannot fit the 8-bit
  header field ⇒ `BadArg` (never a corrupt header).
  **ENV ORDER — wire order is the env order**: stream element `k` is `env[k]` (C links `env[0]`
  first, `:5171-5174`, and emits `env[0]` first, `:3991-3993`) — nothing is reversed.
  The creator pid always emits as `NEW_PID_EXT`; a legacy `PID_EXT` on the wire must have a tiny
  (≤ 3) creation, else `BadArg` (`:5153-5160`).
- **`FunKind::Local`** (the `make_fun3` closure) stores only its engine-internal descriptor
  (`[uniq: u64][index: u32]`) — no module / 16-byte `uniq` / creator pid — and encodes as
  `NEW_FUN_EXT` **iff** a `FunIdentity` was installed (arity/index/`num_free`/env from the term;
  module/uniq/`old_index`/`old_uniq`/creator from the identity); without one ⇒
  `error::unsupported_local_fun()`, never a mis-encoded term.

### `FunIdentity` — the local-fun encode input
`Encoder::with_fun_identity(FunIdentity)` is the additive, optional seam letting the runtime encode
a `make_fun3` closure; `None` by default, so every un-flavoured `encode_term` is byte-for-byte
unchanged.
Its fields mirror the C `ErlFunEntry` (`creator` = `erts_init_process_id`, `external.c:3989`); the
term's own `index` (descriptor word 2) is the wire `index`, and its inlined `uniq: u64` is
engine-internal, never on the wire.
The `function: Option<Atom>` field is the lambda's **local function name** — the `FunT` chunk
entry's function atom, what `erlang:fun_info(F, name)` reports — and is **introspection-only**:
`NEW_FUN_EXT` carries no function-name field, so the bytes are identical with or without it
(`new` sets `None`; `with_function`/`named` attach it; pinned by
`fun.rs::the_function_name_is_never_encoded`).
A `Local` fun's identity is fully derivable from a loaded module: `beam_loader::Module::checksum()`
is the 16-byte `uniq` (C `ErlFunEntry.uniq` = `beam->checksum`, `asm_load.c:1218-1223`),
`FunDescriptor` gives `index` and `uniq` (= the wire `old_uniq`), `old_index` is the descriptor's
`FunT` position, and `creator` is the fixed `erts_init_process_id` `<0.0.0>` — the runtime supplies
it (faithfulness pinned by `tests/local_fun_encode.rs`).
So `unsupported_local_fun` means exactly "no host identity was installed for this term" — a **host**
property, not a missing wire rule.
`fun.rs:67` is the **one** construction site of `error::unsupported_local_fun`; a fun nested
anywhere (list/tuple/map/literal) reaches the same arm.

## Records (`RECORD_EXT` `67`)
Native records (the OTP 29 `ErtsRecordDefinition`/`ErtsRecordInstance` terms from the `records:*`
BIFs, distinct from tagged-tuple records) encode through `record.rs` +
**`beam_term::record`**; the wire form (`external.c:4006-4036` encode) is
`[67][num_fields:4 BE][flags:1][module atom][name atom]`, then `num_fields` key atoms and then
`num_fields` value terms, **both in declaration order**.
`flags` is `0` (`is_exported == false`) or `1` (`true`); the store is ascending atom-index order,
so the encoder maps declaration → sorted rank through `field_order`
(`erl_record.c:1072-1073`) and the decoder inverts it.
`Encoder::encode` probes `is_record` **before** the boxed-family probes (a record header carries the
distinct `RECORD_SUBTAG`).
A duplicate key or a field count whose definition arity overflows `MAX_ARITYVAL` is `BadArg` on
decode, checked before any allocation.

## Map key encode order (`map.rs` + `mod.rs`)
`encode_map`/`encode_hashmap_pairs` are the sole `MAP_EXT` (`116`) emitters; they **never**
deduplicate or re-canonicalize and both emit `K1,V1,…,Kn,Vn` in whatever order the map's own
representation stores — but which order that is differs by representation.
`with_deterministic()`/`set_deterministic(true)` reproduces the C's `deterministic` dflag
(`DFLAG_DETERMINISTIC`, `parse_t2b_opts` `external.c:1497-1499`/`:1551-1553`): it collects the pairs
and sorts them by **full term order** (C `map_key_compare` = `CMP_TERM`, `external.c:3217-3228`;
here `beam_term::compare::cmp`) before emitting, for BOTH the flat map (`:3745-3770`) and the HAMT
(`:3783-3810`); it is **off by default**.
- **Flat map** (≤ 32 pairs) — emits the map's **stored** pair order verbatim (C walks the flat
  arrays in order, `:3746-3769`, so the wire order *is* the keys-tuple order). That order is the
  **constructor's** job, applied before the term reaches the codec: `beam_term::map::write_map`
  (used by `beam_engine`'s `…_map` opcodes and by this crate's decoder via `term::make_map`) sorts
  ≤ `MAP_SMALL_MAP_LIMIT` (32) keys in `erts_cmp_flatmap_keys` order
  (`erl_utils.h:265-272`: two **atom** keys by atom-table index, every other pair by full term
  order), while a flat map built directly on a heap (`term::make_flatmap`) keeps the caller's order.
- **Hash map** (> 32 pairs, a HAMT) — emits the C encoder's wire order, the **reverse** of the
  ascending node-slot pre-order: `term::map_pairs_storage_order` returns pairs in **ascending**
  node-slot pre-order (slots `0..sz`, descending depth-first at each), while the C encoder
  (`:3745-3823`) pushes each node's slots onto a **LIFO** work stack and pops them
  (`WSTACK_POP` pre-decrement; the collision node takes the same path, `:3415-3427`), visiting
  slots in **descending** index order — exactly `reverse(ascending pre-order)`.
The dispatch probes `flatmap_parts` (flat, ≤ 32) → `encode_map`, then `map_pairs_storage_order`
(hash) → `encode_hashmap_pairs`; each matches exactly one representation.
A **locally built HAMT encodes byte-identically to the oracle** (`beam_term` drives the trie with
the bit-exact `erts_map_hash`, so the node structure matches ERTS's; pinned live for integer keys at
33/40/50/64/100/1000 keys and asserted ERTS-decodable), and a decoded map re-encodes **byte-exactly**
for non-atom keys.
**Atom-keyed multi-key maps are byte-comparable across implementations only when their atom tables
agree**: a flat map's atom keys sort by **atom-table index**, which depends on the order atoms were
first interned *during this VM start*, so the same map can yield different `MAP_EXT` bytes in two
fresh `erl` processes.
**Non-atom** keys need no such care, so the live oracle cross-checks use integer- and
binary-keyed corpora and the atom-keyed checks mirror the oracle's intern order first.
The **encoder must not sort** unless `[deterministic]` is asked: a `make_flatmap`-built map emits the
caller's order verbatim.
The recorded `-eval` atom-keyed-map divergence (`../../../../tools/differential/divergences.md`) is
**atom-table seeding**, never an encoding difference — `tests/map_key_order.rs` pins atom-index order
seeding-independently and a compiled-module literal map (`#{b => 2, a => 1}`) agrees byte-for-byte.

## The `COMPRESSED` (`80`) wrapper rule
Owner: `Encoder::finish_compressed`/`compress_if_smaller` (C `external.c:2276-2298`).
With `body` = the plain image's length − 1 (the `VERSION_MAGIC` byte excluded) and `Z` = the zlib
stream length over exactly those `body` bytes, the image is `[131][80][body:4 BE][zlib]` —
`6 + Z` bytes — **iff `Z <= body - 5`**.
Equality is COMPRESSED (the C compresses into a `body_len - 5`-byte destination and keeps the plain
image only on `Z_BUF_ERROR`, which equality does not raise); `body < 5` can never fit the wrapper
and stays plain; and **level 0 never compresses** (`compress_if_smaller` returns `None` without
calling the compressor, so `[{compressed,0}]` yields the plain image exactly).
The wrapper is top-level only, and `encode_term` itself is never compressed.
The stream is `../compress.rs`'s (a `beam_deflate` delegation, zlib 1.3.2), so a compressed image is
**byte-identical to the oracle's** — wrapper, size field and deflate stream alike.

## Constraints
- Inherited from [`../../CONTEXT.md`](../../CONTEXT.md): `beam_common::{Result, BeamError}` only;
  a value beyond a wire bound (atom name > 255 chars, a tuple/flatmap arity past the encodable
  field, an internal id that does not fit its field, a `>u32` image length) ⇒ `SystemLimit`; heap
  exhaustion ⇒ `BeamError::Alloc`; nothing here panics or silently truncates a value.
- Safe Rust only — raw heap words are reached through `../term/`'s safe wrappers.
- Every builder is **additive and optional**: with nothing installed, `encode_term*` is
  byte-for-byte the unflavoured image.

## Test Strategy
- `mod.rs`'s in-module `#[cfg(test)] mod tests` + `tests.rs` (the encode unit suite and the
  `pub(crate)` helpers); `map.rs`/`fun.rs`/`record.rs`/`float.rs` unit tests pin their own emitters.
- Integration binaries (inventory in `../../tests/CONTEXT.md`): `roundtrip.rs`, `wire_roundtrip.rs`,
  `ground_truth.rs`, `minor_version.rs`, `map_key_order.rs`, `t2b_options.rs` (the `[deterministic]`
  and `[local]` forms, byte-exact vs the oracle), `local_fun_encode.rs`, `record_roundtrip.rs`,
  `internal_ids.rs`, `atom_cache_writer.rs`, `compressed_oracle.rs` (whole-image byte identity),
  `compressed_bytes.rs` (the boundary sweep), `tag_coverage.rs`, `tag_space_audit.rs`,
  `property_roundtrip.rs` + `property_oracle.rs` (encode → decode → encode byte identity under
  `V0`/`V1`/`V2`), `interop.rs`.
- Oracle-dependent cases resolve `$ERTS_ORACLE` else `erl` on `$PATH` and **skip loudly**.

## Routing Table
- `mod.rs` → `Encoder`, `FunIdentity`, `AtomEncoding`, `MinorVersion`, the `encode_term*` entry
  points and `with_*` builders, the private `AtomFlavour` resolution, the private fun-classification
  chain, the map emitters, `compress_if_smaller` + the integer/atom/binary emitters; ~1393 lines
  (one state machine — the tag dispatch plus its per-tag emitters)
- `map.rs` → the `[deterministic]` pair sort `sort_pairs_by_key` + the sorted `MAP_EXT` emitter
- `fun.rs` → the fun encoders + `encode_new_fun_ext` (the `NEW_FUN_EXT` size patch)
- `record.rs` → `Encoder::encode_record`
- `float.rs` → the `FLOAT_EXT` text formatter `format_float_ext`/`normalize_exponent`/`FLOAT_EXT_LEN`
- `tests.rs` → the encode unit suite + the `pub(crate)` helpers
- `../decode/` → the decode direction (sibling: `../decode/CONTEXT.md`)
- `../term/` → the materialization wrappers the readers go through
- `../compress.rs` → the `COMPRESSED` plumbing (`beam_deflate`)
- `../ref_wire.rs` → the reference wire packing the internal-ref emitter uses
- `../../tests/` → the integration binaries (sibling — read-only, escalate writes to `../..`)
- `../../../../../erts/emulator/beam/external.c` → authoritative ground truth (read-only)
