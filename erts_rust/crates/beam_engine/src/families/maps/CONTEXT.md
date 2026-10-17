# `beam_engine::families::maps` — the map instruction family

## Intent
The single owner of the four map `beam_loader::Instr` variants —
`GetMapElements`, `HasMapFields`, `PutMapAssoc`, `PutMapExact` — dispatched from
`../mod.rs::dispatch` (variant-partitioned router). Map layout is **read** through
`crate::terms::map_entries` (both frozen layouts: flat map and HAMT) and **built** through
`crate::terms::make_map` → `beam_term::map::write_map` (flat ≤ 32 distinct keys, else HAMT);
key equality is `beam_term::cmp` (exact `=:=`, so `1` and `1.0` are distinct keys). The family
borrows `module: &Module` for the atom table it compares keys with.

Ground truth (read-only): `../../../../erts/emulator/beam/emu/map_instrs.tab:49-173`,
`beam_common.c:2237-2268` (`erts_gc_update_map_exact` sets `BADKEY`), `emu/ops.tab:1364-1424`,
`beam/generators.tab:230-253`, and — for the read path's branchless failure — `emu/beam_emu.c:578-583`
(`i_func_info_IaaI`) + `beam_common.c:448-544` (`handle_error`, the `function_clause` reason).

## Layout
- `mod.rs` — `exec` (dispatch), `lookup`/`position` (linear `beam_term::cmp` scans),
  `goto`, `fail_or_raise` (`:238`), `fail_or_function_clause` (`:260`),
  `raise_error_tuple` (`:280`).
- `read.rs` — `get_map_elements` (`:54`), `has_map_fields` (`:82`).
- `write.rs` — `put_map_assoc` (`:68`), `put_map_exact` (`:119`); both reserve the whole map
  block with one rooted allocation (`make_map_rooted` + `merge_slack_words`) and re-read every
  operand from the post-reserve machine state, so a moving collector may run in the reserve.
- `tests.rs` — the unit suite (both layouts incl. the 32/33-key boundary, all failure paths).

## Failure paths
| Site | Condition | `fail != NO_FAIL` | `fail == NO_FAIL` |
|---|---|---|---|
| `read.rs:65,93` / `write.rs:84,135` | non-map `source` | jump | raise `{badmap,Source}` |
| `write.rs:141` | `put_map_exact` key absent | jump | raise `{badkey,Key}` |
| `read.rs:73,99` | key absent | jump | raise the **bare atom** `function_clause` |

`fail_or_raise` (`mod.rs:238`) forwards the `{Tag,Value}` raise to `raise_error_tuple`
(`mod.rs:280`); `fail_or_function_clause` (`mod.rs:260`) forwards the absent-key `NO_FAIL` branch to
the engine's single failed-clause definition `raise_function_clause_no_fail`
(`dispatch.rs`; call site `mod.rs:267`, `what = "maps"`), which throws class `error`
with the bare atom `function_clause` (no tuple — the reason is not `{Tag,Value}`).

### The absent-key `NO_FAIL` reason (engine-defined)
The read path's C failure is the plain `$FAIL($Fail)` (`emu/map_instrs.tab:49-122` →
`emu/macros.tab:56-60`), which sets **no** exception reason; only `erts_gc_update_map_exact` sets
`BADKEY` (`beam_common.c:2265`). The installed `erlc` never emits `NO_FAIL` for the matching forms
(both read instructions always carry a fail label), so this branchless failure has no ground truth.
It is engine-defined: class `error`, reason the **bare atom** `function_clause` — the reason C gives
a head/guard/pattern failure with no branch (`emu/beam_emu.c:578-583` `i_func_info_IaaI`). The raise
is the engine's single failed-clause definition,
`crate::interpreter::raise_function_clause_no_fail` (a `pub(crate)` re-export of
`dispatch.rs`; `what = "maps"`), which resolves the
tag by name from the running module's atom table (like `badmap`/`badkey`), so a module that cannot
name it yields the typed `error::unsupported("maps: no 'function_clause' atom in the module")`
instead of a substituted atom.

## The reason-atom contract (load-bearing)
- `raise_error_tuple` and `raise_function_clause_no_fail` (`interpreter/dispatch.rs:1263`) resolve
  every reason/class tag through **`machine::namespace_atom(ctx, module, name)`**
  (`../../machine/catch.rs:761`): `ctx.atoms(module).lookup(name)` **first**, the running module's
  load-time `atom_table` snapshot **second**. A name **neither** source can name is the typed
  `error::unsupported("maps: no '<name>' atom in the module")`, which `error::unsupported` renders as
  the reason atom `unsupported_opcode: maps: no '<name>' atom in the module`.
- **Why the live namespace comes first.** `ExecutionContext::atoms` (`../../context.rs`) is the
  comparison/naming seam; a host that owns ONE shared, live atom namespace overrides it (`beam_rt`'s
  `RuntimeContext` **and** `BudgetContext` both do). A `Module` is `Arc`-shared and its `atom_table` is
  an immutable load-time snapshot, so an atom interned into the shared namespace *after* the module was
  loaded has an id beyond the snapshot and the module can never name it — while C resolves reason atoms
  from its global `am_error`/`am_badmap`/… table and is never in that state.
- An atom index is only meaningful against the table it is read with, so the seam's host contract is:
  **the returned index must be valid in the running module's table.** The engine never fabricates or
  substitutes an index; a miss keeps the typed gap.
- **Load-time naming is still a host obligation** for a host that does **not** override `atoms`:
  everything the runtime must be able to name when the module never mentions it must be interned
  *before* the module is loaded, since the host clones that table into each module.
  - `beam_rt`: `boot::CORE_ATOMS` contains `badmap`, `badkey` and `function_clause`; `Runtime::new`
    seeds it before any load, and `CodeServer::load_bytes` clones the shared table into each module —
    and with the `atoms` override the live namespace answers regardless.
  - engine test host: `SEED_ATOMS` (`../../../tests/support/atoms.rs`) is appended to every module
    loaded through it.
  - `beam_loader::load`/`load_file` builds a **private, unseeded** table: for a `.beam` from the system
    `erlc` (OTP 29) the compiler interns `badmap` itself (its `test,is_map` guard fail path builds
    `{badmap,X}`) but **not** `badkey`, and `function_clause` only for a module with a head clause — a
    module that does not is the typed gap on that host (the oracle produces `{badkey,Key}` from its
    globally interned table).

## Reachability of the `NO_FAIL` raises from compiler-emitted code
Verified against the system `erlc` (OTP 29) disassembly:
- `put_map_exact` **missing key** is emitted with `{f,0}` = `NO_FAIL` (and a preceding
  `test,is_map` guard). It is **reachable**: `up(#{}) -> M#{a := 1, b := 2}` on `#{}` ⇒ the
  oracle raises `error:{badkey,b}`. This is the `badkey` path (`write.rs:141`).
- A **non-map** source with `NO_FAIL` is **not** reachable: the compiler always emits
  `test,is_map Fail` first, and its `Fail` path builds `{badmap,X}` and calls
  `erlang:error/1` itself — the `badmap` arms (`read.rs:65,93`, `write.rs:84,135`) are reached
  only from hand-built IR.
- `get_map_elements`/`has_map_fields` are always emitted with the clause/case fail label (the
  matching forms), so their key-absent `NO_FAIL` branch (the engine-defined bare
  `function_clause`) is likewise only reachable from hand-built IR.
- Because `function_clause` is a common atom, a real `.beam` for a matching function usually
  names it — but nameability is still a host load-time obligation (see above): the engine never
  substitutes a foreign index.

## Constraints
- `#![deny(unsafe_code)]` (crate-wide): this node carries no `unsafe`.
- Errors are `beam_common::{Result, BeamError}` only — never a panic, never a wrong value;
  every gap is a typed result.
- `live` (the C collector's GC-root count) is accepted and unused: `MinimalContext` has no
  moving collector.
- Atom-keyed flat-map storage order follows the *module-local* indices, so it can differ from
  the oracle's global-index order — visible only in a `term_to_binary` byte comparison of an
  atom-keyed map, never in map semantics (`lookup`/`position` compare by name via `cmp`).

## Test Strategy
- `tests.rs` unit suite: hand-built `Instr`s + a recording context. The raise cases intern the tag
  explicitly and compare the landed reason: the `{badmap,Source}` cases
  (`Fixture::new(&["a","badmap","error"])` at `:357`, `:685`), the `{badkey,Key}` cases
  (`Fixture::new(&["a","b","badkey","error"])` at `:933`, `:959`), and the absent-key `NO_FAIL`
  bare-`function_clause` cases (`Fixture::new(&["a","b","function_clause","error"])`) for **both**
  read instructions — the **caught** path asserts the handler's `x(1)` equals `make_atom(function_clause)`
  (`:382`, `:539`); the **uncaught** path recovers the reason term from `Raise::live_reason`
  (`uncaught_raise`/`live_reason_term`, `:170`/`:178`) and asserts class `error` (`:409`, `:565`).
- The **missing-atom** typed gap is asserted here too: a module that does not name `function_clause`
  yields the exact `unsupported_opcode: maps: no 'function_clause' atom in the module`
  (`assert_unsupported_reason`, `:188`; tests `:438`, `:594`).
- `../../../tests/gaps.rs` pins the remaining missing-atom gaps (`put_map_assoc/no_fail`,
  `put_map_exact/no_fail`, and the read instructions' `function_clause` nameability gap) with a
  hand-built `Module` + `MinimalContext` (which cannot name the tag). *(Parent-owned; see the
  handoff note below.)*
- `../../../tests/maps.rs` runs the real oracle fixture `../tests/fixtures/mapfx.beam` **both
  ways**, so the reason term — never just `BeamError::Raise(_)` — is asserted:
  `exact_with_a_missing_key_raises_badkey` loads it through the **seeded** host
  (`support::ModuleHost` + `SEED_ATOMS`) and asserts the raise's live reason term
  (`Raise::live_reason`) is the locally built `{badkey,a}` tuple;
  `exact_with_a_missing_key_on_an_unseeded_module_is_the_typed_gap` loads it with the private
  `beam_loader::load_file` table and asserts the exact typed
  `unsupported_opcode: maps: no 'badkey' atom in the module` (and no reason term). Neither can
  pass on the other's error.
- No differential-gate coverage of the compiled instruction path: the `errors/badmap_reason`,
  `errors/badmap_list_value`, `errors/badkey_reason`, `errors/badkey_compound_key` cases are all
  `CaseKind::Eval` (the `maps:get` BIF path), not a compiled `put_map_*`.

## Known Issues
- **Nameability depends on the host's `atoms` namespace** (see "The reason-atom contract" above). A
  host that overrides `ExecutionContext::atoms` with ONE live shared table (`beam_rt`) names every
  tag; a host that does not falls back to the module's load-time snapshot, so a module loaded with
  the private, unseeded `beam_loader::load_file` table gets the typed `unsupported` for the `badkey`
  case (the installed `erlc` interns `badmap` itself but not `badkey`) and for the read
  instructions' `function_clause` case when the module does not name it. The seeded hosts
  (`beam_rt::boot::CORE_ATOMS`, the test `SEED_ATOMS`) name all three, so the raise is the oracle's
  `{badkey,Key}`/`{badmap,Source}` (and the engine-defined bare `function_clause`) exactly.
- `lookup`/`position` are O(n) scans (no hash) and `put_map_assoc` is O(n²) in the pair count
  (`write.rs:105`); correct but unoptimised.
- `write.rs:165` (`put_map_exact: key vanished across the reserve`) is a defensive typed error
  for an impossible post-reserve state.

## Notes for Agents
- `../../../tests/gaps.rs:272`/`:300` pin the two read instructions' `no_fail` cases to
  `maps: no 'function_clause' atom in the module` on an unseeded host; on a seeding host the same
  shape surfaces the bare atom `function_clause` (class `error`).
- Do **not** substitute a foreign atom index: an index is meaningful only against the table it is
  read with, so a name the namespace cannot resolve must stay the typed error.
- `raise_error_tuple` installs the `error` class atom only when the namespace can name it
  (`mod.rs:297-299`), and `raise_function_clause_no_fail` does the same (`dispatch.rs`), so an
  uncaught raise still propagates as `Err(BeamError::Raise(..))`.
- The same by-name/typed-gap pattern is used by the exceptions family
  (`../exceptions/raise.rs:52-86`), so a change to this contract is a two-site change.

## See Also
- `../CONTEXT.md` → the family seam, the router sentinel, the full gap list.
- `../../../../beam_bifs/src/error.rs:130-156` → the BIF-side counterpart
  (`structured_error` can *intern* the tag through `AtomInterner`, degrading to bare `badarg`
  when the host has no interner).
- `../../../beam_rt/src/boot.rs:88-196` → the C `error_atom` vocabulary the host seeds.
