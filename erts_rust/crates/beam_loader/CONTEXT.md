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
  `z,3` allocation entry (`words` = the entry's contribution to the heap-word total).
- `pub enum HeapNeed { Words(u32), Alloc(Vec<AllocKind>) }` — the heap/stack-need operand of the frame
  instructions, in *either* wire encoding (a plain word count or a `z,3` allocation list), with
  `HeapNeed::words(&self) -> u32` (the saturating word total) and
  `HeapNeed::from_operand(&Operand) -> Result<HeapNeed>`. It is the field type of
  `Instr::TestHeap { heap_need, live }`, `Instr::AllocateHeap { stack_need, heap_need, live }` and
  `Instr::Allocate { stack_need, live }`.
### The IR (`src/ir.rs` + the re-exported `src/instr.rs`)
- `pub struct Module { pub name: Atom, pub atoms: Vec<Atom>, pub atom_table: AtomTable,
  pub exports: Vec<Export>, pub imports: Vec<Import>, pub functions: Vec<Function>,
  pub funs: Vec<FunDescriptor>, pub code_version: CodeVersion, pub types: TypeTable, pub line: LineTable,
  pub literals: Arc<LiteralArea>, pub strings: Vec<u8>, pub attributes: Option<Vec<u8>>,
  pub compile_info: Option<Vec<u8>> }`
  + `Module::atom_name(&self, Atom) -> Option<&str>`, `Module::export(&self, Atom, u32) -> Option<Label>`,
  `Module::function(&self, Atom, u32) -> Option<&Function>`, `Module::literal(&self, u32) -> Option<Eterm>`,
  `Module::literals(&self) -> &LiteralArea`, `Module::fun(&self, u32) -> Option<&FunDescriptor>`,
  `Module::funs(&self) -> &[FunDescriptor]`.
  `atoms[i]` = the atom for wire index `i`; `atoms[0]` = `Atom::MAX` (NIL placeholder, never a real atom).
  `Module` is `Clone` (cheap: `literals` is shared behind the `Arc`) and `Debug`, but it is **neither
  `Send` nor `Sync`** (see `src/literal.rs` / Design Decisions) — a loaded module stays on its loading thread.
- `pub struct Export { pub function: Atom, pub arity: u32, pub label: Label }` — `label` = the function **entry label**.
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
- `pub enum Instr` — ~120 named, typed variants covering the opcode families the compiler emits, plus
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
    `Deallocate(u32)`, `Trim { n: u32, remaining: u32 }`, `Return`.
  - **tuples** — `PutTuple2 { dst, elements: Vec<Operand> }`,
    `GetTupleElement { source, element: u32, dst }`.
  - **lists** — `PutList { head, tail, dst }`, `GetList { source, head, tail }`,
    `GetHd { source, head }`, `GetTl { source, tail }`.
  - **maps** — `GetMapElements { fail, source, pairs: Vec<(Operand, Operand)> }`,
    `HasMapFields { fail, source, keys: Vec<Operand> }`,
    `PutMapAssoc { fail, source, dst, live, pairs }`, `PutMapExact { fail, source, dst, live, pairs }`.
  - **records** — `UpdateRecord { hint: Operand, size: u32, src, dst, updates: Vec<(Operand, Operand)> }`.
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
(instructions), `StrT`, `ImpT`, `ExpT`, `FunT`, `LitT`, `Attr`, `CInf`, `Line`, `LocT` (jit-only),
`Type`, `Meta`, `DbgB`, `Recs`, obsolete `Atom`.
### Per-chunk layouts (`src/chunk/`)
- **`AtU8`** — `i32 count` (negative ⇒ "long" varint lengths, real count = `-count`); then per atom a
  length (varint if long, else `u8`) + UTF-8 bytes. **UTF-8, not Latin-1** (`beam_file.c:291`).
  **Index 0 is reserved for NIL**; index 1 is the module name; real atoms are `1..=count`
  (`beam_file.c:262-297`). Total entries = `count+1`.
- **`Code`** — header `u32 head_size` (== 16) then `u32 format_number` (== `BEAM_FORMAT_NUMBER` 0,
  guarded), `u32 max_opcode`, `u32 label_count`, `u32 function_count`; then the instruction stream.
  The stream ends at the `int_code_end/0` instruction (genop 3), not at size exhaustion
  (`beam_file.c:1527,2596`). Instruction = 1 opcode byte (`1..=max_opcode`) + `Opcode::arity` operands.
- **`ExpT`** — `i32 count` × `{ i32 function_atom, i32 arity, i32 label }` (label = **entry** label).
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
  `entries[0]` = "any"), then per entry `u16 type_word` (+ optional `i64 lower`, `i64 upper`, `u8 unit-1`
  gated by `type_word` bits 13-15). Absent/incompatible ⇒ a single "any" fallback entry.
- **`LitT`** — `u32 uncompressed_size` (`0` ⇒ the records follow verbatim; non-zero ⇒ the rest of the
  chunk is a zlib (RFC 1950) stream that inflates to exactly `uncompressed_size` bytes, which ARE the
  records), then `i32 count` × `{ i32 ext_size, ext_size bytes }` (Erlang external-term bytes).
  `src/chunk/literal.rs` handles **both** forms — the compressed form via the public
  `beam_codec::compress::decompress`, rejecting a length that disagrees with `uncompressed_size` — and
  parses the **framing** only (raw ETF payloads); `src/literal::LiteralArea` then decodes each payload
  into a term once at load time. Static literals are referenced by `Operand::Literal(idx)`.
- **`StrT` / `CInf` / `Attr` / `Meta` / `DbgB` / `Recs`** — data-only; captured raw (`StrT` → bytes,
  `Attr`/`CInf` → `Option<Vec<u8>>`). No parsing.
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
  vocabulary is one large flat enum (~120 variants); an enum cannot be spread across submodules and
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
- **The `bs_create_bin` allocation operand accepts both encodings.** The compiler hoists a block's heap
  need into a preceding `bs_create_bin` (`beam_ssa_codegen.erl`, `need_heap_terminator`), so that `z`-op
  operand can be a bare word count or a `z,3` allocation list. `lower_instr` decodes it through
  `HeapNeed` (exactly as `test_heap`/`allocate` do), taking `HeapNeed::words()` for the `u32` field —
  matching the C `marshal_allocation_list`, which always marshals any `z,3` to a single word total.

## Test Strategy
- **Fixtures:** two committed oracle `.beam` files, both compiled from the checked-in `.erl` sources with
  the system `erlc` (OTP 29 / `erts-17.1`, the differential oracle); `tests/fixtures/README.md` documents
  the exact regeneration command and the full wire summary (chunk order/sizes, atoms, exports/imports,
  and the `Code`/`Line`/`Type`/`FunT`/`LitT` details).
  - `tests/fixtures/slice.beam` (712 bytes) from `slice.erl` — the vertical-slice module (`add/2`,
    `fib/1`, `pick/1`, plus `module_info/0,1`) covering the minimal generic opcode subset.
  - `tests/fixtures/coverage.beam` from `coverage.erl` — a broader idiomatic slice: tuples
    (`put_tuple2`/`get_tuple_element`/`select_tuple_arity`), lists (`put_list`/`get_list`), funs
    (`make_fun3` create + `call_fun2` call, populating `FunT`), `case`, `try … catch` (`try`/`try_case`/
    `try_end`), float arithmetic, maps (`put_map_assoc`/`put_map_exact`/`get_map_elements`), bitstrings
    (`bs_match`/`bs_start_match3`/`bs_get_*`/`bs_create_bin`), the `z,3` allocation-list need form, and
    `LitT` literals (`{error,division_by_zero}`, `{error,badarith}`, `#{a=>1,b=>2}`).
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
  and a compressed `LitT` ⇒ each a typed `BeamError` (no panic).
- Unit tests live in-module (`#[cfg(test)]`) in every `src/` file.
- **Gate (GREEN required):** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` read-only), never `cd`, never touch `../tools/` or `../../erts/`.
- Cross-checks against the real system ERTS belong to the sibling `../tools/differential/` harness.

## Known Issues / Intentional M1 gaps
- **Residual `Instr::Generic` opcodes.** The opcodes the loader deliberately does not model still lower
  losslessly to `Instr::Generic`: the **OTP-29 native-record family** (`is_any_native_record/2`,
  `is_native_record/4`, `get_record_elements/3`, `put_record/6`, `is_record_accessible/3`,
  `get_record_field/5`, `get_record_elements_id/4`, `update_record_id/7`), whose operands need
  record-descriptor modelling the IR does not own yet, and the obsolete/transform-only `genop.tab`
  entries (e.g. `is_constant/2`, `make_fun2/1`) the compiler never emits.
- **`EXPORT_EXT` / `NEW_FUN_EXT` fun literals are not decoded** (an upstream `beam_codec` limitation:
  `decode_term` returns `unsupported_external_term`). They appear **nested** inside tuples/maps (e.g.
  `{2, fun maps:remove/2}`, `#{report_cb => fun M:F/1}`), so a top-level-only workaround in `LiteralArea`
  cannot reach them — the shared ETF walker in `beam_codec::decode` must. This is the dominant remaining
  load blocker (~36 of 261 real OTP modules: `maps`, `gen_server`, `proc_lib`, `supervisor`, `logger*`,
  `json`, `compile`, `shell_docs`, …). It needs a fun/export *term layout* that `beam_term` does not
  currently own, so it is a coordinated cross-crate change (`beam_term` layout + `beam_codec` decode +
  `beam_engine` consumption), not a `beam_loader` fix. `BINARY_EXT`/`BIT_BINARY_EXT` ARE already decoded.
- **Atoms are interned into a fresh per-module `AtomTable`**, not a shared `atom.names`-seeded table;
  positional/global atom identity for preloaded modules is a `beam_rt` concern.
- **`LocT` (jit-only local table) is not parsed** (irrelevant to interpretation).
- **`Type` chunk version 3 (OTP 27/28) is parsed with version-4 bit positions** — the OTP 27/28 layout
  shifts the metadata bits (`beam_file.c:654-687`) and is not re-mapped, so such a chunk yields plausible
  but possibly wrong bounds. Version 4 (OTP 29+) is correct.
- **`ChunkId::DBGB` names `"DbgB"`**, but the real debug-info chunk id is `"Dbgi"` (present in the
  fixture); `Dbgi` is therefore treated as an unknown chunk and skipped — the desired behavior.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/error.rs` → loader-specific error constructors over `beam_common::BeamError` (`bad_file`, `truncated`, …)
- `src/container.rs` → `FOR1` header + chunk directory walk, `ChunkId`, raw `Chunk` iteration
- `src/chunk/mod.rs` → chunk-parse dispatch + the assembled raw chunk set
- `src/chunk/atom.rs` → `AtU8` parser (UTF-8, index 0 = NIL)
- `src/chunk/code.rs` → `Code` chunk header (format/max_opcode/label_count/function_count)
- `src/chunk/table.rs` → `ExpT` / `ImpT` / `FunT` parsers (raw entries)
- `src/chunk/line.rs` → `Line` table parser
- `src/chunk/types.rs` → `Type` chunk parser (`TypeTable`)
- `src/chunk/literal.rs` → `LitT` framing parser (raw ETF payloads; compressed ⇒ documented error)
- `src/chunk/misc.rs` → `StrT` / `CInf` / `Attr` / `Meta` / `DbgB` / `Recs` raw capture
- `src/opcode.rs` → `Opcode` enum + name/number/arity roster (derived from `../../lib/compiler/src/genop.tab`)
- `src/operand.rs` → `Operand`/`Reg`/`TypeIndex`/`AllocKind`/`HeapNeed` types
- `src/decode.rs` → instruction-stream → `Vec<RawInstr>` (opcode + operands + byte offset) + the tagged-varint operand decoder
- `src/instr.rs` → the typed instruction vocabulary: `Instr` (~120 variants) + `TestKind`
- `src/ir.rs` → the IR data types: `Module`, `Function`, `Export`, `Import`, `FunDescriptor`, `Block`, `LineTable`, `TypeTable`, … (+ `pub use crate::instr::*`)
- `src/literal.rs` → `LiteralArea` / `SharedLiteralArea`: decoded `LitT` literals materialized into a heap
- `src/lower.rs` → `build_module` + module assembly (re-exports the `group` helpers)
- `src/lower/group.rs` → `group_functions` (the `func_info` boundary rule) + `build_blocks`
- `src/lower/instr.rs` → `lower_instr` (one `RawInstr` → one `Instr`)
- `src/load.rs` → `load` / `load_file` orchestrator
- `tests/fixtures/slice.erl` + `slice.beam` → the vertical-slice oracle fixture
- `tests/fixtures/coverage.erl` + `coverage.beam` → the widened-IR oracle fixture
- `tests/fixtures/README.md` → regeneration commands + the full wire summary
- `tests/` → container/IR/widened-IR/negative/real-module integration tests
- `tests/real_modules.rs` → broad real-OTP `.beam` LOAD sweep (loud skip when `erl`/OTP is absent; allowlisted known failures)
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
