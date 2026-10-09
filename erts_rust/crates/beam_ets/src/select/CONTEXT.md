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
  `match_object`, `match_`, `match_delete`, `select_replace`; `BodyKind`; `run_clauses`,
  `guards_hold`.
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
- **The guard-BIF surface is implemented and oracle-checked.** `expr.rs::decode_call`/
  `eval` cover the C `guard_tab` (`erl_db_util.c:572-976`) set already present plus the
  additions `map_get/2`, `is_map_key/2`, `map_size/1`, `tuple_size/1`, `byte_size/1`,
  `bit_size/1`, `float/1`, `trunc/1`, `round/1`, `abs/1` (small int **and** float) and
  map **construction** in a guard or a body (`#{K => V}` — the C `dmc_map`, evaluated per
  object). Every case is pinned against the live `erl` in `tests/select.rs` and
  `tests/match_spec_run.rs`. **Remaining typed gaps** (each a typed `unsupported_ets`
  raise, or a `badarg` the oracle also raises — never a wrong result):
  - `list_to_tuple/1` is **not** in the oracle's `guard_tab`, so the oracle and this
    crate both reject it with `badarg` (a matching behaviour, not a divergence);
  - `abs/1` on a **bignum** operand (`unsupported_ets: match-spec 'abs'`);
  - `trunc/1`/`round/1` whose result is beyond the 60-bit small range — the oracle
    returns a bignum (`unsupported_ets: match-spec 'trunc'/'round'`);
  - `binary_part/3` and comprehensions are still absent (`badarg`).
  `byte_size/1`/`bit_size/1` read an **off-heap** (refc) binary **correctly**: the
  on-heap `[BinRef][ErlSubBits]` pair's root classifies as `TermClass::Bitstring` and
  `bitstring_size` returns `end - start`, so it is oracle parity — the sibling
  `TermClass::BinRef` → `unsupported_ets` arm is reachable only for a *bare* `BinRef`
  term, which is not a user-visible term. (`tests/select.rs::
  byte_size_and_bit_size_read_an_off_heap_refc_binary_correctly`.)
- **A map in a HEAD is compared as an exact literal**, so a wildcard/variable inside a
  head map **under-matches**; the oracle matches structurally (`#{a=>'_'}` matches
  `#{a=>1}`). A map *expression* in a guard/body **is** constructed (`dmc_map`); only a
  head map is literal.
- **`self/0` is an honest SEMANTIC under-match**, not a typed gap: the ETS layer has no
  process identity, so it raises `badarg` and its clause is skipped, while the oracle
  evaluates it against the calling process (`{self} =:= {self}` matches for the oracle).
  Pinned by `tests/select.rs::documented_capability_gaps_are_typed_and_diverge_from_the_oracle`
  and `tests/match_spec_run.rs::documented_gaps_diverge_from_the_oracle_in_a_run`.
- **`select_replace/2`'s key check is value-based**, not the oracle's static
  `db_match_keeps_key` shape check (`exec.rs` `keeps_key` + the doc block); more
  permissive in three corners, recorded by `tests/ets_replace.rs::recorded_divergence_*`.
- **Continuations are index positions into `tab2list()`, not live cursors**
  (`continuation.rs`); a table mutated between `/3` and `/1` may shift the position
  (bounds-checked, never OOB). Hash-kind order is unspecified.
- **Dead code:** `pattern::bound_vars` is `#[allow(dead_code)]` and has no caller
  (only self-recursion; no test). Its doc "handy for tests" is aspirational.

## Design Decisions
- **The guard-error classification (`exec.rs::guards_hold` returns `Result<bool>`).** A
  guard the **oracle itself raises** — every oracle-raising guard construct is the C
  `badarg` (`element/2` out of range, `hd`/`tl`/`length` of a non-list, `size/1` of a
  non-tuple/binary, `map_get/2` with an absent key, `div`/`rem` by zero, a non-number
  arithmetic operand, …) — makes its clause fail (`Err(BeamError::BadArg) => Ok(false)`,
  the C `matchCatch`); so does a guard whose value is non-boolean or `false`. Any
  **other** error is a capability gap of this runtime (e.g. `unsupported_ets` from
  `ArithHook` arithmetic outside small integers) and is propagated with `Err`, so the
  caller sees a typed failure instead of a silently under-matching spec (workspace
  `CONTEXT.md` §3).
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
- **Coverage:** the differential `matchspec` group still exercises only small-int
  arithmetic-in-a-body (`*`) and `>`/`is_*`/`andalso`/`orelse` guards, so `mismatch = 0`
  alone does **not** establish guard/body-language parity (`KNOWN_DIVERGENCES` is empty).
  The crate-local `tests/select.rs` / `tests/match_spec_run.rs` close that gap: they pin
  the full guard-BIF set (map/tuple/bitstring/float/`trunc`/`round`/`abs` + map
  construction, each with its live-`erl` probe quoted) and the guard-error
  classification (an unevaluable guard is a typed error; an oracle-raising guard skips
  the clause).

## Routing Table
- `mod.rs` → `MatchSpec`/`MatchClause`/`compile`/`SelectPredicate` + term-decode helpers (`atom_name`, `var_index`, `tuple_elements`, `list_elements`)
- `pattern.rs` → `Pattern` + head decode/match (+ dead `bound_vars`)
- `expr.rs` → `Expr`/`CmpOp`/`TypeTest`/`ArithOp`/`Value` + `ArithHook` + `decode`/`eval` (incl. `map_get`/`map_size`/`is_map_key`/`tuple_size`/`byte_size`/`bit_size`/`float`/`trunc`/`round`/`abs` + map construction)
- `exec.rs` → `impl Table` select/match/delete/replace/count + `run_clauses`/`guards_hold`/`spec_for_pattern`/`BodyKind`
- `run.rs` → `match_spec_run` (raw-spec list runner)
- `continuation.rs` → `MatchContinuation` + the limit/continue forms (`traverse`/`resume`)
- `../../../beam_bifs/src/{match_spec.rs,ets/{matches,reverse,specs,extra}.rs}` → the BIF bridges over this layer (they delegate to `compile`/`match_spec_run`/`Table::*`; no duplicated matcher)
