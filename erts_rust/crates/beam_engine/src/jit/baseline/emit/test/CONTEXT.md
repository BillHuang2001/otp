# `emit/test` — the `test` family's lowering and its refusal census

## Intent
The `test`/`select`/`select_tuple_arity` lowering group whose sources live here (`test.rs`, `test/header.rs`, `test/tests.rs`, `test/refusal_census.rs`): the word tests and the boxed-header tests, the comparison helper, and the test-only refusal attribution.
`select.rs` and `aggregate.rs::try_inline_select_tuple_arity` are the sibling switch lowerings.

## API Surface
- `../test.rs` — `try_inline_test` and `test_plan` (the shape plan), `emit_test_helper` (the comparison-helper arm: an `emit_helper_call` to `../test_helper.rs::jit_test`, then `cmp rax, TEST_TRUE / je success`, `cmp rax, TEST_FALSE / je failed`, else `jmp done`), `TEST_TRUE`/`TEST_FALSE`, and the `test` scratch set `TEST_A`=`rax`/`TEST_B`=`r8`/`TEST_C`=`r9`/`TEST_T`=`r10`/`TEST_H`=`r11`.
- `test/header.rs` — the boxed-term header tests.
- `test/tests.rs` — the group's unit tests.
- `test/refusal_census.rs` (`#[cfg(test)] mod refusal_census`) — `the_refusal_attribution_census_is_printed`: `../census/CONTEXT.md` owns the instrument, its self-maintaining pools and the gate command.

## Key facts
- **`TestPlan`** exposes `uses_y` (gates the block's Y publish/refresh), `emits_guard` (whether a run-time guard was emitted) and `needs_deopt` (whether the failure path is the interpreter step).
- A `TestSrc` is an in-bound x-register, an **addressable `Y` slot** (behind the frame-window guard), a small immediate or a raw immediate word.
- **`success`** refills, emits the gated `emit_refresh_y` when `refresh_y_after(pc)`, and falls through; **`failed`** is `linker.emit_exit` to the resolved fail label, so a compiled fail block is jumped to **directly** (its own frame teardown replaces this block's) and any other target returns its `(func, pc)` word to the driver.
- Any shape the helper does not own emits **zero** bytes, so the caller's fallback is exactly the per-instruction `jit_step_one`; the fast path **allocates nothing and writes no heap word**.
- `NO_FAIL` -> `None` (the interpreter's `function_clause` raise); an unresolved fail label -> `None`; an arity operand outside the interpreter's range, a module with neither `true` nor `false` interned for `is_boolean`, an out-of-bound register, or a bignum/float/literal operand -> `None`.
- A comparison `Test` the word/header subset declines is emitted through the helper for the six comparison kinds at two operands with a resolvable non-`NO_FAIL` fail label, **whatever the operands** (a `Y` slot, a float register, a literal-area term, boxed-vs-boxed) — a comparison must route through `beam_term`'s own `cmp`/`cmp_arith`, so a boxed pointer is never word-compared.

## See Also
- `../CONTEXT.md` → the emitter contract, the routing table.
- `../shapes/CONTEXT.md` → the restricted shape list this group is one entry of, and the three deopt levels.
- `../../runtime/CONTEXT.md` → the block-ABI words the helper answers with.
- `../../census/CONTEXT.md` → the refusal census this submodule prints.
- `../../testing/CONTEXT.md` → the suite enumeration.
