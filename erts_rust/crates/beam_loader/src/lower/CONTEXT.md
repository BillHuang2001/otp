# `beam_loader/src/lower/` — Opcode → Typed-IR Lowering

## Intent
Turns the decoded `RawInstr` stream into the typed, block-structured IR `beam_engine` consumes: opcode→`Instr` mapping, `func_info` function grouping, the one superinstruction pass, and post-lowering validation.
Owns the *semantic* half of the loader; the byte/operand decode is `../decode.rs`, the IR data types are `../ir.rs` + `../instr.rs`, and module assembly is `../lower.rs` (`build_module`).
Ground truth: the `=>` transforms and opcode definitions in `../../../../erts/emulator/beam/emu/ops.tab` + `beam_load.c` + `beam_transform_helpers.c`, and `../../../../erts/emulator/beam/jit/asm_load.c` (validation asserts). Read them; never modify them.

## Pipeline (inside `build_module`)
1. `decode::decode_stream` → `Vec<RawInstr>` (`../decode.rs`).
2. `group::group_functions` — split into `Function`s at the `func_info` boundary; each function gets `func_label`/`entry_label`.
3. `lower::instr::lower_instr` — one decoded opcode → exactly one `Instr` variant (or the lossless `Instr::Generic`).
4. `fuse` — coalesce an **adjacent** `Deallocate(N); Return` pair into `Instr::DeallocateReturn(N)`. Runs *before* `build_blocks`, so a block's `start..end` indexes the same fused vector the engine dispatches on. `Label` is the only branch target, so the pair can never cross a block; everything else is copied through verbatim, so `Deallocate`/`Return` stay live for hand-built modules.
5. `group::build_blocks` — label-delimited blocks (`blocks[k]` = `code[start..end]` beginning at `blocks[k].label`); CFG construction is the engine's concern.
6. `validate` — the post-lowering accept/reject pass below.

## Rules
- **`instr.rs` is the whole semantic mapping.** Every opcode the compiler emits maps to one named `Instr`; every other opcode is preserved losslessly as `Instr::Generic { op, operands, offset }` (`offset` = the byte offset in the `Code` stream, for line/backtrace mapping). `Generic` is reached only from the single catch-all arm: every other arm either lowers to a named variant or returns a typed error on an operand-shape mismatch.
- **Two labels per function**, delimited by `func_info`: `label FuncLabel | [line] | func_info | label EntryLabel | body...`, with `Export.label` = `entry_label`.
- **`bs_create_bin`'s allocation operand accepts both encodings.** The compiler hoists a block's heap need into a preceding `bs_create_bin`, so that `z`-op operand can be a bare word count or a `z,3` allocation list; `lower_instr` decodes it through `HeapNeed` (exactly as `test_heap`/`allocate`) and takes `HeapNeed::words()` for the `u32` field.
- **A `put_record` create-form `Id` decodes to `Operand::Atom` (local) or `Operand::Literal(idx)` (foreign)** — measured, not inferred: the wire uses the `a` tag for the running module's own record and a `z,4` literal for a foreign one, whose term is the 2-tuple `{Module,Name}` (`erl_record.c:356-410` reads it with `boxed_val(id)`); `Operand` has no inline-tuple variant, so a literal index is the only representable form and `lower_instr` copies the operand verbatim — nothing narrows a `put_record` `Id` to an atom.
  Over the committed OTP-30 bootstrap corpus (`bootstrap/lib/{compiler,kernel,stdlib}/ebin`, 262 beams) the **2810** create forms split **889** `Operand::Atom` (each a record of the module's own `Recs`) + **1921** `Operand::Literal` (each a `{Owner,Name}` pair whose owner is in the corpus and whose record is `is_exported`) + **0** other shapes; an independent re-decode of the whole `Code` stream agrees instruction for instruction.
  **Trap:** `v3_core.beam` carries **583** create forms and **all 583 are local atoms** (its own `#imodule`/`#c_literal`/`#core`/… records), so an atom `Id` there is correct, not a mis-decode; the foreign form appears in `beam_ssa_opt` (110) and 40 other compiler modules.
  `Operand`'s derived `Debug` renders `Atom(Atom(4))` / `Literal(3)` (raw index, never a name), so no loader-side dump can print a `{Module,Name}` literal as an atom; the *engine*'s local-create error is the only message that names an atom.
- **`validate.rs` (called from `build_module` after `group_functions`)**: a label definition must satisfy `0 < label < label_count` and be unique (`asm_load.c:512-529`), the `func_info` count must equal the header's `function_count` (`:625-629,737-743`), a `func_info` arity must be `<= MAX_ARG` (`:636-639`) ⇒ `SystemLimit`, every `ImportRef` must be `< Module.imports.len()` (`:532-548`), every `Line`/`ExecutableLine`/`DebugLine` `LineRef` must be `< Module.line.items.len()` (`:324-341`), and a `FunT` entry may neither repeat a label (`:150-165`) nor use label 0 (`beam_file.c:429`). A `label_count`/`function_count`/empty-item table of 0 (a hand-built module with no `Code` header) skips the corresponding rule.
- `tests/negative_ir.rs` pins each code-level rejection as an end-to-end `load()` failure beside an unmutated control, located by decoding a fixture and patching one operand in place.

## Constraints
- `beam_common::{Result, BeamError}` only; a construct the C rejects is a typed error here too, never a silently-accepted wrong value.
- `unsafe` forbidden (crate-wide).
- One responsibility per file; `lower.rs` (module assembly) is the only file here-adjacent part that runs long — keep new rules in the specific submodule.

## Design Decisions
- **Lowering is one peephole pass, not a full `ops.tab` transform.** An audit of the C transform surface (`beam_load.c` + the `=>` transforms in `emu/ops.tab`/`beam_transform_helpers.c`) concluded the current surface is **complete for correctness**: every load-time rewrite the C applies is performance-only (generic→specific selection, fusion, `select`-family sorting, key sorting, precomputed hashes, constant folding) or — for the data-dependent native-record/`Recs` resolution — reproduced at runtime by the engine. The sole genuine capability gap is the **foreign** native-record create form (a typed `unsupported` at the engine, never a wrong result).
- **Basic blocks are label-delimited and built after fusion**, so a `Function`'s `labels` map and `blocks` indexes agree with the vector the engine dispatches on.

## Known Issues
- **Looser than the C, deliberately:** an obsolete/transform-only roster opcode (`-allocate_zero/2`, `-is_constant/2`, `-make_fun2/1`; none has an `ops.tab` line) lowers to `Instr::Generic` instead of `PLEASE_RECOMPILE` (`beam_load.c:600-605`); an `f` target no `label` defines is accepted (a release-build C accepts it too) and surfaces as a typed execution error; the `bs_match` sub-instruction vocabulary is only the engine's typed `unsupported` (`beam_load.c:625-638`).
- **Stricter than the C:** a defined label 0 is refused (the C tests only `< 0 || >= label_count`, `asm_load.c:512-518`).

## Notes for Agents
- Read `src/lower/instr.rs` for the per-opcode table; read `src/instr.rs` (`../instr.rs`) for the variant shapes and `../CONTEXT.md` for the vocabulary summary.
- Do not add a load-time optimisation that changes `Instr` shapes without re-running the whole-corpus census (`tests/generic_census.rs`) and the differential gate.

## Routing Table
- `instr.rs` → `lower_instr` (the opcode → `Instr` mapping, one arm per modelled opcode).
- `group.rs` → `group_functions` (the `func_info` boundary rule) + `build_blocks`.
- `fuse.rs` → `fuse` (the only superinstruction pass, `Deallocate`+`Return`).
- `validate.rs` → the post-lowering validation pass (label definitions, `function_count`, `func_info` arity, import refs, `LineRef`s).
- `../../../tests/negative_ir.rs` → the code-level rejection suite that pins `validate.rs` (test node, read-only here).
