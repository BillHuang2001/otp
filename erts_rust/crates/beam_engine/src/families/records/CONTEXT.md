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

## Record-definition resolution (the create form)
`put_record`'s create form (`Src` the literal `nil`, `emu/ops.tab:1648-1651`) is the one record
instruction that must **resolve a definition** before it can build anything, and its `Id` operand
has two shapes:
- An **atom** `Id` names a record of the running module: the definition comes from the module's own
  `Recs` table (`beam_loader::Module::record`) and carries **no export requirement**
  (`i_create_local_native_record` → `erl_create_local_native_record`, `erl_record.c:254-354`).
- A **`{Module,Name}`** `Id` takes the generic form (`i_create_native_record` →
  `erl_create_native_record`, `erl_record.c:356-410`), which **requires `is_exported`**: a
  non-exported definition is the class-`error` raise `{badrecord, Id}` (`:401-409`), never a silent
  create. That gate applies even when `Module` **is** the running module.
- A `{Module,Name}` `Id` naming **another** module asks the host through the defaulted
  `ExecutionContext::foreign_record_definition(owner, name)`: `Ok(Some(def))` with `is_exported`
  builds the instance (the `Id`'s `Module` becomes its definition owner), `Ok(Some(def))` without it
  is the same `{badrecord, Id}` raise, and `Ok(None)` — no record table, or a module the host cannot
  resolve — keeps the honest typed
  `error::unsupported("… create: no record definition table for another module")`. A definition is
  never guessed.
- **No `code:ensure_loaded/1` trap.** C's "the defining module is not loaded ⇒ load it, then retry"
  half (`erl_create_native_record_jit`, `erl_record.c:412-565`) has no counterpart here (this runtime
  has no code server on the execution path), so that question is the host's `Ok(None)`.
- **Atom namespace.** A definition's record name and each field name are re-resolved through the live
  namespace seam (`ExecutionContext::atoms`, the running module's snapshot as fallback) **before** any
  of them is written into a term; a name the namespace cannot spell is a typed `unsupported`, so a
  foreign table's atom index never names a different atom in the result.
- The instance's definition block carries the **real cached hash**
  (`beam_term::hashing::compute_record_definition_hash`, C's
  `make_hash2({Module,Name,IsExported,Keys})`), never a placeholder — it is observable through
  `erlang:phash2/1`.

## Routing Table
- `mod.rs` — the dispatch (`exec`) plus the shared helpers every body uses (`fail_step`, `read_atom`,
  `read_record_id`/`RecordId`, `finding`, `record_definition`) and the read/update bodies
  (`get_record_elements`, `get_record_field`, `update_native_record`, `write_record_instance`), incl.
  the `EXTERNAL`/`UNDERSCORE` scope atoms. New per-variant work goes to a submodule, not here.
- `create.rs` — the `put_record` **create** path: the definition resolver
  (`resolve`/`Resolved`/`Resolution`, incl. the host seam and the live-namespace re-resolution), the
  instance writer (`create_native_record`/`write_created_record`) and the two failure graphs
  (`create_failure`'s `{Tag, {{Module,Name}, Field}}` and `create_badrecord`'s `{badrecord, Id}`).
- `raw.rs` — the family's **single** `#[allow(unsafe_code)]` module. Every function forwards to
  `beam_term::record` (the term-layout LCA); no layout is re-derived. Its one hand-rolled build
  is `write_create_failure_reason` — the create form's `{Tag, {{Module,Name}, Field}}` reason
  graph, whose three tuples must share **one** rooted reservation.
- `tests/` — the per-variant unit suite (see Test Strategy); `tests/foreign_create.rs` is the create
  form's foreign-definition suite over the host seam.

## Failure model
- A **record-semantics** failure the C reports by branching (non-record source, definition
  `module`/`name` mismatch, absent field name, non-exported definition where accessibility is
  required) **branches to the instruction's `fail` label** — the C's `$FAIL` /
  `$FAIL_HEAD_OR_BODY`.
- `fail == NO_FAIL` ⇒ the shared failed-clause raise
  `crate::interpreter::raise_function_clause_no_fail`: class `error`, bare atom
  `function_clause`, resolved through the live atom namespace. Documented in
  `../CONTEXT.md` §"Capability gaps" — cross-reference it rather than restating.
- A genuine capability gap is a typed `error::unsupported`, never a wrong value. In this family they
  are all in the create form: an atom `Id` naming a record the running module does not define; a
  `{Module, Name}` `Id` naming **another** module on a host whose `foreign_record_definition`
  answers `Ok(None)` (see "Record-definition resolution"); a definition whose names the caller's
  atom namespace cannot spell; and an error tag atom the running module cannot name. Everything the
  definitions can answer **builds** — the running module's own create form, a foreign create through
  the seam, and every *update* form (the source instance carries its own definition).

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
- `tests/update.rs` — `put_record`'s update path (fresh copy, read-before-write), `update_record_id`,
  and the create form's no-host cases: an undefined own record name and a foreign id on a
  `MinimalContext` with no record table are the typed `unsupported`.
- `tests/foreign_create.rs` — the create form against a **host record table**, 14 tests: the seam's
  exported, non-exported and `Ok(None)` branches (the `badrecord` one on both the `NO_FAIL` raise and
  a live `fail` label), the created instance's identity and real cached hash, the
  `{SelfModule, Name}` export gate, the atom-`Id` local form's freedom from it, `badfield` outranking
  `no_value`, the first-absent-field `no_value`, `Some(small 0)` vs an absent default, two
  unnameable-name rows, and interpreter / baseline-tier parity.
- `tests/access.rs` — `is_record_accessible`: `external` vs running-module (wildcard) scope.
- `tests/field.rs` — `get_record_field`: the local (atom id, `_` skips the name test) and foreign
  (`{Module,Name}`, export-required) forms.
- `tests/rooted.rs` — the two build paths under the mock **moving** collector (the
  rooted-allocation regression proof).

Each case asserts the success path, the fail-branch `Step::Goto`, or the `NO_FAIL` raise.

**There is no `erlc`/`erl` oracle for the native-record family**: the installed toolchain
(OTP 29.1.1, `erl_features:configurable()` = `[compr_assign,maybe_expr]`) never emits it
(`erlc -enable-feature native_record` answers "the feature 'native_record' does not exist"), so every
expectation above is pinned from the C ground truth (`erts/emulator/beam/erl_record.c`,
`emu/instrs.tab`, `emu/ops.tab`, `emu/macros.tab`) — no oracle agreement is claimed.

Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.
## Known divergences (open, verified against the C)
- **`is_record_accessible`'s wildcard arm compares against the wrong atom.** `emu/ops.tab:1672-1678` replaces the file's `Scope` operand with `$MODULE` (`erts/emulator/utils/beam_makeops:2554` ⇒ `st->module`), so the C's test is `defp->is_exported == am_true || defp->module == <running module>` (`erl_record.c:773-780`). The compiler never writes the module name there — it writes `external` or `auto_local` (`beam_core_to_ssa.erl:904-909`) — so `record_definition_module_is(source, scope)` compares the definition's module against the literal atom `auto_local` and never matches: a non-exported record defined in the running module fails a test the oracle accepts. The wildcard arm must compare against `module.name`, not `scope`.
- **`fail == NO_FAIL` loses the record reason on three opcodes.** `put_record`'s *update* form, `update_record_id` and `get_record_field` reach `$FAIL_HEAD_OR_BODY` only **after** the helper set `p->freason`/`p->fvalue` (`erl_record.c:746-756` `badfield`, `:814-821`/`:872-879` `badfield`, `:790-803`/`:832-861` `badrecord`), so a `0` fail (`goto find_func_info`, `emu/macros.tab:266-278` ⇒ `expand_error_value`, `beam_common.c:754-768`) raises class `error` with `{badfield, {{Module,Name}, Field}}` or `{badrecord, Src}` — **not** `function_clause`. `fail_step` currently raises `function_clause` for all three; only the four `$FAIL`-only variants (`is_any_native_record`, `is_native_record`, `get_record_elements`, `is_record_accessible`) match the C. `{f,0}` reaches a test only via `bif_fail({catch_tag,_})` (`beam_ssa_codegen.erl:1564-1565`), i.e. a test whose failure block is a `catch` entry.
- **`foreign_record_definition` cannot express "this module defines no such record".** `Ok(None)` means both "no record table at all" (typed `unsupported`) and "the module is resolved and has no such record", where C's create path ends in `error:{badrecord, Id}` even after its load retry (`erl_record.c:509-565` ⇒ `erl_create_native_record(…, indicate_unloaded=false)` ⇒ `:407-409`). A host wanting the oracle's reason can answer with a **non-exported** definition — its fields are never read on the `badrecord` arm (`create::resolve`), so that is a zero-engine-change route.

## Coverage notes
- The `differential` gate's `records` group (46 cases: 35 `Eval` + 11 `Module`) pins every
  `records:*` BIF and six of the eight opcodes (186–191; 192/193 are unreachable from the
  installed OTP-29 `erlc`). Every case **creates** its record through `records:create/4`, so
  `PutRecord`'s create form has no oracle-verified case: the local form is proven structurally
  only (here + `tests/update.rs`), and the corpus module's `-record #rec{a, b}` has defaultless
  fields, so a compile-time `#rec{a = 1}` there would be a `novalue` error — a positive case
  needs a record whose fields all carry defaults.
- Stale-comment trap: the module doc of `tools/differential/src/corpus/records.rs` ("The module
  case") and `crates/beam_rt/tests/records.rs` still describe the create form as unimplemented.
  The **local** create form IS implemented here; only a *foreign* `Id` is the gap.

## Handoff: a foreign `PutRecord` create id (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
4. **A *foreign* `PutRecord` create id -- closed by a defaulted engine seam, residual only at the host.** `PutRecord`'s create form for the running module's own records is built in-crate, and an `Id` naming **another** module's record is served by `ExecutionContext::foreign_record_definition(owner, name)` (default `Ok(None)`): `Ok(Some(def))` is the host's record table answering `erl_record.c::erts_record_find_entry`, the engine builds the instance only when the definition is exported (a non-exported one is the C's `error:{badrecord, Id}` raise, `erl_record.c:401-409`), and the `{Module, Name}` form enforces that even when `Module` is the running module. The residual is a host that answers `Ok(None)` -- no record table, or a defining module it cannot load -- which keeps the typed `error::unsupported("put_record: create: no record definition table for another module")`; the engine has no `code:ensure_loaded/1` trap (`erl_create_native_record_jit`, `erl_record.c:412-507` + `erl_finish_create_record_jit`), so the load-then-retry half is the host's, and a created instance stores no definition pointer (the M1 substitution). Pinned in `src/families/records/tests/` and `tests/gaps.rs`.

## See Also
- `../CONTEXT.md` → the family seam, variant ownership, the `NO_FAIL` route and the handoff list.
- `../../../terms/` → `reserve`/`Slack` and the rooted builders the bodies call.
- `../../../../../beam_term/src/record.rs` → the frozen record layout this family forwards to.
- `../../../../../../erts/emulator/beam/erl_record.c` + `emu/instrs.tab:1323-1458` +
  `emu/ops.tab:1611-1694` → the C ground truth (read-only).
