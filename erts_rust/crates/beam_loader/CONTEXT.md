# `beam_loader` — `.beam` Reader & Typed-IR Lowering Front-End

## Intent
Owns the `.beam` **container reader**, the **BEAM instruction decoder**, and the **lowering into the
typed, block-structured IR** that `beam_engine` (wave-4 sibling) interprets. Implements
compatibility contract **A.2** (`.beam` binary surface, workspace `../../CONTEXT.md` §1) and the
Stage-1 loader half of Part II §E/§H (IR seam, §4 of `../CONTEXT.md`).
This crate is on the **critical path of the vertical slice** (load + run a real `.beam`), so
container/instruction decode correctness is the highest priority.
Ground truth = the legacy C reader `../../erts/emulator/beam/beam_file.c` (container + operand
decoding + function grouping), the operand-tag/opcode generator `../../erts/emulator/utils/beam_makeops`,
the opcode roster `../../lib/compiler/src/genop.tab`, and the compiler writer
`../../lib/compiler/src/beam_asm.erl`. Read them; never modify them.

## API Surface (FROZEN — `beam_engine` designs against this)
Re-exported at the crate root and via `beam_loader::prelude::*`.
### Entry points (`src/load.rs`)
- `pub fn load(bytes: &[u8]) -> Result<Module>` — parse a `.beam` image end to end.
- `pub fn load_file(path: impl AsRef<std::path::Path>) -> Result<Module>` — slurp + `load`.
Both are the *only* entry points; all intermediate types are public for inspection/testing.
### Identifiers (`src/ir.rs`, `src/operand.rs`)
- `pub type Atom = beam_term::Atom;` — an index into the crate-owned `AtomTable` (NOT the wire index).
- `pub struct Label(pub u32);` + `pub const NO_FAIL: Label = Label(0);` (the `f,0` failure sentinel).
- `pub enum Reg { X(u32), Y(u32) }`
- `pub struct TypeIndex(pub u32);` — index into `Module.types`; `0` = the "any type" entry.
- `pub struct ImportRef(pub u32);` — index into `Module.imports`.
- `pub type LineRef = u32;` — index into `Module.line.items` (`items[0]` = undefined location).
- `pub enum Operand { X(u32), Y(u32), Integer(i64), Atom(Atom), Nil, Label(Label), Character(u32),
  FloatReg(u32), Literal(u32), Typed { reg: Reg, ty: TypeIndex }, List(Vec<Operand>),
  Allocation(Vec<AllocKind>), BigInteger { negative: bool, magnitude: Vec<u8> } }` — a *typed* `.beam`
  operand.
  `Operand::BigInteger` is a **decode-time-only** form — the C `TAG_o` overflow state for an `i` operand
  wider than a machine word (`beam_file.c:207-237`); `lower::build_module` rewrites it to a synthesized
  `Operand::Literal` (a bignum literal), so a loaded `Module` never carries it.
  `Operand::Literal(idx)` is an index into the module's decoded [`LiteralArea`], resolved through `Module::literal`.
- `pub struct AllocKind { pub kind: u32, pub number: u32, pub words: u32 }` — one decoded
  `z,3` allocation entry, `words` per `marshal_allocation_list` (`beam_file.c:2259-2284`): kind `0`
  (raw words) → `number`, kinds `1` (floats) / `2` (funs) / `3` (native records) → `2 × number`
  (`FLOAT_SIZE_OBJECT` / `ERL_FUN_SIZE` / `RECORD_INST_SIZE(0)`, all `2` on 64-bit).
- `pub enum HeapNeed { Words(u32), Alloc(Vec<AllocKind>) }` — the heap/stack-need operand of the frame
  instructions, in *either* wire encoding (a plain word count or a `z,3` allocation list), with
  `HeapNeed::words(&self) -> u32` (the saturating word total) and
  `HeapNeed::from_operand(&Operand) -> Result<HeapNeed>`. It is the field type of
  `Instr::TestHeap { heap_need, live }`, `Instr::AllocateHeap { stack_need, heap_need, live }` and
  `Instr::Allocate { stack_need, live }`.
### The IR (`src/ir.rs` + the re-exported `src/instr.rs`)
- `pub struct Module { pub name: Atom, pub atoms: Vec<Atom>, pub atom_table: AtomTable,
  pub exports: Vec<Export>, pub locals: Vec<Export>, pub imports: Vec<Import>, pub functions: Vec<Function>,
  pub funs: Vec<FunDescriptor>, pub records: Vec<RecordDefinition>, pub code_version: CodeVersion,
  pub types: TypeTable, pub line: LineTable, pub literals: Arc<LiteralArea>, pub strings: Vec<u8>,
  pub attributes: Option<Vec<u8>>, pub attribute_table: SharedAttributeTable, pub compile_info: Option<Vec<u8>>,
  pub compile_info_table: SharedDecodedTerm, pub meta: Option<Vec<u8>>, pub meta_table: SharedDecodedTerm }`
  + `Module::atom_name(&self, Atom) -> Option<&str>`, `Module::export(&self, Atom, u32) -> Option<Label>`,
  `Module::function(&self, Atom, u32) -> Option<&Function>`, `Module::literal(&self, u32) -> Option<Eterm>`,
  `Module::literals(&self) -> &LiteralArea`, `Module::fun(&self, u32) -> Option<&FunDescriptor>`,
  `Module::funs(&self) -> &[FunDescriptor]`, `Module::records(&self) -> &[RecordDefinition]`,
  `Module::record(&self, Atom) -> Option<&RecordDefinition>`,
  `Module::record_by_name(&self, &str) -> Option<&RecordDefinition>`,
  `Module::attributes_term(&self) -> Option<Eterm>` (the decoded `Attr` list) and
  `Module::attribute_by_name(&self, &str) -> Option<Eterm>`,
  `Module::compile_info_term(&self) -> Option<Eterm>` / `Module::compile_info_by_name(&self, &str) -> Option<Eterm>`
  (the decoded `CInf` compile-info list — what `module_info(compile)` serves) and
  `Module::meta_term(&self) -> Option<Eterm>` / `Module::meta_by_name(&self, &str) -> Option<Eterm>`
  (the decoded `Meta` feature-metadata property list) — all `None` when the chunk is absent/malformed,
  never a panic.
  `records` is the lowered `Recs` table (native-record definitions, declaration order); `record_by_name`
  resolves a *runtime* atom by looking its string up in the module's `atom_table` first (the lookup the
  `records:get_definition/2` BIF needs).
  `atoms[i]` = the atom for wire index `i`; `atoms[0]` = `Atom::MAX` (NIL placeholder, never a real atom).
  `Module` is `Clone` (cheap: `literals`/`attribute_table`/`compile_info_table`/`meta_table` are shared
  behind `Arc`s) and `Debug`, but it is **neither
  `Send` nor `Sync`** (see `src/literal.rs` / Design Decisions) — a loaded module stays on its loading thread.
- `pub struct Export { pub function: Atom, pub arity: u32, pub label: Label }` — `label` = the function **entry label**.
  `Module.locals` holds one `Export` per `LocT` entry (byte-for-byte the `ExpT` layout — the JIT-only
  local-function table; the C keeps it only for `erts_jit_asm_dump`, `beam_file.c:1684`, so interpretation
  ignores it, but the chunk is recognized and parsed rather than silently skipped).
- `pub struct Import { pub module: Atom, pub function: Atom, pub arity: u32 }`
- `pub struct Function { pub func_label: Label, pub entry_label: Label, pub module: Atom,
  pub name: Atom, pub arity: u32, pub code: Vec<Instr>, pub blocks: Vec<Block>,
  pub labels: std::collections::BTreeMap<Label, usize> }`
  `func_label` = the `label` **before** `func_info`; `entry_label` = the `label` **after** it (what
  `Export.label` points to) — both per `beam_file.c:2527-2589`. `labels` maps every label in the
  function to its index in `code`; `blocks` is the label-delimited decomposition (`blocks[k]` is
  `code[start..end]` beginning at `blocks[k].label`).
- `pub struct Block { pub label: Label, pub start: usize, pub end: usize }`
- `pub struct CodeVersion { pub format_number: u32, pub max_opcode: u32, pub label_count: u32,
  pub function_count: u32 }` (from the `Code` chunk header; `format_number` must be `0`).
- `pub struct TypeTable { pub entries: Vec<Type>, pub fallback: bool }`,
  `pub struct Type { pub raw: u16, pub lower: Option<i64>, pub upper: Option<i64>, pub unit: Option<u8> }`
- `pub struct LineTable { pub flags: u32, pub instruction_count: u32, pub items: Vec<LineEntry>,
  pub files: Vec<String> }`, `pub struct LineEntry { pub file: u32, pub line: u32 }`.
- `pub struct FunDescriptor { pub function: Atom, pub arity: u32, pub label: Label, pub index: u32,
  pub num_free: u32, pub uniq: u32 }` — one `FunT` lambda descriptor; `Module::fun(i)` selects the
  entry whose `make_fun3`/`call_fun2` *fun-table index* is `i` (its position in `Module::funs`).
- `pub struct RecordDefinition { pub name: Atom, pub is_exported: bool, pub fields: Vec<RecordField> }`
  — one `Recs` **native-record** definition (`-record #name{…}`), i.e. what the
  `records:get_definition/2` BIF serves as `{#{is_exported => Bool}, Fields}`. `name` is the record-name
  atom; `fields` is in **declaration order** (the file order — never sorted).
  `pub struct RecordField { pub name: Atom, pub default: Option<Eterm> }` — `None` is the file's
  **absent** sentinel (`u,0`), which is *distinct* from a field whose default is literally `0`
  (`i,0` ⇒ `Some(<small 0>)`); see the `Recs` default rule under Design Decisions.
### The raw `Recs` chunk (`src/chunk/record.rs`)
- `pub fn record::parse(data: &[u8], atoms: &[String]) -> Result<Vec<RawRecord>>` — parses a `Recs`
  payload; `atoms` (the `AtU8` names, index 0 = NIL placeholder) is needed to recognize the `true`/`false`
  export flag by name, exactly as the C compares against `am_true`/`am_false`. A non-zero version yields
  an empty vector ("no record table", not an error); a negative count, a wrong opcode byte, a non-atom
  name, an odd field-list length, an unsupported operand tag or a truncated payload ⇒ `BeamError::BadArg`.
- `pub struct RawRecord { pub name: u32, pub is_exported: bool, pub fields: Vec<RawRecordField> }`,
  `pub struct RawRecordField { pub name: u32, pub default: RawRecordDefault }` — **wire** atom indices,
  not interned atoms; the lowered, atom-interned form is `Module.records`.
- `pub enum RawRecordDefault { Absent, Integer(i64), Atom(u32), Literal(u32),
  BigInteger { negative: bool, magnitude: Vec<u8> } }` — a field default in the exact form the file
  encodes it (`Absent` = `u,0`; `Atom(0)` = `nil`/`[]`; `Literal(i)` = a `z,4` index into the module's
  static `LitT` literals; `BigInteger.magnitude` is the big-endian two's-complement field).
- The parsed raw set is carried on `Chunks.records` (in file order; empty when the chunk is absent).
  See `record::parse`'s sibling list types in `src/chunk/mod.rs`.
### The literal area (`src/literal.rs`)
- `pub struct LiteralArea` — the `LitT` literals materialized into **one immutable heap** at load time;
  fields are private (`heap: Option<Heap>`, `terms: Vec<Eterm>`), with a manual `Debug` and a `Default`
  (empty). API: `LiteralArea::empty()`, `LiteralArea::decode(raw: &[Vec<u8>], atoms: &mut AtomTable) -> Result<LiteralArea>`,
  `get(&self, u32) -> Option<Eterm>`, `len(&self) -> usize`, `is_empty(&self) -> bool`,
  `terms(&self) -> &[Eterm]`, `heap(&self) -> Option<&beam_alloc::Heap>`.
  Decoding goes through `beam_codec::decode_term` into a private `beam_alloc::Heap`; an exhausted decode
  is retried (bounded) on a larger arena, a malformed payload is a typed `BeamError`.
  `pub type SharedLiteralArea = Arc<LiteralArea>;` — the type of `Module.literals`.
  `LiteralArea` is **not `Send`/`Sync`** (it owns a raw-pointer `Heap`), and therefore neither is a loaded `Module`.
### The instruction vocabulary (`src/instr.rs`, re-exported by `ir.rs`)
- `pub enum Instr` — **107** named, typed variants covering the opcode families the compiler emits, plus
  the lossless fallback `Generic { op: Opcode, operands: Vec<Operand>, offset: u32 }`.
  Every variant's field order is the **file's** operand order (`emu/ops.tab`). The families:
  - **module/function framing** — `Label(Label)`, `Line(LineRef)`,
    `FuncInfo { module: Atom, function: Atom, arity: u32 }`, `IntCodeEnd`.
  - **moving / register shuffling** — `Move { src, dst }`, `Swap { a, b }`.
  - **calls** — `Call { arity, target: Label }`, `CallLast { arity, target, deallocate }`,
    `CallOnly { arity, target }`, `CallExt { arity, target: ImportRef }`, `CallExtOnly { arity, target }`,
    `CallExtLast { arity, target, deallocate }`, `Bif0 { bif: ImportRef, dst }`,
    `Bif { arity: u8, bif, fail: Label, args: Vec<Operand>, dst }`,
    `GcBif { arity: u8, bif, fail, live: u32, args, dst }`, `CallFun { arity }`,
    `CallFun2 { tag: Operand, arity, func: Operand }`, `Apply { arity }`, `ApplyLast { arity, deallocate }`.
  - **funs** — `MakeFun3 { index: u32, dst, env: Vec<Operand> }` (`index` = the `FunT` fun-table index,
    resolvable through `Module::fun`).
  - **tests / branching** — `Test { kind: TestKind, fail: Label, args: Vec<Operand> }`,
    `SelectVal { value, fail, pairs: Vec<(Operand, Label)> }`,
    `SelectTupleArity { tuple, fail, destinations: Vec<(Operand, Label)> }`, `Jump { target: Label }`.
  - **stack frames** — `Allocate { stack_need: HeapNeed, live }`,
    `AllocateHeap { stack_need: HeapNeed, heap_need: HeapNeed, live }`,
    `TestHeap { heap_need: HeapNeed, live }`, `InitYregs { registers: Vec<Operand> }`,
    `Deallocate(u32)`, `Trim { n: u32, remaining: u32 }`, `Return`,
    `DeallocateReturn(u32)` (the loader's only superinstruction — see `src/lower/fuse.rs`).
  - **tuples** — `PutTuple2 { dst, elements: Vec<Operand> }`,
    `GetTupleElement { source, element: u32, dst }`.
  - **lists** — `PutList { head, tail, dst }`, `GetList { source, head, tail }`,
    `GetHd { source, head }`, `GetTl { source, tail }`.
  - **maps** — `GetMapElements { fail, source, pairs: Vec<(Operand, Operand)> }`,
    `HasMapFields { fail, source, keys: Vec<Operand> }`,
    `PutMapAssoc { fail, source, dst, live, pairs }`, `PutMapExact { fail, source, dst, live, pairs }`.
  - **records** — `UpdateRecord { hint: Operand, size: u32, src, dst, updates: Vec<(Operand, Operand)> }`,
    and the whole **OTP-29 native-record family** (generic opcodes 186-193; C ground truth
    `erl_record.c`), one typed variant per source opcode carrying the file's raw decoded operands
    (like the `bs_*` family): `IsAnyNativeRecord { fail: Label, src: Operand }` (the
    `erlang:is_record/1` guard, 186 — `emu/instrs.tab:1323`, `f xy` in `emu/ops.tab:1634`;
    deliberately a new `Instr` variant rather than a `TestKind`, which the engine matches
    exhaustively), `IsNativeRecord { fail, src, module, name }` (187), `GetRecordElements { fail, src,
    fields: Vec<(Operand, Operand)> }` (188), `PutRecord { fail, id, src, dst, live: u32,
    updates: Vec<(Operand, Operand)> }` (189), `IsRecordAccessible { fail, src, scope }` (190),
    `GetRecordField { fail, src, id, field, dst }` (191), `GetRecordElementsId { fail, id, src,
    fields }` (192, added in OTP 30) and `UpdateRecordId { fail, hint, id, src, dst, live, updates }`
    (193, added in OTP 30). `fields`/`updates` are the file's flattened `{Name,Register}` /
    `{Name,Value}` lists split into pairs (`emu/ops.tab:1636-1686`; `lib/compiler/src/beam_disasm.erl:1402-1431`).
  - **exceptions** — `Badmatch { value }`, `CaseEnd { value }`, `IfEnd`, `Try { frame, fail: Label }`,
    `Catch { frame, fail }`, `TryCase { frame }`, `TryEnd { frame }`, `TryCaseEnd { value }`,
    `CatchEnd { frame }`, `Raise { trace, value }`, `Badrecord { value }`, `BuildStacktrace`, `RawRaise`.
  - **floating point** — `Fmove { src, dst }`, `Fconv { src, dst }`,
    `Fadd`/`Fsub`/`Fmul`/`Fdiv { fail: Label, lhs, rhs, dst }`, `Fnegate { fail, src, dst }`.
  - **messages / receive** — `Send`, `LoopRec { fail: Label, dst }`, `LoopRecEnd { target: Label }`,
    `Wait { target }`, `WaitTimeout { target, time: Operand }`, `Timeout`, `RemoveMessage`,
    `RecvMarkerReserve { marker }`, `RecvMarkerBind { marker, reference }`,
    `RecvMarkerClear { reference }`, `RecvMarkerUse { reference }`.
  - **binary syntax (`bs_*`)** — `BsStartMatch3 { fail: Label, bin, live, dst }`,
    `BsStartMatch4 { fail: Operand, live, src, dst }`, `BsMatch { fail, ctx, commands: Vec<Operand> }`,
    `BsMatchString { fail, ctx, bits: u32, offset: u32 }`, `BsGetTail { ctx, dst, live }`,
    `BsGetPosition { ctx, dst, live }`, `BsSetPosition { ctx, pos }`,
    `BsGetInteger2`/`BsGetFloat2`/`BsGetBinary2 { fail, ctx, live, size, unit: u32, flags: u32, dst }`,
    `BsSkipBits2 { fail, ctx, size, unit, flags }`, `BsTestTail2 { fail, ctx, bits }`,
    `BsGetUtf8`/`BsGetUtf16`/`BsGetUtf32 { fail, ctx, live, flags, dst }`,
    `BsSkipUtf8`/`BsSkipUtf16`/`BsSkipUtf32 { fail, ctx, live, flags }`, `BsInitWritable`,
    `BsCreateBin { fail, alloc: u32, live, unit: u32, dst, segments: Vec<Operand> }`.
  - **debug / module markers** — `ExecutableLine { location: LineRef, index: u32 }`,
    `DebugLine { kind: Operand, location, index, live }`, `NifStart`, `OnLoad`.
  - **fallback** — `Generic { op, operands, offset }` (`offset` = byte offset in the `Code` stream, for
    line/backtrace mapping) for the deliberately-unmodelled opcodes (see Known Issues).
- `pub enum TestKind` — the classification of the `is_*`/`test_arity` family (`IsLt, IsGe, IsEq, IsNe,
  IsEqExact, IsNeExact, IsInteger, IsFloat, IsNumber, IsAtom, IsPid, IsReference, IsPort, IsNil, IsBinary,
  IsBitstr, IsList, IsNonemptyList, IsTuple, IsMap, IsBoolean, IsFunction, IsFunction2, IsTaggedTuple,
  TestArity`) with `TestKind::from_opcode(Opcode) -> Option<TestKind>`;
  `Instr::Test` carries the kind plus the failure label and the tested operands (`fail` is *not* in `args`).
  The variant names carry the C semantics: `IsEq`/`IsNe` are **arithmetic** `==`/`!=` (`is_eq`/`is_ne`, C
  `CMP_EQ_ACTION`/`CMP_NE_ACTION`) while `IsEqExact`/`IsNeExact` are **exact** `=:=`/`=/=`
  (`is_eq_exact`/`is_ne_exact`, C `EQ`); `IsList` accepts `[]`, `IsNonemptyList` does not; `IsFunction2`
  is a fun of exact arity, `IsTaggedTuple` a tuple of the given size whose element 0 is the tag.
- `pub type LineRef` / `pub use beam_term::Eterm;` are re-exported from `ir.rs` (the resolved type of `Module::literal`).
- `pub enum Opcode` (`src/opcode.rs`) — **one variant per generic opcode** (the full `genop.tab`
  roster), with `Opcode::from_u8(u8) -> Option<Opcode>`, `Opcode::as_u8(self) -> u8`,
  `Opcode::name(self) -> &'static str`, `Opcode::arity(self) -> u8`. The roster is data derived from
  `../../lib/compiler/src/genop.tab` (see Design Decisions); it is complete and lossless so
  unmodelled instructions still round-trip as `Instr::Generic`.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum. Malformed/truncated
  container, bad magic, bad format number, unknown opcode tag, out-of-range atom/label/register index,
  structurally wrong operand, unsupported `z` sub-tag ⇒ `BeamError::BadArg`; a size/arity/count bound
  (arity > 255, more than `ERTS_BEAM_MAX_OPARGS`, negative chunk size, atom name not
  UTF-8-representable, an inline instruction operand wider than 8 magnitude bytes) ⇒
  `BeamError::SystemLimit`.
  **Never panic, never `unwrap()` on file data, never silently accept a malformed image.**
- **`unsafe` is forbidden in this crate** (`#![deny(unsafe_code)]`) — the loader is the *safe
  periphery*. Zero-copy is achieved safely via `beam_codec::Reader::bytes(n) -> Result<&'a [u8]>`;
  no raw pointers are needed.
- **Reuse, don't duplicate.** The big-endian byte cursor is `beam_codec::Reader` (the ETF codec's
  bounds-checked cursor, the lowest common ancestor for BE byte reads) — do **not** write a second BE
  reader. Bounds/endianness live there.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_codec`, `beam_alloc`, `libc`.
  Adding a dependency edge requires reporting to the crate-layer node (`../`).
- **Generated data is source, not output.** The operand-tag table and opcode roster come from the
  legacy generator/roster; the crate embeds a **regenerable** table (documented below) — never
  hand-guess a byte value.
- **Files ~1000 lines**, one responsibility per file. `Instr` is one flat module (`src/instr.rs`) —
  an enum cannot be split across submodules (see Design Decisions). Licensing Apache-2.0.

## The wire format (verified against the OTP 29 / erts-17.1 oracle)
### Container (`src/container.rs`)
```
u32  "FOR1"                        // iff magic
u32  form_length                   // = file_size - 8 ; INCLUDES the 4-byte form type
u32  "BEAM"                        // iff FORM TYPE (not a chunk)
chunk entry * :  u8[4] chunk_id | u32 size | size bytes | (4 - size%4)%4 zero pad
```
`"BEAM"` is the form type, **there is no `BEAM` chunk**. Chunks parse until EOF; unknown ids skipped;
duplicate ids → first wins; chunk order is not significant. Chunk ids: `AtU8` (atoms), `Code`
(instructions), `StrT`, `ImpT`, `ExpT`, `LocT` (JIT-only locals), `FunT`, `LitT`, `Attr`, `CInf`,
`Line`, `Type`, `Meta`, `DbgB` (legacy debug), `Dbgi` (compiler debug info), `Docs` (ExDoc),
`Recs`, obsolete `Atom`. The ids the installed corpus actually uses are classified in
`src/chunk/classify.rs` (`ChunkClass`) and enforced by `tests/chunks.rs`: each is parsed+exposed,
parsed-as-raw, or tooling-only (with a reason). `Docs` (a compressed ExDoc term), `DbgB`/`Dbgi`
(debug info) and the obsolete `Atom` chunk are the tooling-only ids.
### Per-chunk layouts (`src/chunk/`)
- **`AtU8`** — `i32 count` (negative ⇒ "long" varint lengths, real count = `-count`); then per atom a
  length (varint if long, else `u8`) + UTF-8 bytes. **UTF-8, not Latin-1** (`beam_file.c:291`).
  **Index 0 is reserved for NIL**; index 1 is the module name; real atoms are `1..=count`
  (`beam_file.c:262-297`). Total entries = `count+1`.
- **`Code`** — header `u32 head_size` (== 16) then `u32 format_number` (== `BEAM_FORMAT_NUMBER` 0,
  guarded), `u32 max_opcode`, `u32 label_count`, `u32 function_count`; then the instruction stream.
  `max_opcode` must lie in `[169, 193]` — `169` = `swap/2` (`genop_swap_2`, `lib/compiler/src/genop.tab:611`),
  `193` = `MAX_GENERIC_OPCODE` (the last `genop.tab` entry, `193: update_record_id/7`); outside that window
  the image is rejected with `BadArg`, exactly as the C refuses an OTP-22-or-earlier or later-version image
  (`beam_load.c:189-225`).
  The stream ends at the `int_code_end/0` instruction (genop 3), not at size exhaustion
  (`beam_file.c:1527,2596`). Instruction = 1 opcode byte (`1..=max_opcode`) + `Opcode::arity` operands.
- **`ExpT`** — `i32 count` × `{ i32 function_atom, i32 arity, i32 label }` (label = **entry** label).
- **`LocT`** — byte-for-byte the `ExpT` layout (`table::parse_locals` reuses the export-table reader;
  the C does the same: `parse_locals_chunk` → `parse_export_table`). The JIT-only local-function
  table, parsed into `Module.locals`; irrelevant to interpretation.
- **`ImpT`** — `i32 count` × `{ i32 module_atom, i32 function_atom, i32 arity }`.
- **`FunT`** (lambdas) — `i32 count` × `{ i32 function_atom, i32 arity, i32 label, i32 index,
  i32 num_free, i32 old_uniq }` (24-byte entries). Parsed into `FunDescriptor`s on `Module.funs`
  (position = the `make_fun3` fun-table index); a descriptor whose `label` is not a label in the module
  is rejected with `BadArg`.
- **`Line`** — `u32 version` (must be 0, else the chunk is ignored), `u32 flags`, `u32 instr_count`,
  `u32 item_count`, `u32 name_count`; then a run of packed tagged items, then `name_count` ×
  `{ i16 len, len bytes UTF-8 }`. `item_count` counts **only** the `TAG_i` ("line") items: the loop reads
  tagged items until `item_count` of them have been `TAG_i`; a `TAG_a <name_index>` sets the current file
  index and is consumed *in addition* (not counted), a `TAG_i <line>` emits a
  `LineEntry { file, line }` (`beam_file.c:443-602`). `items[0]` is the undefined location `{0,0}`;
  `files[0]` is synthesized as `<module>.erl`.
- **`Type`** — `u32 version` (`4` current, `3` OTP 27/28; else fallback), `i32 count` (≥1,
  `entries[0]` = "any"), then per entry a `u16 type_word` (+ optional `i64 lower`, `i64 upper`, `u8 unit-1`
  gated by `type_word` bits 13-15). Absent/incompatible ⇒ a single "any" fallback entry.
  **Version 3** remaps each entry's two bytes `b0`/`b1` into the v4-convention word before the metadata
  bits are tested (`beam_file.c:664-681`): `extra_bits = (b0 << 1) & 0xE0`, `upper = b0 & 0x0F`
  (forced to `0x1F` when it is `0x0F`, so v3's 12-bit all-ones `0xFFF` maps to the v4 "any" `0x1FFF`),
  `word = ((upper | extra_bits) << 8) | b1`. `Type::raw` therefore always holds the reconstructed
  **v4-convention** word (`raw & 0x1FFF` = type union, `raw & 0xE000` = metadata flags) for both versions.
- **`LitT`** — `u32 uncompressed_size` (`0` ⇒ the records follow verbatim; non-zero ⇒ the rest of the
  chunk is a zlib (RFC 1950) stream that inflates to exactly `uncompressed_size` bytes, which ARE the
  records), then `i32 count` × `{ i32 ext_size, ext_size bytes }` (Erlang external-term bytes).
  `src/chunk/literal.rs` handles **both** forms — the compressed form via the public
  `beam_codec::compress::decompress`, rejecting a length that disagrees with `uncompressed_size` — and
  parses the **framing** only (raw ETF payloads); `src/literal::LiteralArea` then decodes each payload
  into a term once at load time. Static literals are referenced by `Operand::Literal(idx)`.
- **`StrT` / `CInf` / `Attr` / `Meta` / `Docs` / `DbgB` / `Dbgi`** — `StrT` is captured raw
  (`Module::strings`). `Attr`, `CInf` and `Meta` are each a single `term_to_binary` payload and are
  **decoded into a term** once at load time into a private arena shared through an `Arc`
  ([`crate::decoded::DecodedTerm`]): `Attr` → the `[{Atom, Value}]` attribute list
  (`Module::attributes_term`/`attribute_by_name`, `src/attributes.rs`) for `module_info(attributes)`;
  `CInf` → the compile-info list (`Module::compile_info_term`/`compile_info_by_name`,
  `beam_asm.erl:236,309` writes it, `erl_bif_info.c:5896-5918` serves `module_info(compile)`) and
  `Meta` → the feature-metadata property list (`Module::meta_term`/`meta_by_name`, `compile.erl:2269-2295`).
  Each chunk's raw bytes stay on the module too (`Module::attributes`/`compile_info`/`meta`).
  `Docs`/`DbgB`/`Dbgi` are tooling-only ids whose payloads are deliberately **not** parsed: they are large
  and near-universal, and eagerly materializing them would regress the measured module-load cost
  (`tests/load_cost.rs`).
- **`Recs`** — `u32 version` (must be `0`; a non-zero version means "no records" — chunk-absent,
  *not* an error, `beam_file.c:1335-1348`), `i32 record_count` (`≥ 0`; negative ⇒ `BadArg`),
  `i32 total_field_count` (informational; `≥ 0`), then `record_count` record entries EACH encoded as
  one BEAM instruction: an opcode byte that must be `Opcode::call_last` (5 — reused only because it
  has 3 operands), then operand 1 = the record-name **atom**, operand 2 = the `true`/`false`
  export-flag **atom**, operand 3 = a `z,1` **argument list** (`u` element count `N`, must be **even**,
  then `N/2` `(field-name atom, default)` pairs). Field defaults (raw tag, *not* via
  `decode_operand`): `u` = **no default** (the `make_catch(0)` sentinel), `i` = a signed small
  integer, a wide `i` = a signed **bignum** (`marshal_integer`), `a` = an atom (`0` ⇒ `nil`),
  `z,4` = a `LitT` literal index. `src/chunk/record.rs` parses it into `Chunks.records`; the
  record's module is the module being loaded (the chunk carries none, like the C's
  `tmp_def->module = beam->module`).
### Operand tags (`src/operand.rs` + `src/decode.rs`) — the authoritative table
Each operand = one *tag/length* byte then optional payload (`beam_file.c:167-240`). Tag = `byte & 0x07`
(3 bits ⇒ a file carries only tags 0–7; tags 8–14 are loader-internal and **never appear in a file**):
| byte | tag | meaning | payload |
|---|---|---|---|
| 0 | `u` | untagged integer | varint magnitude |
| 1 | `i` | integer (sign-extended) | varint magnitude; fits a small int ⇒ `Integer` |
| 2 | `a` | atom index (0 ⇒ NIL) | varint index |
| 3 | `x` | X register | varint ≤ 1023 |
| 4 | `y` | Y register | varint ≤ 1023 |
| 5 | `f` | label (0 ⇒ failure-sentinel `NO_FAIL`) | varint < label_count |
| 6 | `h` | character | (defined, never emitted by the compiler) |
| 7 | `z` | extended: sub-tag varint follows | see below |
The tag byte encodes a variable-length integer: if `byte & 0x08 == 0` the value is `byte >> 4` (0–15);
else if `byte & 0x10 == 0` a 1-byte big-endian tail combines to 0–0x7FF; else `(byte>>5)+2` big-endian
magnitude bytes; if `(byte>>5)==7` a `u`-tagged sub-varint gives `count = S+9` bytes (`beam_file.c:167-240`).
`z` sub-tags (`beam_file.c:2339-2447`): `1` = argument list (`u count` then `count` sub-operands →
`Operand::List`); `2` = float register (`u` index → `Operand::FloatReg`); `3` = allocation list
(`u count` then `count`×(`u kind`,`u number`) → `Operand::Allocation`); `4` = static literal
(`u` index → `Operand::Literal`); `5` = **typed register `tr`** (a nested `x`/`y` sub-operand then a
`u` **type index** → `Operand::Typed`); `0` (obsolete float) and any other ⇒ `BadArg`.
**`tr` packing (`beam_file.c:2419-2443`):** the type index is `Index` into the `Type` chunk; the file
carries `reg | (type_index << 10)` conceptually — but the loader keeps reg and `TypeIndex` **separate**
in `Operand::Typed`. If the `Type` table is a fallback (absent/stripped) the tag collapses to a plain
`X`/`Y` operand (the type index is discarded).

## Design Decisions
- **The opcode roster is embedded from `genop.tab`, not from generated C.** `Opcode` enumerates every
  generic opcode in `../../lib/compiler/src/genop.tab` (the checked-in, wire-frozen roster the compiler
  emits; `format_number` bumps when it changes). `opcode.rs` holds the name/number/arity table with a
  header comment stating it is derived verbatim from that file (with the exact extraction rule:
  `^<n>: <name>/<arity>`, skipping `-`-prefixed transform-only ops is NOT done — all entries are kept so
  the mapping is bijective). **Regeneration:** re-read `genop.tab`; the numbers are the file's first field.
  This is far more robust than parsing `erts/emulator/utils/beam_makeops` output, and `genop.tab` is the
  true source (the generator only adds specific/transform ops the file never contains).
- **BIF operands are `ImpT` indices, NOT `bif.tab` numbers.** Verified against the oracle fixture: the
  `gc_bif*` BIF operand is a plain `u` = the index of `{module,function,arity}` in the `ImpT` chunk
  (`beam_asm.erl:774-776`; `beam_file.c:2310-2313`). The IR stores `ImportRef`; the engine resolves
  `Module.imports[bif]` and the `ExecutionContext` maps `{erlang,'+'/2}` to the BIF.
  **CROSS-CRATE CONTRACT (binding on the `beam_bifs` architect):** the engine/BIF layer keys BIFs
  by `(module, function, arity)` following the real `bif.tab` naming — the loader does **not** know and
  must **not** encode `bif.tab` numbering. `Module.imports` is the single source of BIF identity.
- **`Instr` and `TestKind` live in `src/instr.rs`, re-exported from `ir.rs`.** The instruction
  vocabulary is one large flat enum (107 variants); an enum cannot be spread across submodules and
  splitting the *variants* would destroy the single-name surface the engine matches on, so it is kept in
  one dedicated module while `ir.rs` (the data types) does `pub use crate::instr::*;` so `crate::ir::Instr`,
  `crate::Instr` and `prelude::*` are unchanged. This keeps `ir.rs` focused on the module/function/table
  data structures.
- **`HeapNeed` preserves both need encodings.** `allocate`, `allocate_heap` and `test_heap` accept their
  word need as either a plain count or a `z,3` allocation list (`emu/ops.tab` declares both), so the IR
  keeps both: `HeapNeed::words()` recovers the total the engine needs for its heap-overflow check while
  `HeapNeed::Alloc` preserves the per-kind (`words`/`floats`/`funs`) breakdown a JIT or a precise
  allocator may want.
- **The literal area is write-once, so it is shared behind an `Arc`.** `LitT` literals are decoded into
  one private `beam_alloc::Heap` at load time and never mutated, so `Module` shares the area through
  `Arc<LiteralArea>` and a `Module` clone is cheap. The area owns a raw-pointer heap (its `unsafe` is
  confined to `beam_alloc`), so `LiteralArea` — and therefore a loaded `Module` — is **neither `Send`
  nor `Sync`**: a module stays on the thread that loaded it. Heap exhaustion while decoding is retried
  (bounded) on a larger arena rather than silently truncated.
- **`beam_alloc` is the literal-materialization dependency.** `src/literal.rs` decodes through
  `beam_codec::decode_term` into a `beam_alloc::Heap`; this adds the `beam_loader → beam_alloc` edge
  (in-workspace, no cycle — `beam_codec` already depends on it). `beam_term::heap::Heap` remains a test
  arena only.
- **Two function labels per function.** The file's canonical sequence is
  `label FuncLabel | [line] | func_info M F A | label EntryLabel | body...` (`beam_file.c:2502-2589`).
  `Function` therefore carries both `func_label` and `entry_label`; `Export.label` equals `entry_label`.
  Functions are delimited by `func_info` instructions (the reader's own boundary rule).
- **Atoms are interned in file order into a crate-owned `AtomTable`.** `Module.atoms[i]` maps the wire
  index to the interning `Atom`; `atoms[0]` = `Atom::MAX` (NIL). `Operand::Atom` holds the interned
  `Atom`. Interning into a *shared, `atom.names`-seeded* table (needed for preloaded/positional atoms)
  is `beam_rt`'s job — see Known Issues.
  **Shared-table seam — no dedicated entry point exists.** `load`/`load_file` always build a *private*
  per-module table (`src/load.rs:36`). To share, drive the pipeline manually: `container::parse` →
  `chunk::parse` → intern `chunks.atoms[1..]` (wire order) through the shared table into a `Vec<Atom>`
  → `decode::decode_stream(stream, &atoms, &code_version, &types)` →
  `lower::build_module(&chunks, &raw, table)`. **`build_module` takes the `AtomTable` by value and moves
  it into `Module.atom_table`**, so pass a *clone* of the shared table to keep the same numbering; every
  atom index baked into the IR (`Module.atoms`, `Operand::Atom`, exports/imports) then indexes the
  shared table. The decoder resolves `a` operands through the caller-built `&[Atom]`, not the table.
- **`z,5` `tr` is normalized to `reg` + `TypeIndex`**, so the interpreter may ignore the annotation and
  a JIT may consume it later. The `Type` chunk is parsed into `TypeTable` (with the fallback table).
- **`Instr` is a named enum; `lower_instr` is the whole semantic mapping.** `src/lower/instr.rs` maps
  every opcode the compiler emits to its named variant; every other opcode is preserved losslessly as
  `Instr::Generic { op, operands, offset }` so nothing is lost and the engine/JIT can extend without a
  format change. `offset` is the byte offset in the `Code` stream.
- **Basic blocks are label-delimited.** `Function.blocks` is one block per label, `code[start..end]`;
  `Function.labels` is the label→index map an engine walks linearly with. CFG construction is the
  engine's concern. `src/lower/group.rs` owns `group_functions` (the `func_info` boundary rule) and
  `build_blocks`; `src/lower.rs` owns `build_module` + module assembly.
- **The instruction decoder takes the atom map and the type table.** `decode_stream(stream, atoms,
  version, types)` and `decode_operand(r, atoms, types)` (loader-internal — *not* part of the frozen
  `ir`/`prelude`/`load` surface `beam_engine` designs against) resolve an `a` operand to the interned
  `Atom` at decode time and collapse a `z,5` `tr` via `types.fallback`. So `RawInstr` operands already
  hold interned atoms and `lower` never rewrites operands. `load` interns the `AtU8` names into a fresh
  `AtomTable` *before* decoding and passes that same table on to `build_module` (interning is idempotent,
  so `Module.atoms` matches the atoms the decoder embedded).
- **Operand-encoding gotchas (oracle-verified).** A `label/1` operand is written with the **`u`** tag (an
  `Integer`, not an `f` `Label`); jump/failure targets (`call*`, `call_ext*`, test `fail`, `select_val`
  alternatives) use the `f` tag and are strict `Label`s. An `i` operand is sign-extended only in the
  explicit-count form (`read_tagged` `byte_count >= 2`); the 1-byte-tail form (values 16–0x7FF) is
  unsigned — matching `beam_file.c`.
- **Wide integer instruction operands become synthesized literals.** A `u`/`i` operand whose magnitude
  does not fit a machine word is the C `TAG_o` overflow state (`beam_file.c:207-237`); the decoder models
  it as `Operand::BigInteger`, and `lower::build_module` appends a synthesized ETF bignum payload to the
  module's literal set and rewrites the operand to `Operand::Literal`. This keeps `.beam`-wire concerns in
  `beam_loader`, mirrors the C (`marshal_integer` → `beamfile_add_literal`) and needs no engine change
  (the engine already resolves `Operand::Literal` via `Machine::literal`).
- **`Recs` field defaults are classified from the *raw* operand tag, and the record table keeps
  declaration order.** Two facts pin the `Recs` output:
  1. **Declaration order.** `records:get_definition/2` returns a record's fields in **declaration
     order**, and the file writes them in that order, so neither `record::parse` nor `lower::build_records`
     sorts them (`RecordDefinition.fields` is exactly the file's `(name, default)` pair sequence).
     Records themselves are likewise kept in file order.
  2. **`u,0` (absent) vs `i,0` (integer `0`).** A default's *operand tag* — not its magnitude — carries
     the absent/`0` distinction: `u` (`u,0`) is the C's absent sentinel `make_catch(0)` ⇒
     `RawRecordDefault::Absent` ⇒ `RecordField.default == None`; `i` (`i,0`) is a real small-integer
     default of `0` ⇒ `Some(<small 0>)`. The shared instruction decoder (`decode_operand`) collapses
     both tags to `Operand::Integer`, so `record::parse` reads defaults through the raw tag via the
     shared `decode::read_tagged` and classifies them in `record::read_default` — `u` ⇒ absent, `i` ⇒
     small integer or (wider than a small) a signed bignum, `a` ⇒ atom (`a,0` = `nil`), `z,4` ⇒ a
     static-`LitT` literal index (`beam_file.c:1200-1216`). A wide `i` default stays a `BigInteger` on
     the field; `build_records` appends its ETF payload to the module's synthesized-literal set
     (numbered after the file's own `LitT`, the same rule wide instruction operands follow) and
     `resolve_records` then materializes the term once the literal area is decoded.
- **The `bs_create_bin` allocation operand accepts both encodings.** The compiler hoists a block's heap
  need into a preceding `bs_create_bin` (`beam_ssa_codegen.erl`, `need_heap_terminator`), so that `z`-op
  operand can be a bare word count or a `z,3` allocation list. `lower_instr` decodes it through
  `HeapNeed` (exactly as `test_heap`/`allocate` do), taking `HeapNeed::words()` for the `u32` field —
  matching the C `marshal_allocation_list`, which always marshals any `z,3` to a single word total.
- **Opcode arity is table-indexed and the decode vector is pre-sized (measurement-backed).** `src/decode.rs`
  builds a `OnceLock<[u8; 256]>` arity table *derived from* `Opcode::from_u8`/`Opcode::arity` (the roster
  stays the single source of truth; a unit test pins all 256 bytes), turning the per-instruction
  `Opcode::arity()` linear scan (~33 ns) into an array index (~1 ns); `decode_stream` pre-sizes its
  instruction vector from `stream.len()/4`, capped so a crafted oversized `Code` chunk cannot force an
  unbounded reservation. Both were justified by the committed `tests/load_cost.rs` measurement (~5-6 %
  faster module load overall; decoder stage −3.4 %). The remaining ~30 % of load time is IR-owned-`Vec`
  allocator churn — a future win needs ownership changes in `src/lower/group.rs`/`src/ir.rs`, not the frozen IR.
- **Load cost is measured, not guessed.** `tests/load_cost.rs` is the committed, dependency-free
  measurement (deterministic 200-module sample; ns/instruction, bytes/second, per-stage and per-module
  breakdown). Its own doc header carries a "recorded reference numbers" table; treat those as indicative
  and re-measure rather than quote (single-pass wall clock is noisy on shared machines — the per-module
  aggregate is the stable metric).

## Test Strategy
- **Fixtures:** the committed oracle `.beam` files, all compiled from the checked-in `.erl` sources with
  the system `erlc` (OTP 29 / `erts-17.1`, the differential oracle); `tests/fixtures/README.md` documents
  the exact regeneration command and the full wire summary (chunk order/sizes, atoms, exports/imports,
  and the `Code`/`Line`/`Type`/`FunT`/`LitT`/`Recs` details).
  - `tests/fixtures/slice.beam` (712 bytes) from `slice.erl` — the vertical-slice module (`add/2`,
    `fib/1`, `pick/1`, plus `module_info/0,1`) covering the minimal generic opcode subset.
  - `tests/fixtures/coverage.beam` from `coverage.erl` — a broader idiomatic slice: tuples
    (`put_tuple2`/`get_tuple_element`/`select_tuple_arity`), lists (`put_list`/`get_list`), funs
    (`make_fun3` create + `call_fun2` call, populating `FunT`), `case`, `try … catch` (`try`/`try_case`/
    `try_end`), float arithmetic, maps (`put_map_assoc`/`put_map_exact`/`get_map_elements`), bitstrings
    (`bs_match`/`bs_start_match3`/`bs_get_*`/`bs_create_bin`), the `z,3` allocation-list need form, and
    `LitT` literals (`{error,division_by_zero}`, `{error,badarith}`, `#{a=>1,b=>2}`).
  - `tests/fixtures/records_fixture.beam` from `records_fixture.erl` — the `Recs`-chunk fixture: four
    native records (`#user` with an `i,-1` default; `#empty`; `#mixed` exercising *every* default
    encoding — `i,0`, the `u,0` absent sentinel, an atom, a tuple/`LitT` literal, a list/`LitT` literal
    and a wide-`i` bignum; `#shared` carrying `-export_record([shared])`). The module is named
    `records_fixture`, **not** `records`, to avoid colliding with the stdlib `records` module (which
    would make the oracle's `records:get_definition/2` loop forever).
  - `tests/fixtures/nr_def.beam` / `nr_use.beam` from `nr_def.erl` / `nr_use.erl` (`+deterministic`) —
    the OTP-29 **native-record** pair: `nr_def` defines/exports `#rec`, `nr_use` imports it, defines its
    own `#local`, and builds/reads/updates both, so together they emit opcodes 186-191
    (`is_any_native_record`, `is_native_record`, `get_record_elements`, `put_record`,
    `is_record_accessible`, `get_record_field`); the hand-written OTP-29 `erlc` *does* emit these.
  - `tests/fixtures/linecov.beam` from `linecov.erl` (`+line_coverage`) — emits `executable_line` (183).
  - `tests/fixtures/dbgline.beam` from `dbgline.erl` (`+beam_debug_info`) — emits `debug_line` (184).
  - `tests/fixtures/bstail.beam` from `bstail.erl` (`+no_ssa_opt`) — emits `bs_test_tail2` (121); with SSA
    opts on, `bs_translate/1` folds it into a `bs_match` `ensure_exactly` command, so `+no_ssa_opt` is
    needed to observe the generic opcode.
  - `tests/fixtures/literals_verbatim.beam` / `literals_compressed.beam` (same source
    `literals_fixture.erl`, the second with `+compressed_literals`) — the two `LitT` framings, covering
    every literal kind (tuples/lists/maps/floats/bignums/binaries/external funs); plus
    `literals_oracle.escript`, the independent Erlang-side oracle for the literal sweep.
- **Ground truth is derived, never guessed:** `beam_disasm:file(...)` plus a byte decode; if a fixture
  byte changes, re-derive the expected IR.
- **`tests/container.rs`:** the chunk directory (ids/sizes in file order), the `AtU8` atoms, the
  `ExpT`/`ImpT` tables (file order preserved), the `Code` header `(0, 181, 15, 6)`, and the `Line`/`Type`
  tables.
- **`tests/ir.rs`:** the complete per-function instruction stream for all six functions, the
  `func_label`/`entry_label` pairs, `export/2` → entry label, and the label→index / block decomposition.
- **`tests/widened_ir.rs`:** the widened typed IR over `coverage.beam` — the typed variants for tuples,
  lists, funs, exceptions, floats, maps and bitstrings, the `HeapNeed` encodings, the decoded
  `LiteralArea` (`Module::literal`/`literals`) and the `FunDescriptor` table (`Module::fun`/`funs`).
- **`tests/negative.rs`:** truncated header, bad `"FOR1"` magic, bad form type, `format_number != 0`,
  a chunk size that overruns the file, an unknown/out-of-range opcode byte, an out-of-range atom/label
  index, a truncated operand varint, a `z` sub-tag `0`/unknown, a structurally wrong `HeapNeed` operand
  and a truncated/malformed compressed `LitT` ⇒ each a typed `BeamError` (no panic).
- **`tests/real_modules.rs`:** the broad real-OTP LOAD sweep. The `lib` root is discovered
  generically — `$ERTS_LIB_DIR` when set, else `erl -noshell -eval 'io:format("~s",[code:lib_dir()])'`
  (no hardcoded app names) — then every `.beam` under it is collected recursively (each versioned app
  dir's `ebin`, plus doc/examples & `proper_ext`). On this install that is **34 applications / 1366
  modules**; every one is loaded through the public pipeline and must load (zero failures tolerated: any
  parse failure fails the test, and each is printed by relative path with its exact typed error), plus
  the "loaded but yielded no function" check. Floors are `total ≥ 1000` modules and `app_count ≥ 30`, well
  below the real counts. A second test walks the decoded `LitT` literals of the real `maps` and
  `gen_server` modules (via the single-app `app_ebin("stdlib")` helper) and asserts the nested
  `EXPORT_EXT` funs resolve to their `{module,function,arity}` (`fun maps:remove/2`,
  `gen_server:format_log/1,2`) — semantic evidence, not a count. Loudly skips when the system `erl`/OTP is
  unavailable (`skip_reason` still requires `stdlib`+`kernel`).
- **`tests/widened_ir.rs`** also loads `tests/fixtures/bsalloc.beam` (the `bs_create_bin` `z,3` operand).
- **`tests/records.rs`:** the `Recs` chunk over `tests/fixtures/records_fixture.beam` — the parsed
  definitions reproduce the pinned `records:get_definition/2` output (declaration order, every default
  encoding incl. the `u,0`/`i,0` distinction and the synthesized wide-`i` bignum literal), plus the
  `record_by_name`/`record` lookups, a live-`erl` re-derivation of the pinned facts (bounded wall-clock,
  loud skip without OTP), and hand-built malformed-`Recs` images rejected with `BeamError::BadArg` through
  the whole `load` pipeline — each next to a well-formed **control** image (same `AtU8`/`Code`, only the
  `Recs` payload varying) that genuinely loads, so the negative cases cannot pass for a structural reason.
- **`tests/generic_census.rs`:** the residual `Instr::Generic` census over every installed
  stdlib/kernel/compiler `.beam` — asserts the unmodelled-opcode set equals the documented baseline, so a
  newly-emitted unmodelled opcode fails the test; loud skip when `erl`/OTP is absent.
- **`tests/line_info.rs`:** `linecov.beam` carries a typed `Instr::ExecutableLine` and `dbgline.beam` a
  typed `Instr::DebugLine` (neither on `Generic`); asserts the `Line` table and every function's
  `Instr::FuncInfo` are preserved (the runtime needs `current_location`/stack traces).
- **`tests/bs_test_tail.rs`:** `bstail.beam` carries a typed `Instr::BsTestTail2` (not `Generic`), with the
  fail label inside `f/1`.
- **`tests/chunks.rs`:** walks the whole installed corpus asserting every chunk id is classified
  (`chunk::classify`) and every module loads (unknown id ⇒ failure naming it); positive controls require
  all 15 corpus ids to appear so the pass cannot be vacuous; pins key ids' classes row-by-row and
  cross-checks the decoded `Attr` against the runtime `module_info(attributes)` on `gen_server`/`kernel`.
- **`tests/literals.rs`:** both `LitT` framings decode to identically-encoded terms; structural assertions
  for every literal kind; plus a whole-corpus sweep re-encoding every decoded literal byte-exactly and
  comparing semantically against `literals_oracle.escript` for map literals whose key order is non-canonical.
- **`tests/load_cost.rs`:** the committed, dependency-free module-load-cost measurement (Design Decisions):
  a deterministic 200-module sample reporting ns/instruction and bytes/second, with per-stage and
  per-module breakdown; loud skip without OTP.
- Unit tests live in-module (`#[cfg(test)]`) in every `src/` file.
- **Gate (GREEN required):** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` read-only), never `cd`, never touch `../tools/` or `../../erts/`.
- Cross-checks against the real system ERTS belong to the sibling `../tools/differential/` harness.

## Known Issues / Intentional M1 gaps
- **`Instr::Generic` fallback opcodes — 133 of 193 typed, 60 Generic.** 108 opcodes lower to a named
  `Instr` mapping (`src/lower/instr.rs`) and the 25 `is_*`/`test_arity` opcodes to `Instr::Test`, so 133
  of the 193 `genop.tab` opcodes are typed; the other 60 keep `Instr::Generic`. 59 of those are the
  `-`-prefixed obsolete/transform-only entries the compiler never emits (`is_constant/2`, `make_fun2/1`,
  `allocate_zero/2`, the legacy `bs_*` forms, …); the 60th is **`set_tuple_element/3` (opcode 67)** — a
  non-obsolete `genop.tab` entry that is also never emitted (the compiler emits `put_tuple2`). The
  compiler emits none of the 60, so the observed residual census over the installed stdlib/kernel/compiler
  modules is **empty** (`tests/generic_census.rs` pins it); it is a deliberate, lossless gap — never a
  wrong result.
- **Previously-unexercised variants are now fixture-proven reachable (not merely claimed).** The on-PATH
  OTP-29 `erlc` DOES emit the native-record family: the committed `nr_def`/`nr_use` pair lowers 186-191
  typed. `executable_line` (183) / `debug_line` (184) are reachable via `+line_coverage` /`+beam_debug_info`
  and `bs_test_tail2` (121) via `+no_ssa_opt`; each has a committed fixture and a typed-lowering test.
  Opcodes 192/193 stay unreachable (OTP-30 additions to `genop.tab` this `erlc` cannot emit) — their typed
  lowering is covered by `src/lower/instr.rs` unit tests only. None of these falls back to `Generic`. Their
  *execution* is oracle-checked one level up at the `../tools/differential` gate (its `records.rs` `nr_native`
  module, run under both runtimes through `beam_rt`); `beam_engine`'s own suite covers them structurally
  only, and `put_record/6`'s cross-module create form is a typed `unsupported` there (see
  `../../crates/CONTEXT.md`).
- **Lowering is one peephole pass, not a full `ops.tab` transform.** `lower_instr` (`src/lower/instr.rs`) maps one decoded opcode to exactly one `Instr`. The only rewrite after that is `src/lower/fuse.rs` (`fuse`), a single forward pass that coalesces an **adjacent** `Deallocate(N); Return` pair in each function body into the one superinstruction `Instr::DeallocateReturn(N)`, run *before* `build_blocks` derives the blocks/labels (so a block's `start..end` indexes the same fused vector the engine dispatches on). `Label` is the only branch target, so two adjacent non-`Label` instructions share a block and the pair can never cross one; everything else is copied through verbatim, so `Deallocate`/`Return` stay live for hand-built modules that bypass the loader. That is the whole superinstruction surface here; the C builds a much larger set from `emu/ops.tab` in `beam_load.c` (a candidate for later). Never a correctness gap: `beam_engine`'s core `DeallocateReturn` arm executes exactly the two constituents in order.
- **The `Instr::Generic` fallback is reached only from the single catch-all arm** (`src/lower/instr.rs:611-628`): every other arm either lowers to a named variant or returns a typed error on an operand-shape mismatch — no opcode lowers to `Generic` *conditionally*.
- **No module MD5/checksum (the remaining recorded A.2 divergence).** The C builds `beam->checksum` as
  an MD5 over `AtU8`+`Code`+`StrT`+`ImpT`+`ExpT`+`FunT` (each `OldUniq` zeroed)+`LitT`+`Meta`+`Recs`+`DbgB`
  at load (`beam_file.c:1751-1830`); the loader computes no checksum (no `checksum`/`md5` symbol anywhere
  in `src/`), so `Module` carries no MD5. Closing it is a crate-layer decision, not a `beam_loader` one:
  `crates/beam_dist/src/auth.rs` already ships a self-contained RFC 1321 `md5`, so the algorithm belongs
  at a lowest-common-ancestor crate (`beam_codec`/`beam_common`) or behind a new dependency edge — do not
  duplicate it here.
- **Atoms are interned into a fresh per-module `AtomTable`**, not a shared `atom.names`-seeded table;
  positional/global atom identity for preloaded modules is a `beam_rt` concern.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/error.rs` → loader-specific error constructors over `beam_common::BeamError` (`bad_file`, `truncated`, …)
- `src/container.rs` → `FOR1` header + chunk directory walk, `ChunkId`, raw `Chunk` iteration
- `src/chunk/mod.rs` → chunk-parse dispatch + the assembled raw chunk set
- `src/chunk/atom.rs` → `AtU8` parser (UTF-8, index 0 = NIL)
- `src/chunk/code.rs` → `Code` chunk header (format/max_opcode/label_count/function_count)
- `src/chunk/table.rs` → `ExpT` / `LocT` / `ImpT` / `FunT` parsers (raw entries; `parse_locals` reuses the export reader)
- `src/chunk/line.rs` → `Line` table parser
- `src/chunk/types.rs` → `Type` chunk parser (`TypeTable`)
- `src/chunk/literal.rs` → `LitT` framing parser (both the verbatim and the zlib/RFC1950-compressed form) → raw ETF payloads
- `src/chunk/classify.rs` → `ChunkClass`/`ChunkKind`/`CHUNK_KINDS`/`classify`: the per-chunk-id classification (parsed+exposed / parsed-as-raw / tooling-only + reason) the corpus checker test enforces
- `src/chunk/misc.rs` → `StrT` / `CInf` / `Attr` / `Meta` raw capture (`DbgB`/`Dbgi` deliberately unparsed, tooling-only)
- `src/chunk/record.rs` → `Recs` parser: `RawRecord`/`RawRecordField`/`RawRecordDefault` + `parse` (raw-tag defaults, declaration order)
- `src/opcode.rs` → `Opcode` enum + name/number/arity roster (derived from `../../lib/compiler/src/genop.tab`)
- `src/operand.rs` → `Operand`/`Reg`/`TypeIndex`/`AllocKind`/`HeapNeed` types
- `src/decode.rs` → instruction-stream → `Vec<RawInstr>` (opcode + operands + byte offset) + the tagged-varint operand decoder
- `src/instr.rs` → the typed instruction vocabulary: `Instr` (107 variants) + `TestKind`
- `src/ir.rs` → the IR data types: `Module`, `Function`, `Export`, `Import`, `FunDescriptor`, `RecordDefinition`/`RecordField`, `Block`, `LineTable`, `TypeTable`, … (+ `pub use crate::instr::*`)
- `src/literal.rs` → `LiteralArea` / `SharedLiteralArea`: decoded `LitT` literals materialized into a heap (+ the shared `decode_terms`/`decode_terms_at_capacity` retry path)
- `src/attributes.rs` → `AttributeTable` / `SharedAttributeTable`: the decoded `Attr` chunk term (`Module::attributes_term`/`attribute_by_name`) for `module_info(attributes)`
- `src/lower.rs` → `build_module` + module assembly (re-exports the `group` helpers)
- `src/lower/group.rs` → `group_functions` (the `func_info` boundary rule) + `build_blocks`
- `src/lower/fuse.rs` → `fuse` (the loader's only superinstruction pass: adjacent `Deallocate;Return` → `DeallocateReturn`)
- `src/lower/instr.rs` → `lower_instr` (one `RawInstr` → one `Instr`)
- `src/load.rs` → `load` / `load_file` orchestrator
- `tests/fixtures/slice.erl` + `slice.beam` → the vertical-slice oracle fixture
- `tests/fixtures/coverage.erl` + `coverage.beam` → the widened-IR oracle fixture
- `tests/fixtures/records_fixture.erl` + `records_fixture.beam` → the `Recs`-chunk oracle fixture (native-record definitions for `records:get_definition/2`)
- `tests/fixtures/README.md` → regeneration commands + the full wire summary
- `tests/` → container/IR/widened-IR/records/negative/real-module integration tests
- `tests/records.rs` → `Recs`-chunk integration tests: declaration order, every default encoding, name/atom lookups, the pinned-`ORACLE` ↔ live-`erl` `records:get_definition/2` cross-check (bounded), and malformed-image rejection through `load` beside a well-formed control image
- `tests/generic_census.rs` → the residual `Instr::Generic` census over the installed OTP (asserts the unmodelled-opcode set equals the documented baseline)
- `tests/real_modules.rs` → broad real-OTP `.beam` LOAD sweep over the whole `lib` root (all 34 apps / 1366 modules recursively; every discovered module must load; loud skip when `erl`/OTP is absent) + the nested fun-literal MFA check over `maps`/`gen_server`
- `tests/native_record.rs` → the OTP-29 native-record family (186-191) over the committed `nr_def`/`nr_use` fixtures + the `erl_eval` `is_any_native_record` positive control + a stdlib/kernel/compiler sweep asserting no family member lowers to `Instr::Generic`
- `tests/line_info.rs` → typed `executable_line`/`debug_line` (the `linecov`/`dbgline` fixtures) + line-table/`func_info` preservation
- `tests/bs_test_tail.rs` → typed `bs_test_tail2` over the `bstail` fixture
- `tests/chunks.rs` → the whole-corpus chunk-id classification checker + the `Attr` oracle cross-check
- `tests/literals.rs` → the two `LitT` framings + the whole-corpus literal round-trip/oracle sweep
- `tests/load_cost.rs` → the committed module-load-cost measurement (ns/instruction, bytes/second)
- `tests/fixtures/{nr_def,nr_use,linecov,dbgline,bstail}.{erl,beam}` → native-record / line / test-tail oracle fixtures
- `tests/fixtures/literals_fixture.erl` + `literals_{verbatim,compressed}.beam` + `literals_oracle.escript` → the `LitT` framing fixtures + the Erlang-side literal oracle
- `tests/fixtures/bsalloc.erl` + `bsalloc.beam` → oracle fixture exercising the `bs_create_bin` `z,3` allocation operand
- `../../erts/emulator/beam/beam_file.c` → container + operand decoding + function grouping (read-only)
- `../../lib/compiler/src/genop.tab` → the authoritative opcode roster (read-only)
- `../../lib/compiler/src/beam_asm.erl` → the compiler-side writer of the same format (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the IR seam (`beam_loader` → `beam_engine`), dependency waves (§4, wave 2).
- `../../CONTEXT.md` §1 A.2 / §4 → the `.beam` binary surface and the execution-engine IR seam.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §E → loader/IR design rationale.
- `../beam_codec/CONTEXT.md` → the ETF codec whose `Reader`/`decode_term` this crate reuses; same error strategy.
- `../beam_alloc/CONTEXT.md` → the `Heap` the decoded literal area owns.
- `../beam_term/CONTEXT.md` → `Atom`/`AtomTable`/`Eterm` (the IR's atom identity and resolved literal type).
