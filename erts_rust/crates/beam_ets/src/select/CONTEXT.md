# `beam_ets/src/select` — the match-spec compiler + executor

## Intent
The `ets:select`/`match` family's engine: decode a match-spec clause list into a
`MatchSpec` (`mod.rs::compile`), decode head patterns (`pattern.rs`) and guard/body
expressions (`expr.rs`), run them over table objects (`exec.rs`) or over a list
(`run.rs`), and expose the limit/continuation traversal (`continuation.rs`).
Mirrors the C `erl_db_util.c` (`dmc_expr`/`dmc_fun`/`guard_tab`) **semantics**, not
its compiled-opcode program. The interior is a Rust AST, not a term program.

## API Surface (consumed by `beam_bifs` and the crate's `Table`)
- `mod.rs` — `compile(spec, atoms) -> Result<MatchSpec>`; `MatchSpec { clauses }`,
  `MatchClause { head, guards, body }`; `SelectPredicate<'a>` (the `Table::select_all`
  primitive); legacy `pub type` aliases `MatchPattern`/`MatchElement`/`MatchGuard`/
  `MatchBody`/`MatchBodyElement` (declared for source compatibility; **no consumer**).
- `pattern.rs` — `Pattern` (`Wildcard`/`Bind(u32)`/`Literal(Eterm)`/`Tuple`/`Cons`),
  `decode`, `matches`. `'$_'`/`'$$'` in a **head** are ordinary literal atoms; a
  repeated `'$N'` enforces `eq`.
- `expr.rs` — `Expr`, `CmpOp`, `TypeTest`, `ArithOp`, `Value`; `decode`, `eval`,
  `value_bool`; the `ArithHook` seam (`DefaultArithHook`, `set_arith_hook`, thread-local).
- `exec.rs` — `impl Table`: `select`, `select_reverse`, `select_delete`, `select_count`,
  `match_object`, `match_`, `match_delete`, `select_replace`; `BodyKind`; `run_clauses`.
- `run.rs` — `match_spec_run(list, raw_spec, atoms, heap)` (the oracle's
  `match_spec_run/2` takes an already-compiled spec; this takes the **raw** list).
- `continuation.rs` — `MatchContinuation` (opaque) + the `/3`+`/1` limited/continue forms.

## Constraints
- Term order/hash come only from `beam_term::compare` (`cmp`/`cmp_arith`/`eq`) — never
  re-implemented here.
- Guard truth is an internal `Value::Bool`, so it never requires `true`/`false` interned.
  A term-valued body boolean (`true`/`false`) **does** need them interned; so does `EXIT`
  for the body-`FAIL_TERM` path.
- `unsafe` only in the term-word readers (`mod.rs` tuple/list accessors, `pattern.rs`,
  `expr.rs`), each with a `// SAFETY:` rationale.

## Known Issues (honest gaps)
- **A guard's typed capability-gap error is silently swallowed.** `exec.rs::guards_hold`
  maps **every** `Err(_)` to "clause fails". A guard whose evaluation hits an
  *unsupported* (not oracle-raising) construct — match-spec arithmetic outside small
  ints (`/`, bignum/float operands, i64 overflow → `unsupported_ets`) or `self/0`
  (→ `badarg`) — therefore makes the clause fail and the spec silently **under-matches**,
  while the oracle evaluates it and matches. The body path (`run_clauses`) correctly
  propagates non-`BadArg` errors; the guard path does not. No `ArithHook` is installed
  anywhere in the workspace (`set_arith_hook` appears only here + its tests), so the
  shipped runtime is permanently small-int-only.
- **Unsupported constructs fail as `badarg`, indistinguishable from a malformed spec.**
  `expr.rs::decode_call`'s fallthrough `_ => Err(badarg())` and the map-expression guard
  (`expr.rs` map branch) reject every call outside the recognised set with plain `badarg`
  — the same error a genuinely-invalid spec yields. Missing vs the oracle's `guard_tab`:
  `map_get`, `float`, `trunc`, `round`, `list_to_tuple`, `is_map_key`, `map_size`,
  `tuple_size`, `byte_size`/`bit_size`, `binary_part`, comprehensions, map construction
  and map patterns in a head (the head map is compared as an exact literal, so it
  **under-matches**). `abs/1` is small-int only; `self/0` raises. Inconsistent with the
  `ArithHook` path, which reports `unsupported_ets`.
- **Map patterns/expressions and `self/0` are the pinned typed divergences** (see
  `tests/select.rs::documented_capability_gaps_are_typed_and_diverge_from_the_oracle`,
  `tests/match_spec_run.rs::documented_gaps_diverge_from_the_oracle_in_a_run`).
- **`select_replace/2`'s key check is value-based**, not the oracle's static
  `db_match_keeps_key` shape check (`exec.rs` `keeps_key` + the doc block); more
  permissive in three corners, recorded by `tests/ets_replace.rs::recorded_divergence_*`.
- **Continuations are index positions into `tab2list()`, not live cursors**
  (`continuation.rs`); a table mutated between `/3` and `/1` may shift the position
  (bounds-checked, never OOB). Hash-kind order is unspecified.
- **Dead code:** `pattern::bound_vars` is `#[allow(dead_code)]` and has no caller
  (only self-recursion; no test). Its doc "handy for tests" is aspirational.

## Design Decisions
- **`ArithHook` is a thread-local seam** (`set_arith_hook`) so `beam_ets` (wave 1) needs
  no `beam_bifs` edge; `DefaultArithHook` is total-or-refusing (never a fabricated result:
  `/`, non-small operands, overflow → `unsupported_ets`; `div`/`rem` by 0 → `badarg`).
- **Continuations carry a cloned compiled `MatchSpec` + limit + direction + index**, so a
  `/1` resume never recompiles and keeps its direction (`select` vs `select_reverse`).
- **`select_replace` is two-phase** (compute+validate every replacement, then mutate) so a
  `badarg` leaves the table unchanged; `duplicate_bag` rebuilds affected keys from
  `take`'s checked-out copies.
- **Body failures ≠ guard failures:** a body expression's `BadArg` becomes the atom `EXIT`
  (C `FAIL_TERM`) and evaluation continues; a guard failure skips the clause (C
  `matchCatch`) — both match the oracle.

## Test Strategy
- Unit/module tests live with the sibling test files; the select layer's evidence is
  `../.../tests/select.rs`, `tests/match_spec_run.rs`, `tests/ets_select.rs`,
  `tests/ets_continuation.rs`, `tests/ets_replace.rs`, and the differential group
  `tests/differential/battery/{matchspec,replace,continuation}.rs` (compared line-by-line
  against the live `erl`).
- **Coverage caveat:** the differential `matchspec` group exercises only small-int
  arithmetic-in-a-body (`*`) and `>`/`is_*`/`andalso`/`orelse` guards — it never exercises
  `/`, floats, bignums, `self/0`, `abs/1`, `element/2`, maps or the unsupported-call set,
  so `mismatch = 0` does **not** establish guard/body-language parity (and
  `KNOWN_DIVERGENCES` is empty).

## Routing Table
- `mod.rs` → `MatchSpec`/`MatchClause`/`compile`/`SelectPredicate` + term-decode helpers (`atom_name`, `var_index`, `tuple_elements`, `list_elements`)
- `pattern.rs` → `Pattern` + head decode/match (+ dead `bound_vars`)
- `expr.rs` → `Expr`/`CmpOp`/`TypeTest`/`ArithOp`/`Value` + `ArithHook` + `decode`/`eval`
- `exec.rs` → `impl Table` select/match/delete/replace/count + `run_clauses`/`guards_hold`/`spec_for_pattern`/`BodyKind`
- `run.rs` → `match_spec_run` (raw-spec list runner)
- `continuation.rs` → `MatchContinuation` + the limit/continue forms (`traverse`/`resume`)
- `../../../beam_bifs/src/{match_spec.rs,ets/{matches,reverse,specs,extra}.rs}` → the BIF bridges over this layer (they delegate to `compile`/`match_spec_run`/`Table::*`; no duplicated matcher)
