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
- `expr/` — `mod.rs`: `Expr`/`CmpOp`/`TypeTest`/`ArithOp`/`Value`, the decoder
  (`decode`/`decode_tuple`/`decode_call`), the evaluator (`eval`/`eval_term`/
  `value_bool`), the shared `classify` probe and the `ArithHook` seam
  (`DefaultArithHook`, `set_arith_hook`, thread-local); `guard.rs`: the guard-BIF
  helpers (`element`/`size`/`tuple_size_of`/`bitstring_length`/`float_of`/
  `integral_of`/`ceil_floor`/`max_min`/`binary_part2,3`/`is_record3`/
  `is_function2`/`is_integer3`/`node0,1`/`xor`/`bnot`/`build_map`/`compare`/
  `type_test`/`bool_term`/`bool_of_atom`/`write_float_term`) and the `SelfHook`
  seam (`set_self_hook`, thread-local).
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
  `expr/{mod,guard}.rs`), each with a `// SAFETY:` rationale.

## Known Issues (honest gaps)
- **The guard-BIF surface is the whole C `guard_tab` class.** `expr/` decodes and
  evaluates the connectives (`andalso`/`and`/`orelse`/`or`/`not`), the comparisons,
  the type tests, the pre-existing guard BIFs (`element/2`, `abs/1`, `size/1`,
  `hd/1`, `tl/1`, `length/1`, `map_get/2`, `is_map_key/2`, `map_size/1`,
  `tuple_size/1`, `byte_size/1`, `bit_size/1`, `float/1`, `trunc/1`, `round/1`),
  the wider forms the C `guard_tab` also binds (`ceil/1`, `floor/1`, `max/2`,
  `min/2`, `binary_part/2,3`, `is_record/3`, `is_function/2`, `is_integer/3`,
  `node/0,1`, `xor/2`, `bnot/1`) and map **construction** in a guard or a body
  (`#{K => V}` — the C `dmc_map`, evaluated per object). Every case is pinned
  against the live `erl` in `tests/select.rs`/`tests/match_spec_run.rs` and the
  differential `matchspec` group. **Remaining typed gaps** (each a typed
  `unsupported_ets` raise, or a `badarg` the oracle also raises — never a wrong
  result):
  - `list_to_tuple/1` is **not** in the oracle's `guard_tab`, so the oracle and this
    crate both reject it with `badarg` (a matching behaviour, not a divergence);
  - `abs/1` and `bnot/1` on a **bignum** operand (`unsupported_ets: match-spec
    'abs'` / `match-spec 'bnot': a bignum operand`);
  - `trunc/1`/`round/1`/`ceil/1`/`floor/1` whose result is beyond the 60-bit small
    range — the oracle returns a bignum (`unsupported_ets: match-spec 'trunc'/'round'`);
  - `binary_part/2,3` on a source that is not byte-aligned is `badarg` (the oracle's
    own refusal, not a gap);
  - small-integer-only `DefaultArithHook` arithmetic (the guard-error
    classification below), and no `is_record/2`, comprehensions or
    `process_info`-style guards.
  `byte_size/1`/`bit_size/1` read an **off-heap** (refc) binary **correctly**: the
  on-heap `[BinRef][ErlSubBits]` pair's root classifies as `TermClass::Bitstring` and
  `bitstring_size` returns `end - start`, so it is oracle parity — the sibling
  `TermClass::BinRef` → `unsupported_ets` arm is reachable only for a *bare* `BinRef`
  term, which is not a user-visible term. (`tests/select.rs::
  byte_size_and_bit_size_read_an_off_heap_refc_binary_correctly`.)
- **A bare `{}` (0-arity tuple) as a guard/body expression PANICS.** `decode_tuple`
  (`src/select/expr/mod.rs:451`) reads `atom_name(elements[0], atoms)` after only
  the `elements.len() == 1` construct-form check, so an empty tuple indexes an
  empty slice → index-out-of-bounds panic. `decode` recurses over tuples/lists/maps,
  so any `{}` reachable as an expression subterm triggers it (a nested `{{{}}}` too;
  `{{}}` and `{const,{}}` do not). Every entry point that reaches
  `beam_ets::compile` is affected — `ets:select/2,3`, `select_delete`, `select_count`,
  `select_reverse/2,3`, `select_replace`, `match/m`, `match_object/…`,
  `ets:match_spec_compile/1`, `ets:match_spec_run/2`, `ets:match_spec_run_r/3` — and
  `erlang:match_spec_test/3` (its outer `structural_lint` does not descend into
  expressions, so the `{}` reaches `beam_ets::match_spec_run`). The oracle raises
  `badarg` (`ets:select(T,[{'$1',[],[{}]}])`, `ets:select(T,[{'$1',[{}],[ok]}])`,
  `ets:match_spec_compile([{'$1',[],[{}]}])`) and `erlang:match_spec_test(1,
  [{'$1',[],[{}]}],table)` returns `{ok,{error,[{error,"{} is neither a function
  call, nor a tuple (tuples are written {{ ... }})."}]}}`. No existing test covers a
  bare `{}`. Minimal C-faithful fix: reject an empty tuple in `decode_tuple`
  (`if elements.is_empty() { return Err(badarg()); }` before the `elements[0]` read).
- **A map in a HEAD matches structurally; its keys must be LITERAL.** `pattern.rs`
  decodes a head map to `Pattern::Map(Vec<(Eterm, Pattern)>)`: the keys stay
  literal — a variable (`'$N'`) or `'_'` key is `badarg`, at **every** nesting
  depth (the C `db_match_compile` "Variable found in map key."/"Underscore found
  in map key.") — while the values recurse like tuple elements, so `#{a => '_'}`
  matches `#{a => 1}` and an object carrying extra keys still matches (the C
  `matchMap` requires every literal key present and `map_size(object) >= n`). A
  map *expression* in a guard/body is **constructed** (`dmc_map`); only a head
  map is a pattern.
- **`self/0` resolves through the `SelfHook` seam; its default is the typed
  `badarg`.** `guard.rs` holds a thread-local `SelfHook` (`set_self_hook`) that a
  runtime knowing the calling process's identity installs; `beam_ets` never
  installs one (the ETS layer has no process identity), so the default answers the
  typed `badarg` — a `self/0` **guard** clause is then skipped (`guards_hold`
  treats the oracle's own `badarg` as a failed clause) and a `self/0` **body** is
  the typed `badarg`. The oracle evaluates it against the calling process, so
  without an installed hook this is an honest SEMANTIC under-match. Pinned by
  `tests/select.rs::documented_capability_gaps_are_typed_and_diverge_from_the_oracle`
  and `tests/match_spec_run.rs::documented_gaps_diverge_from_the_oracle_in_a_run`.
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
- **`select_replace/2` validates the body statically, up front** — the oracle's
  `db_match_keeps_key` (`erts/emulator/beam/erl_db_util.c:1422`): every clause's body must
  be a one-expression list of `'$_'`, the `{{ … }}` construction form or `{const, {…}}`,
  and the constructed key must be *syntactically* the head's key (or the
  `{element, KeyPos, '$_'}` form) — otherwise the whole call is `badarg` even when nothing
  matches. `exec.rs::static_keeps_key`/`clause_keeps_key` carry the check, with the
  `BodySide` walk mirroring the C `db_match_eq_body`. The rejected shapes are pinned by
  `tests/ets_replace.rs::static_check_*` (each asserting the unchanged table).
- **`select_replace` is then two-phase** (compute every replacement, then mutate) so a
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
- **Coverage:** the differential `matchspec` group pins the extended guard/body set
  directly against the live `erl` — `ceil`/`floor`, `max`/`min`, `binary_part/2,3`,
  `is_record/3`, `is_function/2`, `is_integer/3`, `node/0`, `xor/2`, `bnot/1` and the
  head-map literal-key rule — on top of the earlier `>`/`is_*`/`andalso`/`orelse`
  guards and small-int arithmetic-in-a-body; `select_guard_is_integer3` is green
  (both bounds of the `is_integer/3` range are inclusive).
  The crate-local `tests/select.rs` / `tests/match_spec_run.rs` pin the full
  guard-BIF set (map/tuple/bitstring/float/`trunc`/`round`/`abs` + map construction,
  each with its live-`erl` probe quoted) and the guard-error classification (an
  unevaluable guard is a typed error; an oracle-raising guard skips the clause).

## Routing Table
- `mod.rs` → `MatchSpec`/`MatchClause`/`compile`/`SelectPredicate` + term-decode helpers (`atom_name`, `var_index`, `tuple_elements`, `list_elements`)
- `pattern.rs` → `Pattern` + head decode/match (+ dead `bound_vars`)
- `expr/mod.rs` → `Expr`/`CmpOp`/`TypeTest`/`ArithOp`/`Value` + `ArithHook` + `decode`/`decode_tuple`/`decode_call`/`eval`/`eval_term`/`value_bool`/`classify`
- `expr/guard.rs` → the guard-BIF helpers (incl. `ceil_floor`/`max_min`/`binary_part2,3`/`is_record3`/`is_function2`/`is_integer3`/`node0,1`/`xor`/`bnot` + `map_get`/`map_size`/`is_map_key`/`tuple_size`/`byte_size`/`bit_size`/`float`/`trunc`/`round`/`abs` + map construction) + the `SelfHook`/`set_self_hook` seam
- `exec.rs` → `impl Table` select/match/delete/replace/count + `run_clauses`/`guards_hold`/`spec_for_pattern`/`BodyKind` + the static `select_replace` body check (`static_keeps_key`/`clause_keeps_key`, the `db_match_keeps_key` mirror)
- `run.rs` → `match_spec_run` (raw-spec list runner)
- `continuation.rs` → `MatchContinuation` + the limit/continue forms (`traverse`/`resume`)
- `../../../beam_bifs/src/{match_spec.rs,ets/{matches,reverse,specs,extra}.rs}` → the BIF bridges over this layer (they delegate to `compile`/`match_spec_run`/`Table::*`; no duplicated matcher)
