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
  Allocation(Vec<AllocKind>) }` — a *typed* `.beam` operand.
- `pub struct AllocKind { pub kind: u32, pub number: u32, pub words: u32 }` — one decoded
  `z,3` allocation entry (`words` = the entry's contribution to the heap-word total).
### The IR (`src/ir.rs`)
- `pub struct Module { pub name: Atom, pub atoms: Vec<Atom>, pub atom_table: AtomTable,
  pub exports: Vec<Export>, pub imports: Vec<Import>, pub functions: Vec<Function>,
  pub code_version: CodeVersion, pub types: TypeTable, pub line: LineTable,
  pub literals: Vec<Vec<u8>>, pub strings: Vec<u8>, pub attributes: Option<Vec<u8>>,
  pub compile_info: Option<Vec<u8>> }`
  + `Module::atom_name(&self, Atom) -> Option<&str>`, `Module::export(&self, Atom, u32) -> Option<Label>`,
  `Module::function(&self, Atom, u32) -> Option<&Function>`.
  `atoms[i]` = the atom for wire index `i`; `atoms[0]` = `Atom::MAX` (NIL placeholder, never a real atom).
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
- `pub enum Instr` — named, typed variants for the Milestone-1 opcode subset, plus a lossless
  fallback: `Generic { op: Opcode, operands: Vec<Operand>, offset: u32 }`.
  Named variants: `Label(Label)`, `Line(LineRef)`, `FuncInfo { module: Atom, function: Atom, arity: u32 }`,
  `IntCodeEnd`, `Move { src: Operand, dst: Operand }`,
  `GcBif { arity: u8, bif: ImportRef, fail: Label, live: u32, args: Vec<Operand>, dst: Operand }`,
  `Test { kind: TestKind, fail: Label, args: Vec<Operand> }`,
  `SelectVal { value: Operand, fail: Label, pairs: Vec<(Operand, Label)> }`,
  `Swap { a: Operand, b: Operand }`, `Call { arity: u32, target: Label }`,
  `CallLast { arity: u32, target: Label, deallocate: u32 }`, `CallOnly { arity: u32, target: Label }`,
  `CallExt { arity: u32, target: ImportRef }`, `CallExtOnly { arity: u32, target: ImportRef }`,
  `CallExtLast { arity: u32, target: ImportRef, deallocate: u32 }`,
  `Allocate { stack_need: u32, live: u32 }`, `AllocateHeap { stack_need: u32, heap_need: u32, live: u32 }`,
  `TestHeap { heap_need: u32, live: u32 }`, `Deallocate(u32)`, `Trim { n: u32, remaining: u32 }`, `Return`.
  `pub enum TestKind` — the `is_*` family (`IsLt, IsGe, IsEq, IsNe, IsEqExact, IsNeExact, IsInteger,
  IsFloat, IsNumber, IsAtom, IsPid, IsReference, IsPort, IsNil, IsBinary, IsBitstr, IsList,
  IsNonemptyList, IsTuple, IsMap, TestArity`) with `TestKind::from_opcode`.
- `pub enum Opcode` (`src/opcode.rs`) — **one variant per generic opcode** (the full `genop.tab`
  roster), with `Opcode::from_u8(u8) -> Option<Opcode>`, `Opcode::as_u8(self) -> u8`,
  `Opcode::name(self) -> &'static str`, `Opcode::arity(self) -> u8`. The roster is data derived from
  `../../lib/compiler/src/genop.tab` (see Design Decisions); it is complete and lossless so
  unknown-to-M1 instructions still round-trip as `Instr::Generic`.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only** — no crate-local error enum. Malformed/truncated
  container, bad magic, bad format number, unknown opcode tag, out-of-range atom/label/register index,
  unsupported `z` sub-tag ⇒ `BeamError::BadArg`; a size/arity/count bound (arity > 255, more than
  `ERTS_BEAM_MAX_OPARGS`, negative chunk size, atom name not UTF-8-representable) ⇒ `BeamError::SystemLimit`.
  **Never panic, never `unwrap()` on file data, never silently accept a malformed image.**
- **`unsafe` is forbidden in this crate** (`#![deny(unsafe_code)]`) — the loader is the *safe
  periphery*. Zero-copy is achieved safely via `beam_codec::Reader::bytes(n) -> Result<&'a [u8]>`;
  no raw pointers are needed.
- **Reuse, don't duplicate.** The big-endian byte cursor is `beam_codec::Reader` (the ETF codec's
  bounds-checked cursor, the lowest common ancestor for BE byte reads) — do **not** write a second BE
  reader. Bounds/endianness live there.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_codec`, `libc`. Adding a
  dependency edge requires reporting to the crate-layer node (`../`).
- **Generated data is source, not output.** The operand-tag table and opcode roster come from the
  legacy generator/roster; the crate embeds a **regenerable** table (documented below) — never
  hand-guess a byte value.
- **Files ~1000 lines**, one responsibility per file. Licensing Apache-2.0.

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
  i32 num_free, i32 old_uniq }` (24-byte entries). Parsed into `Module`-level fun descriptors is M1-out;
  framing is validated and the raw entries retained (documented gap).
- **`Line`** — `u32 version` (must be 0, else the chunk is ignored), `u32 flags`, `u32 instr_count`,
  `u32 item_count`, `u32 name_count`; then `item_count-1` tagged items (`TAG_a <name_index>` /
  `TAG_i <line>`); then `name_count` × `{ i16 len, len bytes UTF-8 }`. `items[0]` is the undefined
  location `{0,0}`; `files[0]` is synthesized as `<module>.erl` (`beam_file.c:443-602`).
- **`Type`** — `u32 version` (`4` current, `3` OTP 27/28; else fallback), `i32 count` (≥1,
  `entries[0]` = "any"), then per entry `u16 type_word` (+ optional `i64 lower`, `i64 upper`, `u8 unit-1`
  gated by `type_word` bits 13-15). Absent/incompatible ⇒ a single "any" fallback entry.
- **`LitT`** — `u32 uncompressed_size` (`0` ⇒ payload stored verbatim; non-zero ⇒ zlib/deflate), then
  `i32 count` × `{ i32 ext_size, ext_size bytes }` (Erlang external-term bytes). **M1 parses the
  framing and stores the raw ETF bytes; it does NOT decode literals into terms** — and the **compressed
  (`uncompressed_size != 0`) form is unsupported** (no zlib dep) and returns a clear `BadArg`. Static
  literals are referenced by `Operand::Literal(idx)`.
- **`StrT` / `CInf` / `Attr` / `Meta` / `DbgB` / `Recs`** — data-only; captured raw (`StrT` → bytes,
  `Attr`/`CInf` → `Option<Vec<u8>>`). No parsing.
### Operand tags (`src/operand.rs`) — the authoritative table
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
  **CROSS-CRATE CONTRACT (binding on the future `beam_bifs` architect):** the engine/BIF layer keys BIFs
  by `(module, function, arity)` following the real `bif.tab` naming — the loader does **not** know and
  must **not** encode `bif.tab` numbering. `Module.imports` is the single source of BIF identity.
- **Two function labels per function.** The file's canonical sequence is
  `label FuncLabel | [line] | func_info M F A | label EntryLabel | body...` (`beam_file.c:2502-2589`).
  `Function` therefore carries both `func_label` and `entry_label`; `Export.label` equals `entry_label`.
  Functions are delimited by `func_info` instructions (the reader's own boundary rule).
- **Atoms are interned in file order into a crate-owned `AtomTable`.** `Module.atoms[i]` maps the wire
  index to the interning `Atom`; `atoms[0]` = `Atom::MAX` (NIL). `Operand::Atom` holds the interned
  `Atom`. Interning into a *shared, `atom.names`-seeded* table (needed for preloaded/positional atoms)
  is `beam_rt`'s job — see Known Issues.
- **`z,5` `tr` is normalized to `reg` + `TypeIndex`**, so the interpreter may ignore the annotation and
  a JIT may consume it later. The `Type` chunk is parsed into `TypeTable` (with the fallback table).
- **`Instr` is a named enum with a lossless `Generic` fallback.** The M1 interpreter subset gets typed,
  matchable variants (move/bif/test/select_val/call*/allocate/return/…); every other opcode is preserved
  as `Instr::Generic { op, operands, offset }` so nothing is lost and the engine/JIT can extend. The
  `offset` is the byte offset in the `Code` stream (for line/backtrace mapping).
- **Basic blocks are label-delimited.** `Function.blocks` is one block per label, `code[start..end]`;
  `Function.labels` is the label→index map an engine walks linearly with. CFG construction is the
  engine's concern.

## Test Strategy
- **Fixture (required):** `tests/fixtures/slice.beam` compiled from the checked-in
  `tests/fixtures/slice.erl` with the system `erlc`; `tests/fixtures/README.md` documents the exact
  regeneration command and the wire byte-summary. `slice.erl` exports `add/2`, `fib/1` and covers the
  required opcode subset: `label, line, func_info, move, gc_bif2, is_eq_exact, swap, call_only,
  call_ext_only, return, int_code_end` plus `tr` typed registers.
- **Ground truth captured once** (verified with `beam_disasm:file/1` + a byte decode of the oracle
  fixture) — the loader tests assert the disassembled IR for `add/2` and `fib/1` **matches this
  instruction sequence exactly**:
  - `add/2`: `label(1) line(1) func_info(slice,add,2) label(2) gc_bif2{bif=import0('+'/2),fail=0,live=2,args=[x0,x1],dst=x0} return`
  - `fib/1` entry (`func_label=3, entry_label=4`): `label(3) line(2) func_info(slice,fib,1) label(4) move(1,x2) move(0,x1) call_only(3,label6)`
  - `fib/3` (`func_label=5, entry_label=6`): `label(5) line(3) func_info(slice,fib,3) label(6) is_eq_exact{fail=7,args=[x0,0]} move(x1,x0) return label(7) line(4) gc_bif2{bif=import1('-'),fail=0,live=3,args=[x0,1],dst=x0} gc_bif2{bif=import0('+'),fail=0,live=3,args=[Typed{x,1,ty1},Typed{x,2,ty2}],dst=x1} swap(x1,x2) call_only(3,label6)`
  - `module_info/0` (`func_label=8,entry_label=9`): `move(atom slice, x0) call_ext_only(1, import2{erlang,get_module_info,1})`
  - atoms (wire order): `[NIL, slice, add, erlang, '+', fib, '-', module_info, get_module_info]`;
    imports: `[{erlang,'+',2},{erlang,'-',2},{erlang,get_module_info,1},{erlang,get_module_info,2}]`;
    exports: `[{add,2,label2},{fib,1,label4},{module_info,0,label9},{module_info,1,label11}]`.
- **Container tests:** parse the fixture and assert the chunk list, the `Code` header
  (`format_number=0`), atoms/exports/imports as above.
- **IR tests:** the instruction sequence above; label→index maps; block decomposition; `Export.label`
  == `Function.entry_label`.
- **Negative tests:** truncated header, bad `"FOR1"` magic, bad form type, `format_number != 0`,
  chunk size that overruns the file, unknown opcode tag, out-of-range atom/label index, truncated
  operand varint, `z` sub-tag `0`/unknown, compressed `LitT` ⇒ each a typed `BeamError` (no panic).
- **Gate (GREEN required):** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_loader`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` read-only), never `cd`, never touch `../tools/` or `../../erts/`.
- Cross-checks against the real system ERTS belong to the sibling `../tools/differential/` harness.

## Known Issues / Intentional M1 gaps
- **`LitT` literals are not decoded into terms** (raw ETF bytes only), and the **compressed form is
  unsupported** (no zlib dependency). Loading a module whose literals are needed at runtime therefore
  yields `Operand::Literal(idx)` + raw bytes; term materialization needs `beam_codec` + a binary ctor
  that `beam_codec` itself still lacks. Escalated to the parent node.
- **`FunT` lambdas are validated for framing but not surfaced as fun descriptors** — a module using
  `fun`/lambdas (opcodes `make_fun*`, `call_fun`) loads, but the fun descriptors are not modeled in M1.
- **Lambdas / maps / bitstrings / records opcodes are not in the M1 interpreter subset** — they decode
  losslessly as `Instr::Generic` but the engine's M1 loop does not execute them.
- **Atoms are interned into a fresh per-module `AtomTable`**, not a shared `atom.names`-seeded table;
  positional/global atom identity for preloaded modules is a `beam_rt` concern.
- **`executable_line`/`debug_line` opcodes (183/184) are `Generic`**; only `line/1` (via the `Line`
  chunk) drives `Instr::Line`. Column info is not surfaced.
- **`LocT` (jit-only local table) is not parsed** (irrelevant to interpretation).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/error.rs` → loader-specific error constructors over `beam_common::BeamError` (`bad_file`, `truncated`, …)
- `src/container.rs` → `For1` header + chunk directory walk, `ChunkId`, raw `Chunk` iteration
- `src/chunk/mod.rs` → chunk-parse dispatch + the assembled raw chunk set
- `src/chunk/atom.rs` → `AtU8` parser (UTF-8, index 0 = NIL)
- `src/chunk/code.rs` → `Code` chunk header (format/max_opcode/label_count/function_count)
- `src/chunk/table.rs` → `ExpT` / `ImpT` / `FunT` parsers
- `src/chunk/line.rs` → `Line` table parser
- `src/chunk/types.rs` → `Type` chunk parser (`TypeTable`)
- `src/chunk/literal.rs` → `LitT` framing parser (uncompressed; compressed ⇒ documented error)
- `src/chunk/misc.rs` → `StrT` / `CInf` / `Attr` / `Meta` / `DbgB` raw capture
- `src/opcode.rs` → `Opcode` enum + name/number/arity roster (derived from `../lib/compiler/src/genop.tab`)
- `src/operand.rs` → `Operand`/`Reg`/`TypeIndex`/`AllocKind` + the tagged-varint operand decoder
- `src/decode.rs` → instruction-stream → `Vec<RawInstr>` (opcode + operands + byte offset)
- `src/ir.rs` → the frozen IR: `Module`, `Function`, `Export`, `Import`, `Instr`, `Block`, `LineTable`, `TypeTable`, …
- `src/lower.rs` → `RawInstr` → functions/blocks/label maps (the `func_info` boundary rule)
- `src/load.rs` → `load` / `load_file` orchestrator
- `tests/fixtures/slice.erl` + `slice.beam` + `README.md` → the committed oracle fixture + regeneration notes
- `tests/` → container/IR/negative integration tests
- `../../erts/emulator/beam/beam_file.c` → container + operand decoding + function grouping (read-only)
- `../../lib/compiler/src/genop.tab` → the authoritative opcode roster (read-only)
- `../../lib/compiler/src/beam_asm.erl` → the compiler-side writer of the same format (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the IR seam (`beam_loader` → `beam_engine`), dependency waves (§4, wave 2).
- `../../CONTEXT.md` §1 A.2 / §4 → the `.beam` binary surface and the execution-engine IR seam.
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §E → loader/IR design rationale.
- `../beam_codec/CONTEXT.md` → the ETF codec whose `Reader` this crate reuses; same error strategy.
- `../beam_term/CONTEXT.md` → `Atom`/`AtomTable` (the IR's atom identity).
