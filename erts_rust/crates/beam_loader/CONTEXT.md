# `beam_loader` — `.beam` Reader & Typed-IR Lowering Front-End

## Intent
Owns the `.beam` **container reader**, the **BEAM instruction decoder**, and the **lowering into the typed, block-structured IR** that `beam_engine` (wave-4 sibling) interprets.
Implements compatibility contract **A.2** (`.beam` binary surface, `../../CONTEXT.md` §1) and the Stage-1 loader half of Part II §E/§H (IR seam, §4 of `../CONTEXT.md`).
On the **critical path of the vertical slice** (load + run a real `.beam`), so container/instruction decode correctness is the highest priority.
Ground truth = the legacy C reader `../../erts/emulator/beam/beam_file.c` (container + operand decoding + function grouping), `../../erts/emulator/beam/beam_load.c` + `emu/ops.tab` (transforms), the operand-tag/opcode generator `../../erts/emulator/utils/beam_makeops`, the opcode roster `../../lib/compiler/src/genop.tab`, and the writer `../../lib/compiler/src/beam_asm.erl`. Read them; never modify them.

## API Surface (FROZEN — `beam_engine` designs against this)
Two entry points, both in `src/load.rs`: `pub fn load(bytes: &[u8]) -> Result<Module>` and `pub fn load_file(path: impl AsRef<Path>) -> Result<Module>` (slurp + `load`).
These are the *only* entry points; every intermediate type is public for inspection/testing.
Everything else is re-exported at the crate root and via `beam_loader::prelude::*`.
**`src/CONTEXT.md` holds the full frozen API** — every field list, the identifier types, the operand-tag table, the decode bounds and the crate's design decisions. Read it before touching the seam.
### The IR in brief (field lists in `src/CONTEXT.md`)
- `Module { name, atoms, atom_table, exports, locals, imports, functions, funs, records, code_version, types, line, literals: Arc<LiteralArea>, strings, attributes + attribute_table, compile_info + compile_info_table, meta + meta_table, checksum }` with accessors `atom_name`, `export`, `function`, `literal`/`literals`, `checksum`, `fun`/`funs`, `records`/`record`/`record_by_name`, `attributes_term`/`attribute_by_name`, `compile_info_term`/`compile_info_by_name`, `meta_term`/`meta_by_name` — all `None` on an absent/malformed chunk, never a panic.
- `Export { function, arity, label }` (`label` = the **entry** label; `Module.locals` = one `Export` per `LocT` entry) · `Import { module, function, arity }` · `Function { func_label, entry_label, module, name, arity, code, blocks, labels }` (`blocks` is the label-delimited decomposition, `labels` a label→index map; every IR field follows the **file's** operand order) · `Block { label, start, end }`.
- `CodeVersion { format_number, max_opcode, label_count, function_count }` · `TypeTable { entries, fallback }` / `Type { raw, lower, upper, unit }` · `LineTable { flags, instruction_count, items, files }` / `LineEntry { file, line }`.
- `FunDescriptor { function, arity, label, index, num_free, uniq }` (position in `Module.funs` = the fun-table index) · `RecordDefinition { name, is_exported, fields }` / `RecordField { name, default: Option<Eterm> }`.
- Identifiers: `Atom` (= `beam_term::Atom`, an index into the crate-owned table, not the wire index) · `Label(u32)` + `NO_FAIL = Label(0)` · `Reg::{X,Y}` · `TypeIndex(u32)` (`0` = "any") · `ImportRef(u32)` · `LineRef = u32` (index into `Module.line.items`, `items[0]` = undefined location).
- `Operand { X, Y, Integer, Atom, Nil, Label, Character, FloatReg, Literal, Typed { reg, ty }, List, Allocation, BigInteger }` — `BigInteger` is decode-time-only (rewritten to a synthesized `Operand::Literal`); `HeapNeed::{Words, Alloc(Vec<AllocKind>)}` keeps both wire encodings of a frame instruction's need.
- `Module` is `Clone` + `Debug` but **neither `Send` nor `Sync`** (it owns the `LiteralArea`), and `Module::clone` is Θ(module size) because `AtomTable::clone` is — hence `beam_rt`'s code server caches `Arc<Module>`.
- `pub enum Instr` — 108 variants (107 named + the lossless fallback `Generic { op, operands, offset }`), one per modelled opcode; `pub enum TestKind` — the 25-member `is_*`/`test_arity` classification (`Instr::Test { kind, fail, args }`).
- `pub enum Opcode` — one variant per generic opcode of the full `genop.tab` roster, with `from_u8`/`as_u8`/`name`/`arity`, complete and lossless so an unmodelled instruction round-trips as `Instr::Generic`.
- `LiteralArea`/`SharedLiteralArea` — the `LitT` literals materialized into one immutable, literal-tagged `beam_alloc::Heap` at load; `Module::literal(idx)` resolves `Operand::Literal(idx)`. `LiteralArea` (and therefore a loaded `Module`) is **not `Send`/`Sync`**.
- `Module.checksum: [u8; 16]` — the module MD5, computed at load and precomputed for the `NEW_FUN_EXT` `uniq` field, so no `.beam` re-read is ever needed.

## The `.beam` in brief (byte layouts and bounds in `src/chunk/CONTEXT.md`)
`u32 "FOR1" | u32 form_length (= filesize-8, includes the form type) | u32 "BEAM"` then `chunk entry*: u8[4] id | u32 size | payload | 0-3 pad`.
`"BEAM"` is the form type, not a chunk; the walk is bounded by `form_length` (bytes past it are ignored, a longer one truncates).
Chunks: `AtU8`, `Code`, `ImpT`, `ExpT` (**mandatory**) plus optional `StrT`, `LocT`, `FunT`, `LitT`, `Attr`, `CInf`, `Line`, `Type`, `Meta`, `DbgB`, `Dbgi`, `Docs`, `Recs`, obsolete `Atom`; unknown ids skipped, duplicate ids keep the first, order not significant.
The instruction stream ends at `int_code_end/0`, not at chunk-size exhaustion; an instruction is one opcode byte + `Opcode::arity` tagged operands (`Operand`'s tag table and every decode bound are in `src/CONTEXT.md`).
Line info is one `Line` table plus per-function labels — there is no separate range-table chunk.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum. Malformed/truncated container, bad magic, bad format number, unknown opcode tag, out-of-range atom/label/register index, structurally wrong operand, unsupported `z` sub-tag ⇒ `BeamError::BadArg`; a size/arity/count bound (arity > 255, more than `ERTS_BEAM_MAX_OPARGS`, negative chunk size, atom name not UTF-8-representable, an inline operand wider than 8 magnitude bytes) ⇒ `BeamError::SystemLimit`.
- **Never panic, never `unwrap()` on file data, never silently accept a malformed image.** A construct the C rejects is a typed error here too.
- **`unsafe` is forbidden in this crate** (`#![deny(unsafe_code)]`) — the loader is the *safe periphery*; zero-copy is achieved safely via `beam_codec::Reader::bytes(n) -> Result<&'a [u8]>`.
- **Reuse, don't duplicate.** The big-endian byte cursor is `beam_codec::Reader` (the BE-bounds LCA) — do **not** write a second BE reader; the MD5 is `beam_common::md5::digest` — the crate exposes no MD5 of its own.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_codec`, `beam_alloc`, `libc`. Adding a dependency edge requires reporting to the crate-layer node (`../`).
- **Generated data is source, not output.** The opcode roster and operand-tag table come from `genop.tab`/`beam_makeops`; the crate embeds a **regenerable** table — never hand-guess a byte value.
- **Files ~1000 lines**, one responsibility per file. `Instr` is one flat module (`src/instr.rs`) — an enum cannot be split across submodules. Licensing Apache-2.0.

## Invariants other crates depend on
- **BIF operands are `ImpT` indices, NOT `bif.tab` numbers.** The `gc_bif*` BIF operand is the index of `{module,function,arity}` in `ImpT`; the IR stores `ImportRef` and the engine resolves `Module.imports[bif]`. **CROSS-CRATE CONTRACT (binding on `beam_bifs`):** the engine/BIF layer keys BIFs by `(module, function, arity)` following real `bif.tab` naming — the loader does **not** know and must **not** encode `bif.tab` numbering. `Module.imports` is the single source of BIF identity.
- **A decoded literal is self-contained inside the one arena** — nothing in a literal term points out of it (a literal binary stays an `ErlHeapBits` block at any size, a fun inlines its descriptor), so a literal `Eterm` needs only the owning `Module` alive. Detail: `src/CONTEXT.md`.
- **The literal arena is tagged `TAG_LITERAL_PTR` at one call site, and that tagging is load-bearing.** An untagged live literal root makes `beam_rt`'s collector decline the collection (`context::gc::root_is_unscanned`), so a copy-bound loop whose only live root is a module literal never collects and its heap grows linearly (measured ≈0.33 GC/op + flat `heap_size` ≈700 words tagged, vs `heap_size = 466 + 94·N` untagged). See `src/CONTEXT.md`, `../beam_term/CONTEXT.md`, `../../tools/bench`.
- **Atoms are interned in `AtU8` file order into a per-module `AtomTable`**; `Module.atoms[0]` is `Atom::MAX` (NIL placeholder). Interning into a shared `atom.names`-seeded table (preloaded/positional atoms) is `beam_rt`'s job, and there is no dedicated entry point for it — the manual pipeline is documented in `src/CONTEXT.md`.
- **`FunT` ordering and shape are part of the contract:** `Module.funs` keeps file order (the `NEW_FUN_EXT` `old_index` is that position), `FunDescriptor.arity` is the lambda's FULL arity (exposed arity = `arity - num_free`), and `FunDescriptor.uniq` is the 4-byte `old_uniq` while the 16-byte `uniq` is `Module::checksum()`. Field-by-field mapping: `src/CONTEXT.md`.
- **`Recs` keeps declaration order** and distinguishes the file's absent default (`u,0` → `None`) from a literal `0` (`i,0` → `Some(0)`).
- **The `Code` header's `max_opcode` is a version gate only** — it must lie in `[169, 193]`; decode still accepts any byte in `1..=MAX_GENERIC_OPCODE`, exactly as the C does.

## Design Decisions
- **The opcode roster is embedded from `genop.tab`** (the checked-in, wire-frozen roster the compiler emits; `format_number` bumps when it changes), derived verbatim with every entry kept so the mapping is bijective. Regeneration = re-read the file; the numbers are its first field. Never hand-guess a byte value.
- **The checksum is computed inside `build_module` from the retained raw chunk directory** (`Chunks::raw`), so the public `load` and every manual sibling pipeline (`beam_rt`/`beam_engine`) get the digest for free, with no signature change and no `.beam` re-read.
- **Basic blocks are built after the one fusion pass**, so `Function.labels`/`blocks` index exactly the vector the engine dispatches on; CFG construction stays the engine's concern.
- **The loader is the safe periphery:** `#![deny(unsafe_code)]`, zero-copy via `beam_codec::Reader`, term materialization only through `beam_codec::decode_term` + `beam_alloc::Heap`. Raw-word writing stays in the engine/GC/tagging crates.
- **Opcode arity is table-indexed and the decode vector is pre-sized** (`OnceLock<[u8; 256]>` from the roster; capacity from `stream.len()/4`, capped) — both measurement-backed by `tests/load_cost.rs`; re-measure there before optimizing further.

## Test Strategy
- **GREEN required:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader` and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`. Always through `erts_rust/scripts/cargo.sh` (`$HOME` read-only), never `cd`, never touch `../tools/` or `../../erts/`.
- **Ground truth is derived, never guessed** (`beam_disasm:file(...)` + a byte decode); fixtures are checked-in `.erl` + oracle `.beam` compiled `+deterministic` from `$TMPDIR`; every rejection test sits beside an unmutated control image; a test needing `erl`/`erlc` **skips loudly**, never silently.
- **Breadth:** `tests/real_modules.rs` loads all **34 apps / 1366 modules** of a generically-discovered `lib` root (no allowlist); `tests/generic_census.rs` pins the residual `Instr::Generic` set; `tests/chunks.rs` pins the whole-corpus chunk-id classification; `tests/checksum.rs` cross-checks `beam_lib:md5/1`; `tests/fun_name.rs`/`tests/records.rs` cross-check the live `erl`.
- **Performance discipline:** module-load cost is measured by `tests/load_cost.rs` and quoted from there only — never carry stale figures in prose.
- Per-test-file scope, the fixture inventory and the path-free guard are in `tests/CONTEXT.md`.
- Cross-checks against the real system ERTS as a *compatibility gate* belong to the sibling `../tools/differential/` harness.

## Known Issues / Intentional M1 gaps
Every item is a typed error or a documented accept/reject divergence — never a wrong result.
- **`Instr::Generic` fallback: 134 of 193 opcodes typed, 59 Generic.** 109 lower to a named `Instr`, 25 to `Instr::Test`; the other 59 are exactly the `-`-prefixed obsolete/transform-only roster entries the compiler never emits (`is_constant/2`, `make_fun2/1`, `allocate_zero/2`, the legacy `bs_*` forms, …). The residual census over the whole installed OTP `lib` root is **empty** (`tests/generic_census.rs`), and `Generic` is reached only from the single catch-all arm of `lower/instr.rs`.
- **Opcodes 192/193 are unreachable from the on-PATH OTP-29 `erlc`** (OTP-30 additions) — covered by `lower/instr.rs` unit tests only. Every other variant, including the native-record family 186-191, `executable_line`/`debug_line` and `bs_test_tail2`, has a committed fixture proving reachability, and their *execution* is oracle-checked at the `../tools/differential` gate.
- **Lowering is one peephole pass, not a full `ops.tab` transform.** The only rewrite is `Deallocate(N); Return` → `Instr::DeallocateReturn(N)`; an audit of the C transform surface (`beam_load.c` + the `=>` transforms) concluded the current surface is **complete for correctness** — every missing C rewrite is performance-only or is reproduced at runtime by the engine. The sole genuine capability gap is `put_record/6`'s **foreign** create form (engine-side typed `unsupported`; the local form works). Detail: `src/lower/CONTEXT.md`.
- **Load-time validation follows the C reader's accept/reject rules**, with a short list of malformed-image-only divergences (both stricter and looser) at `src/chunk/CONTEXT.md` and `src/lower/CONTEXT.md`. Load-bearing examples: a defined label 0 is refused (the C tests only `< 0 || >= label_count`); obsolete/transform-only opcodes lower to `Instr::Generic` instead of `PLEASE_RECOMPILE`; an `f` target no `label` defines is accepted and surfaces as a typed execution error.
- **The `DbgB` body is never parsed**, so `code:get_debug_info/1` parity (A.8) would need the payload exposed on `Module`; `Docs`/`Dbgi` are deliberately not materialized (it would regress the measured load cost).
- **A map-bearing `LitT` payload is NOT byte-preservable** — a flat map is stored in `erts_cmp_flatmap_keys` (atom-index) order while `+deterministic` writes name-lexicographic order, so only a **map-free** literal may be asserted byte-exact. Faithful behaviour, not a defect. Detail: `src/CONTEXT.md`.
- **The oracle's only Erlang-observable rejection is the bare atom `badfile`** — from `code:load_binary/3`, `erlang:prepare_loading/2`, `code:load_abs/1` and `code:load_file/1` alike; `erts_internal:beamfile_module_md5/1` reports `undefined`. The detailed reason (`corrupt file header`, `invalid opcode`, `invalid label num 3 (0 < label < 3)`, `missing export table`, `no specific operation found`, …) exists **only** as an asynchronous legacy `error_logger` report to the caller's group leader, so no finer error class is expressible or needed here. `beam_lib` is a chunk reader, not a loader, and `code:get_object_code/1` returns raw bytes unvalidated.
- **Structural debt:** `src/lower.rs` (2150 lines), `src/decode.rs` (1680), `src/lower/instr.rs` (1288) and `src/instr.rs` (1207) exceed the ~1000-line baseline — single-responsibility but large; split before growing them (`instr.rs` stays long by construction: an enum cannot be split).

## Notes for Agents
- **Read the child CONTEXT.md that owns the rule**, not this file: the frozen API and the operand-tag/decode-bound tables are in `src/CONTEXT.md`; per-chunk byte layouts and chunk-level bounds in `src/chunk/CONTEXT.md`; opcode→IR semantics, grouping, fusion and validation in `src/lower/CONTEXT.md`; fixtures and per-test scope in `tests/CONTEXT.md`.
- Rule-by-rule doc-comments live on the code (`src/lower/instr.rs`, `src/decode.rs`, `src/operand.rs`, `src/chunk/*.rs`) — read those for details.
- Run cargo only through `erts_rust/scripts/cargo.sh` with `--manifest-path erts_rust/Cargo.toml` (`$HOME` is read-only; a bare `cargo` from the repo root cannot resolve the workspace).
- The system Erlang toolchain (OTP 29 / erts-17.1) is on `PATH`; `erlc` produces fixtures and `erl` is the oracle.

## Routing Table
- `src/` → the frozen API layer: container walk, opcode roster, operand/instruction decoder, IR types, `Instr` vocabulary, literal area, checksum, attributes/decoded-term tables, `load`, and `build_module` (`src/CONTEXT.md` carries the full API surface, the operand-tag table, decode bounds and the design decisions)
- `src/chunk/` → the `.beam` chunk parsers (`AtU8`/`Code`/`ExpT`/`LocT`/`ImpT`/`FunT`/`Line`/`Type`/`LitT`/`Recs`/`StrT`/`CInf`/`Attr`/`Meta`), the chunk-id classification, and every chunk-level accept/reject bound (`src/chunk/CONTEXT.md`)
- `src/lower/` → opcode → `Instr` mapping (`lower_instr`), `func_info` function grouping, the `Deallocate`+`Return` fusion, and post-lowering validation (`src/lower/CONTEXT.md`)
- `tests/` → fixtures, rejection suites, whole-corpus sweeps, oracle cross-checks and the load-cost measurement (`tests/CONTEXT.md`)
- `tests/fixtures/` → the committed `.erl` sources + oracle `.beam` images, with the regeneration recipe and wire summary in `README.md`
- `tests/common/` → the `tests/`-level support module (`mod.rs`: installed-OTP discovery + raw ETF payload inspection)
- `../../erts/emulator/beam/beam_file.c` → container + operand decoding + function grouping (read-only reference)
- `../../erts/emulator/beam/beam_load.c`, `emu/ops.tab`, `beam_transform_helpers.c` → the C load-time transform surface (read-only reference)
- `../../lib/compiler/src/genop.tab` → the authoritative opcode roster (read-only); `../../lib/compiler/src/beam_asm.erl` → the compiler-side writer (read-only)
- `../../erts/emulator/utils/beam_makeops` → the operand-tag generator the wire format derives from (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the IR seam (`beam_loader` → `beam_engine`), dependency waves (§4, wave 2).
- `../../CONTEXT.md` §1 A.2 / §4 → the `.beam` binary surface and the execution-engine IR seam.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §E → loader/IR design rationale.
- `../beam_codec/CONTEXT.md` → the ETF codec whose `Reader`/`decode_term`/`compress` this crate reuses; same error strategy.
- `../beam_alloc/CONTEXT.md` → the `Heap` the decoded literal area owns; `../beam_term/CONTEXT.md` → `Atom`/`AtomTable`/`Eterm` and the literal tagging (`literal.rs` / `TAG_LITERAL_PTR`).
- `../beam_engine/CONTEXT.md` → the consumer of this IR; `../beam_rt/CONTEXT.md` → the code server that caches `Arc<Module>`.
- `../../tools/bench` → the measured copy-bound loop numbers behind the literal-tagging rationale.
