# `beam_loader/src/chunk/` — `.beam` Chunk Parsers

## Intent
Parses every `BEAM` chunk the loader consumes, once, into typed Rust data on `Chunks` (plus the raw directory on `Chunks::raw`).
Owns the *byte layouts* of the container's chunks and the chunk-level accept/reject bounds; the container walk itself is `../container.rs`, the operand/instruction decode is `../decode.rs`, and lowering is `../lower/`.
Ground truth: `../../../../erts/emulator/beam/beam_file.c` (chunk readers), `../../../../erts/emulator/beam/beam_types.c` (the `Type` chunk), `../../../../erts/emulator/utils/beam_makeops`. Read them; never modify them.

## Container (owned by `../container.rs`, summarized here because the walk bounds the chunk readers)
```
u32  "FOR1"                        // magic
u32  form_length                   // = file_size - 8 ; INCLUDES the 4-byte form type
u32  "BEAM"                        // FORM TYPE, not a chunk
chunk entry * :  u8[4] chunk_id | u32 size | size bytes | (4 - size%4)%4 zero pad
```
`"BEAM"` is the form type — there is no `BEAM` chunk.
The walk is bounded by `form_length`, not by the file (`iff_init` requires `file_len >= 8 + form_length` and sets the chunk-view size to `form_length`), so bytes past the form are ignored and only a `form_length` larger than the file truncates.
A chunk whose padded payload does not fit the form ⇒ `BadArg`; unknown ids are skipped; duplicate ids keep the first occurrence (matching `Container::find`); chunk order is not significant.
Chunk ids: `AtU8`, `Code`, `ImpT`, `ExpT` (**mandatory** — an absent one is `BadArg`), then optional `StrT`, `LocT`, `FunT`, `LitT`, `Attr`, `CInf`, `Line`, `Type`, `Meta`, `DbgB`, `Dbgi`, `Docs`, `Recs`, and the obsolete Latin-1 `Atom`.
`classify.rs` classifies every id (`ChunkClass`/`ChunkKind`) as parsed+exposed, parsed-as-raw, or tooling-only with a reason; `Docs`, `DbgB`/`Dbgi` and the obsolete `Atom` are the tooling-only ids.
`tests/chunks.rs` enforces that classification over the installed corpus and requires all 15 corpus ids in its positive controls.

## API Surface
`chunk::parse` dispatches on the id; `Chunks` carries the parsed tables (`atoms`, `code`, `exports`, `imports`, `funs`, `line`, `types`, `literals`, raw `StrT`/`CInf`/`Attr`/`Meta` payloads, `records`) plus `raw: Vec<RawChunk<'a>>` — the whole directory with raw payloads in file order, which the checksum and any other byte-level consumer read (including ids left unparsed, `DbgB`).
### Per-chunk layouts
- **`AtU8`** (`atom.rs`) — `i32 count` (negative ⇒ varint lengths, real count = `-count`); then per atom a length (varint if long, else `u8`) + **UTF-8** bytes. Index 0 is reserved for NIL, index 1 is the module name, real atoms are `1..=count`; total entries = `count+1`. `count >= 1` (`CHECK_ITEM_COUNT`, `beam_file.c:260`). A name over `MAX_ATOM_CHARACTERS` (255 chars) or `MAX_ATOM_SZ_LIMIT` (1020 bytes) ⇒ `SystemLimit` (`atom.c:283-323`).
- **`Code`** (`code.rs`) — `u32 head_size` (== 16 — it counts the four header words that FOLLOW it, so the header is 20 bytes), then `u32 format_number` (must be `BEAM_FORMAT_NUMBER` 0), `u32 max_opcode`, `u32 label_count`, `u32 function_count`, then the instruction stream. `max_opcode` must lie in `[169, 193]` — 169 = `swap/2` (`genop.tab:611`, the first post-OTP-22 opcode), 193 = `MAX_GENERIC_OPCODE` (`update_record_id/7`, the last `genop.tab` entry); outside that window ⇒ `BadArg`, exactly as the C refuses an OTP-22-or-earlier or later-version image. The stream ends at `int_code_end/0` (genop 3), not at size exhaustion; an instruction is one opcode byte (`1..=max_opcode`) + `Opcode::arity` operands. The C decodes **any** opcode byte in `1..=MAX_GENERIC_OPCODE` regardless of the header's own `max_opcode` (that field is only the version gate) and rejects an in-range byte with no specific operation later (`no specific operation found`); this crate lowers such a byte to `Instr::Generic`.
- **`ExpT`** (`table.rs`) — `i32 count` × `{ i32 function_atom, i32 arity, i32 label }` (label = **entry** label).
- **`LocT`** (`table.rs`) — byte-for-byte the `ExpT` layout (the reader reuses the export-table parser, as the C does); the JIT-only local-function table, parsed into `Module.locals`, irrelevant to interpretation.
- **`ImpT`** (`table.rs`) — `i32 count` × `{ i32 module_atom, i32 function_atom, i32 arity }`.
- **`FunT`** (`table.rs`) — `i32 count` × `{ i32 function_atom, i32 arity, i32 label, i32 index, i32 num_free, i32 old_uniq }` (24-byte entries) → one `FunDescriptor` per entry, in file order (position = the `make_fun3` fun-table index). A descriptor whose `label` is not a label in the module ⇒ `BadArg`. Arity is **unbounded on both sides** (`beam_file.c:410-431` checks only the atom index and the label).
- **`Line`** (`line.rs`) — `u32 version` (must be 0, else the chunk is ignored), `u32 flags`, `u32 instr_count`, `u32 item_count`, `u32 name_count`; then packed tagged items, then `name_count` × `{ i16 len, len bytes UTF-8 }`. `item_count` counts **only** the `TAG_i` ("line") items: the loop reads tagged items until `item_count` of them have been `TAG_i`; a `TAG_a <name_index>` sets the current file index and is consumed *in addition* (not counted); a `TAG_i <line>` emits a `LineEntry { file, line }`. `items[0]` is the undefined location `{0,0}`; `files[0]` is synthesized as `<module>.erl`. There is **no separate range-table chunk** — the range table is synthesized at load from this table plus the function labels, so `Module.line` + per-function labels is the loader's complete line-info obligation.
- **`Type`** (`types.rs`) — `u32 version` (`4` current, `3` OTP 27/28; anything else ⇒ fallback), `i32 count` (≥1, `entries[0]` = "any"), then per entry `u16 type_word` (+ optional `i64 lower`, `i64 upper`, `u8 unit-1`, gated by `type_word` bits 13-15). Absent or unsupported version ⇒ a single "any" fallback entry. **Version 3** remaps a `b0`/`b1` entry into the v4-convention word: `extra_bits = (b0 << 1) & 0xE0`, `upper = b0 & 0x0F` (forced to `0x1F` when `0x0F`, so v3's all-ones `0xFFF` becomes the v4 "any" `0x1FFF`), `word = ((upper | extra_bits) << 8) | b1`. `Type::raw` always holds the reconstructed **v4-convention** word (`raw & 0x1FFF` = type union, `raw & 0xE000` = metadata flags).
- **`LitT`** (`literal.rs`) — `u32 uncompressed_size` (`0` ⇒ records follow verbatim; non-zero ⇒ the rest of the chunk is a zlib (RFC 1950) stream inflating to exactly `uncompressed_size` bytes, which ARE the records), then `i32 count` × `{ i32 ext_size, ext_size bytes }` (Erlang external-term bytes). Handles **both** framings — the compressed one via the public `beam_codec::compress::decompress`, rejecting a length that disagrees with `uncompressed_size` — and parses the **framing only** (raw ETF payloads); `../literal::LiteralArea` decodes each payload into a term once at load. Static literals are referenced by `Operand::Literal(idx)`.
- **`StrT` / `CInf` / `Attr` / `Meta` / `Docs` / `DbgB` / `Dbgi`** (`misc.rs`) — `StrT` is captured raw (`Module::strings`). `Attr`, `CInf` and `Meta` are each a single `term_to_binary` payload **decoded into a term** once at load into a private arena shared through an `Arc` (`../decoded::DecodedTerm`): `Attr` → the `[{Atom,Value}]` list (`../attributes.rs`) for `module_info(attributes)`, `CInf` → the compile-info list for `module_info(compile)`, `Meta` → the feature-metadata property list. Each raw payload stays on the module too (`Module::attributes`/`compile_info`/`meta`). `Docs`/`DbgB`/`Dbgi` are tooling-only and deliberately **not** parsed: `Docs`/`Dbgi` are large and near-universal, so eagerly materializing them would regress the measured module-load cost (`tests/load_cost.rs`); `DbgB` is kept only as raw bytes on `Chunks::raw`.
- **`Recs`** (`record.rs`) — `u32 version`, `i32 record_count` (`>= 0`, negative ⇒ `BadArg`), `i32 total_field_count` (informational, `>= 0`), then `record_count` entries, EACH encoded as one BEAM instruction: an opcode byte that must be `Opcode::call_last` (5 — reused only because it has 3 operands), operand 1 = the record-name **atom**, operand 2 = the `true`/`false` export-flag **atom**, operand 3 = a `z,1` **argument list** (`u` element count `N`, must be **even**, then `N/2` `(field-name atom, default)` pairs). Field defaults are classified through the **raw** operand tag, not via `decode_operand`: `u` = **no default** (the `make_catch(0)` sentinel), `i` = a signed small integer, a wide `i` = a signed **bignum**, `a` = an atom (`0` ⇒ `nil`), `z,4` = a `LitT` literal index. The record's module is the module being loaded (the chunk carries none).

## API Surface — the raw `Recs` types
- `pub fn record::parse(data: &[u8], atoms: &[String]) -> Result<Vec<RawRecord>>` — `atoms` (the `AtU8` names, index 0 = NIL placeholder) recognizes the `true`/`false` export flag by name. A non-zero version yields an **empty vector** (the C instead rejects it, `BEAMFILE_READ_CORRUPT_RECORD_TABLE` — see Known Issues); a negative count, a wrong opcode byte, a non-atom name, an odd field-list length, an unsupported operand tag or a truncated payload ⇒ `BeamError::BadArg`.
- `pub struct RawRecord { pub name: u32, pub is_exported: bool, pub fields: Vec<RawRecordField> }`, `pub struct RawRecordField { pub name: u32, pub default: RawRecordDefault }` — **wire** atom indices, not interned atoms; the lowered form is `Module.records`. Carried on `Chunks.records` (file order; empty when absent).
- `pub enum RawRecordDefault { Absent, Integer(i64), Atom(u32), Literal(u32), BigInteger { negative: bool, magnitude: Vec<u8> } }` — a default in the exact form the file encodes it (`Absent` = `u,0`; `Atom(0)` = `nil`/`[]`; `Literal(i)` = a `z,4` index into the static `LitT`; `BigInteger.magnitude` is the BE two's-complement field).

## Constraints
- `beam_common::{Result, BeamError}` only; no panic, no `unwrap()` on file data, never silently accept a malformed image.
- `#![deny(unsafe_code)]` (crate-wide); the big-endian cursor is `beam_codec::Reader` — do not write a second BE reader.
- Chunk data is source-of-truth input: layouts come from the C readers above, never guessed.
- Files ~1000 lines, one chunk family per file.

## Chunk-level bounds (all in this subtree, all typed)
- `ExpT`/`LocT`/`ImpT` arity in `0..=MAX_ARG` (255, `beam_file.c:335,373`) ⇒ `SystemLimit`; a negative `ExpT`/`LocT` label ⇒ `BadArg` (`:374`).
- `AtU8` `count >= 1` (`:260`) plus the two atom-name caps (`atom.c:283-323`) ⇒ `SystemLimit`.
- A negative `Line` item value (`:519-522`) ⇒ `BadArg`.
- `Type` semantics of `beam_types.c:44-82` — `flags == 0`, a lower/upper bound without `INTEGER|FLOAT`, a unit without `BITSTRING`, `entries[0]` not `BEAM_TYPE_ANY`, `count < 1` — ⇒ `BadArg`, so the fallback table exists only for an absent or unsupported-version chunk.
- A `FunT` entry may neither repeat a label (`jit/asm_load.c:150-165`) nor use label 0 (`beam_file.c:429`).
- `tests/negative_tables.rs` pins every one of these as an end-to-end `load()` failure beside an unmutated control image.

## Known Issues
- **A `Recs` `version != 0` is a success with no records here, where the C rejects the chunk** (`:1333-1341`). Accepted divergence, malformed images only.
- **A `Recs` `total_field_count < 0` is rejected here; the C stores it unchecked** (it is informational). Stricter, malformed images only.
- **`LocT` is always parsed here; the C parses it only under `erts_jit_asm_dump`** (`beam_file.c:1684`). Stricter, malformed images only.
- **`Attr`/`CInf`/`Meta` are decoded eagerly at load, so a malformed payload fails the load**; the C defers decoding to query time.
- **A compressed `LitT` whose declared `uncompressed_size` exceeds the inflated length is rejected**; the C uses zlib's actual length, so only a too-*small* declaration fails there.
- **The `DbgB` body is never parsed** — its raw bytes stay on `Chunks::raw` for the checksum and are dropped once `build_module` assembles the `Module`, so neither its internal validation (`beam_file.c:887-1057`) nor `debug_line`'s `DbgB` item index (`asm_load.c:719-724`) is checked. `code:get_debug_info/1` parity (A.8) would need the payload exposed on `Module`.
- **`Docs`/`Dbgi` are not materialized** (deliberate — see the load-cost reason above).

## Notes for Agents
- Rule-by-rule doc-comments live on each parser (`src/chunk/<file>.rs`); read those, not this file.
- `beam_lib` is a chunk reader, not a loader: it accepts a bad `format_number`/`max_opcode`/opcode bytes and answers `{error,beam_lib,{not_a_beam_file,_}}`/`{missing_chunk,_,Id}`/`{chunk_too_big,_}`/`{invalid_chunk,_,Id}` for container damage (and *raises* on a shortened `AtU8`) — do not treat it as the accept/reject oracle.

## Routing Table
- `mod.rs` → `parse` dispatch + `Chunks` (+ `raw`/`RawChunk`) + the shared `decode_terms*`/arena-retry helpers.
- `atom.rs` → `AtU8` (`AtU8` names, count convention, caps).
- `code.rs` → `Code` header + the `max_opcode` version window.
- `table.rs` → `ExpT`/`LocT`/`ImpT`/`FunT` (`table::parse` shared by `LocT`).
- `line.rs` → `Line` table (tagged items, synthesized `files[0]`).
- `types.rs` → `Type` chunk + v3→v4 word remap + the fallback table.
- `literal.rs` → `LitT` framing (verbatim + zlib) → raw ETF payloads.
- `misc.rs` → `StrT`/`CInf`/`Attr`/`Meta` raw capture.
- `record.rs` → `Recs` parser (`RawRecord`/`RawRecordField`/`RawRecordDefault`).
- `classify.rs` → `ChunkClass`/`ChunkKind`/`classify` (the whole-id classification `tests/chunks.rs` enforces).
