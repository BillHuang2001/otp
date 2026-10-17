# `beam_engine/src/families/records` — the record instruction family

## Intent
Owns every record opcode: the OTP-26 **tagged-tuple** `update_record` and the OTP-29/30
**native-record** set. Record *semantics* (definition identity, field lookup by atom name,
accessibility, create vs update) live here; the record *layout* does not — the frozen
`ErtsRecordDefinition`/`ErtsRecordInstance` interiors are `beam_term::record`'s and are only
ever forwarded to.

Read `../CONTEXT.md` first: it holds the family seam's contract, the disjoint variant-ownership
rule, the `NO_FAIL` raise route (`raise_function_clause_no_fail`) and the shared capability-gap
list. The crate-level execution model is in `../../CONTEXT.md`, the per-opcode semantics and the
full gap list in `../../../INSTRUCTION_SURFACE.md`.

## Owned variants (9 — routed here by `../mod.rs::dispatch`)
- `UpdateRecord` — the tagged-tuple `#r{f = V}` update (`i_update_record_copy`,
  `emu/instrs.tab:785`). Copy-then-patch: `hint` is ignored (M1 has no destructive update, so
  the result is always a fresh copy — observationally equivalent because the compiler only emits
  `inplace` for a dead source) and `offset` is 1-based.
- The eight native-record variants (generic opcodes 186–193): `IsAnyNativeRecord`,
  `IsNativeRecord`, `GetRecordElements`, `GetRecordElementsId`, `PutRecord`,
  `IsRecordAccessible`, `GetRecordField`, `UpdateRecordId`. Every field reference is an **atom
  name** resolved against the record's *own* definition, never a compile-time table.
  `GetRecordElementsId`'s `Id` and `UpdateRecordId`'s `Hint`/`Id` are discarded by the C
  transforms, so both share their generic twin's implementation exactly.

## Routing Table
- `mod.rs` — `exec` (the variant dispatch), the shared helpers (`fail_step`, `read_atom`,
  `EXTERNAL`/`UNDERSCORE`), and the per-variant bodies (`create_native_record`,
  `local_definition`, `write_local_record`, the update paths). ~1086 lines: it is one cohesive
  family body and documents why at its top — do not grow it; new per-variant code goes to a
  submodule.
- `raw.rs` — the family's **single** `#[allow(unsafe_code)]` module. Every function forwards to
  `beam_term::record` (the term-layout LCA); no layout is re-derived. Its one hand-rolled build
  is `write_create_failure_reason` — the create form's `{Tag, {{Module,Name}, Field}}` reason
  graph, whose three tuples must share **one** rooted reservation.
- `tests/` — the per-variant unit suite (see Test Strategy).

## Failure model
- A **record-semantics** failure the C reports by branching (non-record source, definition
  `module`/`name` mismatch, absent field name, non-exported definition where accessibility is
  required) **branches to the instruction's `fail` label** — the C's `$FAIL` /
  `$FAIL_HEAD_OR_BODY`.
- `fail == NO_FAIL` ⇒ the shared failed-clause raise
  `crate::interpreter::raise_function_clause_no_fail`: class `error`, bare atom
  `function_clause`, resolved through the live atom namespace. Documented in
  `../CONTEXT.md` §"Capability gaps" — cross-reference it rather than restating.
- A genuine capability gap is a typed `error::unsupported`, never a wrong value:
  `put_record/6`'s **create** form for a **foreign** `{Module, Name}` record id
  (`local_definition`, "create: no record definition table for another module"). The C reaches
  another module's definition through the code server's record table
  (`erts_record_find_entry`) and may trap to `code:ensure_loaded/1`; this runtime has neither.
  **The running module's own create form IS implemented** (`create_native_record` from the
  module's own `Recs` definition), as is every *update* form (the source instance carries its
  own definition). Closable only by a host record-definition source — **OWNER:
  `beam_rt`/`beam_bifs`** (`beam_bifs`' `RecordSource` seam), not this node.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; `raw.rs` is this node's only non-test exemption, and every
  `unsafe` block carries its `// SAFETY:` rationale.
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no panic, no
  `unwrap()` on loaded data, never a wrong result.
- Term construction/inspection is **only** `beam_term` (via `crate::terms` + `raw.rs`); ordering
  uses `beam_term::{cmp, eq}`.
- Every record build reserves its block through the **rooted** seam
  (`crate::terms::reserve`/`*_rooted` → `Machine::alloc_rooted`) with `RootSet::ALL_LIVE`, and
  reads operand values **after** the reservation (frozen reserve → read → write ordering); the
  `live` operand is therefore carried but not consulted.

## Test Strategy
`tests/mod.rs` holds the shared scaffolding (`Fixture`, `record`, `field`, `labels_with`, `run`,
`raise_reason_atom`, the family-membership test) plus one submodule per opcode group; fixtures are
real native records built through `beam_term::write_record` into the `MinimalContext` arena and
instructions are hand-built `Instr`s driven straight through `exec` (no loader involved).
- `tests/native.rs` — `is_native_record`: matching definition, module/name mismatch, non-record,
  `NO_FAIL`, atom-operand decoding.
- `tests/elements.rs` — `get_record_elements` / `…_id`: list-order writes, branch-after-partial-
  write, non-record, `NO_FAIL`, and the unnameable-`function_clause` typed error.
- `tests/update.rs` — `put_record`'s update path (fresh copy, read-before-write) and create form,
  its absent-field `fail`, its two typed create-gap cases (unknown name, another module), plus
  `update_record_id`.
- `tests/access.rs` — `is_record_accessible`: `external` vs running-module (wildcard) scope.
- `tests/field.rs` — `get_record_field`: the local (atom id, `_` skips the name test) and foreign
  (`{Module,Name}`, export-required) forms.
- `tests/rooted.rs` — the two build paths under the mock **moving** collector (the
  rooted-allocation regression proof).

Each case asserts the success path, the fail-branch `Step::Goto`, or the `NO_FAIL` raise.
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.

## See Also
- `../CONTEXT.md` → the family seam, variant ownership, the `NO_FAIL` route and the handoff list.
- `../../../terms/` → `reserve`/`Slack` and the rooted builders the bodies call.
- `../../../../../beam_term/src/record.rs` → the frozen record layout this family forwards to.
- `../../../../../../erts/emulator/beam/erl_record.c` + `emu/instrs.tab:1323-1458` +
  `emu/ops.tab:1611-1694` → the C ground truth (read-only).
