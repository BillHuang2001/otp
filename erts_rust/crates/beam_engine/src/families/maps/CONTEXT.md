# `beam_engine::families::maps` — the map instruction family

## Intent
The single owner of the four map `beam_loader::Instr` variants —
`GetMapElements`, `HasMapFields`, `PutMapAssoc`, `PutMapExact` — dispatched from
`../mod.rs::dispatch` (variant-partitioned router). A read descends the map **once per key**
through `beam_term::map::map_get` (both frozen layouts: the flat map's bounded key scan, the HAMT's
hashed path walk) and never materializes a pair list;
a write **copies the path** through `beam_term::map::map_put_cow` when the source is a hash map
(> 32 keys) and **rebuilds** through `crate::terms::make_map` → `beam_term::map::write_map` when it
is a flat map (≤ 32 distinct keys).
Key equality is `beam_term::cmp` (`EQ` = exact `=:=`, so `1` and `1.0` are distinct keys). The
family borrows `module: &Module` for the atom table it compares keys with.

Ground truth (read-only): `../../../../erts/emulator/beam/emu/map_instrs.tab:49-173`,
`beam_common.c:2237-2268` (`erts_gc_update_map_exact` sets `BADKEY`), `emu/ops.tab:1364-1424`,
`beam/generators.tab:230-253`, and — for the read path's branchless failure — `emu/beam_emu.c:578-583`
(`i_func_info_IaaI`) + `beam_common.c:448-544` (`handle_error`, the `function_clause` reason).

## Layout
- `mod.rs` — `exec` (dispatch), `position` (`:233`, the flat write arm's `beam_term::cmp` scan),
  `goto` (`:240`), `fail_or_raise` (`:249`), `fail_or_function_clause` (`:271`),
  `raise_error_tuple` (`:292`).
- `read.rs` — the shared read core `HeldSource` (`:115`), `load` (`:131`), `key_at` (`:167`),
  `descend` (`:187`), then `get_map_elements` (`:200`) and `has_map_fields` (`:229`); the read model
  is the section below.
- `write.rs` — `key_count` (`:129`, the O(1) size word), `takes_cow_arm` (`:142`, the layout branch),
  `merge_slack_words` (`:149`), `unchanged_after_update` (`:181`, the flat arm's no-change pre-walk),
  `cow_update` (`:227`, the shared COW reservation + chained `region.put`), `put_map_assoc` (`:270`),
  `put_map_exact` (`:344`); next section for the two arms and the no-change answers.
- `tests.rs` — the unit suite (both layouts incl. the 32/33-key boundary, all failure paths, the
  size-word equivalence, the large-map reference-builder equivalence, the no-change identity rows, and
  the `path_copying` module: the rebuild-equivalence sweep over `n ∈ {0,1,31,32,33,34,250,2_500}`, the
  exact-key fail/raise no-write rows, the HAMT multi-pair no-change rows, the size-independent
  words-reserved pin and the `term_to_binary` byte row), plus its `read_path` submodule
  (`tests_read.rs`: the read instructions' size sweep — see Test Strategy).

## The no-change fast path (term identity)
An update that changes nothing answers the **source map's own `Eterm` word**, so
`erts_debug:same(M#{a => 1}, M)` is `true` when `M` already holds `a => 1` — on a flat map and on a
hashmap (>32 keys) alike.
Ground truth: `erts_gc_update_map_assoc`'s `changed_keys`/`changed_values` fast path
(`erts/emulator/beam/beam_common.c:2094-2142`), `erts_gc_update_map_exact`'s (`:2309,2325-2326`) and
`erts_hashmap_insert`'s same-value answer (`erl_map.c:2493,2523-2526`).
- **HAMT source (> 32 keys): the decision is `map_put_cow`'s own zero-write arm.** `map_put_cow`
  answers the **input map word** when the stored value word already equals `value`, and that insert
  path allocates nothing — so the HAMT arm runs no pre-walk at all, it starts the one reservation and
  writes nothing.
  Cost of a no-change update on a HAMT: one reservation and zero writes, so the identity answer is
  the writer's own and the words-reserved pin (`tests.rs`) is its figure.
- **Flat source (≤ 32 keys): the decision is `write.rs::unchanged_after_update`**, kept because the
  flat arm still rebuilds (it has no path-copying writer to defer to, and the pre-walk is what keeps
  `put_map_assoc_no_change_allocates_nothing` green — no reservation happens on that path).
  Every pair's key must already exist (looked up with `beam_term::map::map_get` under the live atom
  namespace, i.e. the map's own key rule, so `1` and `1.0` stay distinct) **and** its value must equal
  the stored value by **raw `Eterm` word** equality, never by `=:=` (the C compares raw words).
- Both decisions are pure reads that allocate nothing — no materialized operand, not even a pair-list
  `Vec` — which is what makes writing the source word to `dst` sound under a collecting host: no
  collection can run between the read and the store, so the word cannot have moved.
- Residual (flat arm only): an operand whose read would materialize it (a non-small
  `Operand::Integer`/`Character`, i.e. a bignum) declines the pre-walk up front, so such an update
  rebuilds the map — correct contents, identity lost. The HAMT arm has no such residual: its answer is
  the writer's.
- `put_map_exact` reaches its no-change answer **after** the `{badkey,Key}` validation: a missing key is
  never a no-op, so it takes the failure path first; a present key set then falls through to the same
  per-layout answer (the COW writer's zero-write arm on a HAMT, the pre-walk then a rebuild on a flat
  map).
- Pins (`tests.rs`): `put_map_assoc_no_change_returns_the_source_word`,
  `put_map_assoc_no_change_allocates_nothing` (the arena's `live_words` is unchanged — an invariant,
  not a magic count), `put_map_assoc_no_change_on_a_hash_map_returns_the_source_word` (a 40-key
  HAMT), `put_map_assoc_multi_pair_no_op_is_the_source_word` (every pair a no-op ⇒ identity) and its
  `…_with_one_change_is_not_the_source_word` twin, plus
  `put_map_exact_no_change_{returns_the_source_word,on_a_hash_map_returns_the_source_word}` and
  `put_map_exact_missing_key_without_fail_raises_badkey`.
- Size- and key-complete pins: `a_no_op_update_answers_the_source_word_at_every_flat_size` sweeps every
  flat size `3..=8` (single-pair and whole-map no-op, plus a changing row that must build a fresh map of
  the same size), and `a_no_op_update_answers_the_source_word_at_every_hash_map_key` does the same for a
  40-key HAMT at keys `0`, `7`, `20` and `39` (all published, one per node).
- The rule is reached through **compiled code** too, not only through a hand-built instruction:
  `tests/maps.rs`'s `mapfx` module covers `assoc/1` (`#{K => V}`), `exact/1` (`#{K := V}`) and
  `assoc_exact/1` on both tiers (each asserts the two tiers answer the same word), plus a 40-key HAMT and
  a changing update whose answer must differ from the source while both tiers stay `is_eq`.
- The `path_copying` module adds the HAMT multi-pair no-op/one-change rows for **both** instructions at
  sizes up to 2 500.
after** it, i.e. after the instruction's last allocation, so neither the keys nor the map
     word can move afterwards.
  Two invariants follow on both paths: no descent reads a word a destination write of this instruction
  clobbered, and no held raw `Eterm` survives a possibly-collecting read unrelocated.
  The `&AtomTable` still comes from `ctx.atoms(module)` per descent (it borrows `*ctx` immutably, so it
  cannot be held across `read_operand`'s `&mut`).
  Detail: [`read`](read.rs)'s module docs.
- **The error → outcome table (per key).**
  An absent key (`map_get` → `Ok(None)`) takes the instruction's **fail** outcome
  (`fail_or_function_clause`: the fail label, or the engine-defined bare `function_clause` under
  `NO_FAIL`); a non-map source takes `fail_or_raise(.., "badmap", source)` at the entry check, before
  any write; and a `map_get` `TermError::BadMap` on a word that already passed `predicates::is_map` is a
  malformed map **interior** (an unknown HAMT node header) and stays the typed `BeamError::BadArg` —
  the error a malformed map surfaces. "This operand is not a map" never reaches that error: it is
  decided once, on the very word every descent consumes
  (`read.rs::descend`).
- Cost: one `map_get` descent per key and **nothing materialized**; the allocating-key path additionally
  reserves one bignum's words per such key, in the single reservation above.
  Any `terms::map_entries`-based path that remains in the family is on the **flat write** arm only
  (`write.rs::unchanged_after_update`'s pre-walk, ≤ 32 keys) and in `tests`, never on a read.

## Failure paths
| Site | Condition | `fail != NO_FAIL` | `fail == NO_FAIL` |
|---|---|---|---|
| `read.rs:211,240` / `write.rs:284,358` | non-map `source` | jump | raise `{badmap,Source}` |
| `write.rs:385` (COW) / `:414` (flat) | `put_map_exact` key absent | jump | raise `{badkey,Key}` |
| `read.rs:220,245` | key absent | jump | raise the **bare atom** `function_clause` |

`fail_or_raise` (`mod.rs:249`) forwards the `{Tag,Value}` raise to `raise_error_tuple`
(`mod.rs:292`); `fail_or_function_clause` (`mod.rs:271`) forwards the absent-key `NO_FAIL` branch to
the engine's single failed-clause definition `raise_function_clause_no_fail`
(`dispatch.rs`; call site `mod.rs:279`, `what = "maps"`), which throws class `error`
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
    seeds it before any load, and `CodeServer::load_bytes` forks the shared table into each module (`beam_rt/src/codeserver.rs:927` — an index-preserving shallow copy, not a deep clone) —
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
  oracle raises `error:{badkey,b}`. This is the `badkey` path (`write.rs:164`).
- A **non-map** source with `NO_FAIL` is **not** reachable: the compiler always emits
  `test,is_map Fail` first, and its `Fail` path builds `{badmap,X}` and calls
  `erlang:error/1` itself — the `badmap` arms (`read.rs:65,93`, `write.rs:102,155`) are reached
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
  atom-keyed map, never in map semantics (`map_get`/`position` compare keys by name via `cmp`).
  A HAMT result has a third order again (its node layout), so a byte comparison of a > 32-key map is
  only meaningful between two builds of the **same** arm; canonical `map_pairs` order is the
  layout-independent invariant.
- `crates/beam_engine/Cargo.toml` carries a **dev-only** `beam_codec` edge (wave 1 → wave 4,
  downward, no build-time cycle) for the `term_to_binary` byte row in `tests.rs`'s `path_copying`.

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
- The same file's hash-map-cap source rows (`get_reads_a_hashmap_source`,
  `get_with_a_missing_key_in_a_hashmap_takes_the_fail_label`,
  `has_reports_presence_in_a_hashmap`) build their source with the frozen writer
  (`beam_term::map::write_map` via `terms::make_map` plus small-integer filler keys, so the key count
  is past `MAP_SMALL_MAP_LIMIT`), because only the real builder places a leaf in the slot its own
  key's hash selects -- the locality `map_get` follows; the file's hand-written `Hamt` node blocks
  serve the **write** path only, which enumerates the source or rebuilds it flat.
- `tests.rs`'s `read_path` submodule (`tests_read.rs`) is the read side's size sweep, over sources
  built with `terms::make_map` so each takes the frozen layout for its size
  (`n ∈ {0,1,31,32,33,34,250,2_500}`): **every** present key fetched into its own `x(n)` destination
  (two-key and three-key rows, so the destination count is unrelated to the map's size),
  `has_map_fields` leaving its sentinel registers untouched, a 32-key atom-keyed flat source read by
  both instructions, an absent key taking the `fail` label on both layouts (the earlier present key's
  write staying harmless, the absent key's destination never written), the same absent key under
  `NO_FAIL` raising the bare `function_clause` at every size on both layouts, that raise's typed-gap
  residual on a hash map whose module does not name the atom, a non-map source's `fail` label and
  `{badmap,Source}` for both instructions, and a `1.0` key not answering an integer `1` read.
- `tests_read.rs`'s **aliasing rows** pin the instruction's atomicity: `an_aliasing_destination_reads_the_original_map`
  sweeps `n ∈ {0,1,31,32,33,34,250,2_500}` and puts the aliasing pair (`Dst == Src`) in the first, the
  second and a middle position, asserting every destination holds the value of the map read at entry;
  `an_absent_key_after_an_aliasing_write_takes_the_fail_label` and
  `an_absent_key_mid_way_leaves_the_source_word_unchanged` hold the source in a register the fail body
  does not write (`x(5)`) and assert both the `fail` jump and that the source word is unchanged.
- `../../../tests/maps.rs` runs the two rows of the committed fixture `tests/fixtures/mapalias.beam`
  (`mapalias.erl`, assembled from `khepri_tree:gather_node_props/2`): `values/1` is exactly the
  `erlc`-emitted shape the bug needed — `get_map_elements` whose **first** pair's destination is the
  source register (`[{atom,payload_version},{x,1},{atom,child_list_version},{x,2}]` with source
  `{x,1}`) — and `a_destination_that_aliases_the_source_reads_the_original_map` asserts the fetched
  tuple; `missing/1` (`[{atom,no_such_key},{x,1}]`) pins the absent-key path through
  `an_aliasing_read_of_an_absent_key_takes_the_clause_path` (the seeded `ModuleHost`, live reason
  `cmp`-equal to `function_clause`, the raise's head frame `missing`).
- `../../../tests/map_assoc_alloc.rs` keeps a per-thread-armed counting `#[global_allocator]` and the
  `harness + build + one walk` band over one `put_map_assoc` (1 000 and 2 775 source pairs), measured
  in the same binary, as an **upper-bound regression guard on the rebuild shape**: `build` is the
  reference map the semantic pin compares against and `walk` is one `terms::map_entries` enumeration,
  so an update that went back to enumerate-and-rebuild (or to the old `map_entries(map)?.len()` double
  walk) still blows through the band while the hash-map arm sits far below it — it enumerates nothing,
  and its arena reservation is not an allocator event at all.
  The produced map is compared with the reference builder in canonical
  `beam_term::map::map_pairs` order (`=:=` pair by pair), because a COW result's storage order is not
  the rebuild's.
  The same file carries the **read** pin (`the_map_read_instructions_do_not_allocate_a_pair_list`):
  `get_map_elements` and `has_map_fields` (two keys) over a hash-map source at 250 and 6 250 keys make
  the **same** allocation count at both sizes (9 events, all of them the run harness) and stay under a
  small constant, so a read that went back to a pair list fails there as well as on the shape; the one
  `terms::map_entries` walk of the same source printed beside it as evidence grows with `n` (12 → 18
  events), which is the figure a pair-list read would have added.
- `tests.rs`'s `path_copying` module is the rebuild-equivalence gate for the COW arm: for
  `put_map_assoc` and `put_map_exact` over `n ∈ {0,1,31,32,33,34,250,2_500}` (flat ≤ 32, HAMT ≥ 33)
  with a replaced key, a replaced key plus a new one, and a new key, it asserts the instruction's
  result is `=:=` the `terms::make_map` rebuild of the same merge, the same `map_size`, and the same
  canonical `map_pairs` sequence; plus the exact-key fail/raise no-write rows, the HAMT multi-pair
  no-change rows, the size-independent words-reserved pin and the `term_to_binary` byte row.
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
- **Cost model per write (n = source keys, k = pairs), per arm.**
  - **HAMT source (> 32 keys):** one rooted reservation of `map_put_cow_words_capacity(n)·k + slack`
    arena words (293·k for any `n > 32`), then k puts, each descending the key's hash path and copying
    only the nodes on it — O(k·log n) new nodes, **no** pair list, **no** sort, **no** whole-map block.
    The source is not enumerated at all.
  - **Flat source (≤ 32 keys):** the rebuild — the pre-walk (a `map_get` per pair, materializing
    nothing), then one `map_entries` walk into a pair `Vec`, k `position` scans (the old O(n·k) shape)
    and `write_map`'s sort + build: at least two pair `Vec`s (`terms/map.rs` + `beam_term/src/map.rs`)
    and a new map block of `map_words_capacity(n+k)` words.
- **A multi-pair HAMT update over-reserves.** The per-put capacity is a size-only bound, so `k` pairs
  reserve `293·k` words even though the written node count is O(k·log n); `k` is 1 for the compiled
  single-key forms (`M#{K => V}` / `M#{K := V}`), so the waste is only reachable from hand-built IR.
  A HAMT result's internal node order is also not the rebuild's (content, `=:=` and canonical
  `map_pairs` order are identical).
- **No known-costly path remains on the read side.** Both read instructions descend once per key
  with `beam_term::map::map_get` (the HAMT's hashed walk, the flat map's bounded scan of ≤ 32 keys),
  allocate nothing, and never build a pair list — the retired `map_entries` + linear-scan shape is
  gone (`tests/map_assoc_alloc.rs::the_map_read_instructions_do_not_allocate_a_pair_list` pins the
  flat allocation count across a 25× size range). A flat map's `map_get` is still a bounded linear
  scan (at most `MAP_SMALL_MAP_LIMIT` = 32 `cmp` comparisons), which is the frozen flat-map read rule,
  not a cost that can grow.
  `position` (`mod.rs:225`) survives only for the two **merge** closures (`write.rs:326`, `:433`);
  `put_map_exact` validates each key with a hashed `map_get` descent — on both arms — and then
  re-checks presence inside the COW closure, so no arm takes a full source walk to validate.
- The baseline tier has **no inline arm** for any map instruction (`jit/baseline/emit/block.rs:1376`
  → `emit_family` → `family.rs:57` `jit_family` → `families::dispatch`), so each one costs a helper
  call plus a second variant match on top of the family body.
- `write.rs` (`put_map_exact: key vanished across the reserve`) is a defensive typed error for an
  impossible post-reserve state, raised in both arms.

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

## Map `is_eq_exact`: the O(k log k) / two-`Vec` cost (from `../../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md` (which keeps the contract and a pointer).
- **A map `is_eq_exact` costs O(k log k) and two heap `Vec`s per comparison.** `eval_test`'s `IsEqExact` is a plain `cmp(a, b, ctx.atoms(module))` (`src/interpreter/test.rs:69-72`) with no map fast path, and `beam_term::compare::cmp_maps` collects and re-sorts both maps' pairs (`beam_term/src/compare.rs:323-338` -> `map.rs:482-489` -> `collect_pairs`, which allocates a `Vec<(Eterm,Eterm)>` per map).

## See Also
- `../CONTEXT.md` → the family seam, the router sentinel, the full gap list.
- `../../../../beam_bifs/src/error.rs:130-156` → the BIF-side counterpart
  (`structured_error` can *intern* the tag through `AtomInterner`, degrading to bare `badarg`
  when the host has no interner).
- `../../../beam_rt/src/boot.rs:88-196` → the C `error_atom` vocabulary the host seeds.
