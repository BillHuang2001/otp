# `baseline/testing` — the native baseline tier's test enumeration

## Intent
The enumeration of what the baseline tier's own suites pin: the crate-internal tests under
`src/jit/baseline/` (all `#[cfg(test)]`, most as `#[path]` submodules of `../tests.rs` so they share its
fixtures and helpers) plus the one submodule of `../emit/test.rs`.
This node owns the *enumeration* only.
The instruments' tables and gate commands are `../census/CONTEXT.md`; every measured figure is `../measure/CONTEXT.md`; the tier's contract and inline set are `../CONTEXT.md` and `../emit/CONTEXT.md`.

## Cross-module tier equivalence is NOT here
Tier equivalence on real modules lives in the crate's `tests/jit*.rs` (see `../../../CONTEXT.md`), which drive the **lazy** path through `Machine::run_with_tier`.
These suites are the in-node pins: the block ABI/layout, the per-lowering agreement, and each lever's structural claim.

## `../tests.rs` (the ABI, layout and per-lowering pins)
Unit-tests the ABI encoding, the `#[repr(C)]` runtime layout, regalloc, the fast-path constants/arithmetic, the deopt/yield folding (a deopted `wait` folds to `RunOutcome::Wait`), and that a collecting `test_heap` reaches the host's admission door exactly once and reloads the cached registers afterwards.
- The **heap / frame-admission** family's own pins: tier agreement for `allocate` (both `HeapNeed` encodings) / `allocate_heap` / `test_heap` / `trim`; an over-trim raising the interpreter's own typed diagnostic on both tiers; the exactly-once admission (with the instruction's own `heap_need`/`live`) for the two heap-admitting members on each tier; and the structural claim that none of the four records a static or a run-time deopt.
- It also pins that the comparison-`test` lowering leaves no `Test` entry in the two-process copy-bound probe's per-opcode `jit_step_one` table.
- `the_y_window_traffic_of_the_committed_fixtures_is_measured` first pins the Y-traffic instrument on a synthetic shape with known traffic (2 loads / 2 stores / 2 same-block slot reuses), then counts the committed `erlc` fixtures: **17 guarded Y accesses over 224 013 emitted bytes**, **14** `Y`-touching blocks (**6** of them publishing the window in their prologue, so **8** dead publishes elided), **19** invalidation sites and **1** same-block slot reuse — the measured bound on what a per-block single-slot Y-value cache could save.
- `the_two_process_copy_bound_shape_is_measured_on_both_tiers` and `the_entry_module_priming_is_measured_on_both_tiers` are the measurement probes whose figures are in `../measure/CONTEXT.md` (the latter asserts the `module_fingerprint` counts only).

## `../call_tests.rs` (the frame-preserving local `Call`)
`the_frame_preserving_local_call_agrees_with_the_interpreter` (a two-argument and a nested chain), `the_frame_preserving_local_call_records_no_static_deopt`, `the_lowered_call_still_gives_its_continuation_a_native_sub_block`, an undefined-label deopt, no register touch/write, and `the_call_store_sentinel_is_disjoint_from_every_other_block_word`.
- `the_eager_sub_block_reads_the_call_result_out_of_machine_x` is the **eager-layout** pin (helpers `call_result_passthrough_case`/`eager_sub_block_context`): on the `f(A,B) -> g(B)` shape — a mid-block frame-preserving `call` followed by two **unguarded** inlined `move`s that carry the result across the continuation — it compiles the **eager** artifact through `JitBackend::{compile, run_compiled}` on `BaselineJit` (the tier entries go through `cache::run_baseline_cached*`, i.e. the **lazy** artifact, so they cannot reach the eager sub-block layout at all) and asserts the result equals both the interpreter's `Machine::run_export` and the hand-computed value; the callee is cache-free (a non-lowered `bif`) so no callee register can mask a stale one.
  It first pins the premise that the containing IR block's allocation and the sub-range allocation differ — `containing.slot(0).is_some() && sub_range.slot(0).is_some()`, `!containing.entry_loads(0)`, `sub_range.entry_loads(0)` — so a re-collapse of the eager pass onto one allocation fails on the shape itself, not only on the value.
- The in-block **transfer** is pinned by `a_lazy_cross_function_call_is_transferred_in_block` (transfer taken, no decline, fewer round-trips), `deep_cross_function_non_tail_recursion_does_not_grow_the_native_stack` (20 000-deep mutual non-tail recursion, both tiers return the same value — the tail transfer's reason), `a_refused_preemption_charge_parks_at_the_callee_entry_and_resumes_exactly_once` (parked at the **callee** entry, resume yields the value, `reduce` hooks fire `(1,1)`), `a_served_local_call_is_never_transferred` (`in_block_calls == 0`, the BIF value delivered) and `an_uncompilable_callee_declines_to_the_driver` (a function with no IR blocks → `in_block_call_declines > 0`, the driver's word path).

## `../family_tests.rs` (the family-owned arm)
`the_family_owned_constructors_agree_across_the_tiers`, a family raise identical on both tiers, `the_family_owned_constructors_are_lowered_without_a_jit_step_one_deopt`, and a family-built term reaching a later native read through the cache refill.

## `../tail_exit_tests.rs` (the unlinked tail-call path)
`emit/link.rs::emit_tail_exit_to_driver`: forward/backward/self tail calls, a raise inside the tail callee, a frame-size mismatch, an undefined label keeping the interpreter's error, and `the_unlinked_tail_exit_records_no_call_last_deopt`.
Also `the_probed_local_call_records_one_spill_call_and_none_at_the_exit` and `the_probed_tail_call_still_agrees_with_the_interpreter`.

## `../y_operand_tests.rs` (the Y-operand shapes)
The Y-operand `test`/`select_val`/`select_tuple_arity` lowerings, asserting tier agreement and the `Loc` gate (`the_y_gate_is_true_for_a_y_source_and_false_for_an_x_source`).

## `../raise_tier_tests.rs` (the frame-discipline pin)
`a_raise_at_its_own_pc_plus_one_reads_the_enclosing_frames_y_slot`: `allocate 2` (frame A) → `y(0) := 11` → `try` → `allocate 1` (frame B) → `case_end` raising → the handler at the raise's **own `pc + 1`** → `try_case` → `y(0) → x(0)`.
Asserts this tier returns `11` (not the destroyed frame's word), `step_one == 0` (nothing is masked by an interpreter step), and interpreter/baseline agreement.
`fold_step` encodes `Step::Next` and `Step::Goto { pc: pc + 1 }` identically, so the raising block links straight into the handler block with **no** driver iteration between the unwind and the handler; the read is correct only because both emitted window derivations are unconditional (see `../measure/CONTEXT.md` and `../emit/prologue/CONTEXT.md`).

## `../operand_tests.rs`
`the_move_deopt_breakdown_over_the_corpus_is_printed`, the move-shape deopt census.

## `../bif_tests.rs` (the named-BIF fast paths)
Its own `bif_host()` supplies the interpreter tier's BIF semantics, since no BIF is engine-native.
Per lowered accessor a tier-equivalence value test plus its deopt shapes:
- `hd_is_lowered_to_the_cons_read` / `element_is_lowered_to_the_tuple_read` / `tuple_size_is_lowered_to_the_tuple_header_read`, with `the_declined_bif_shapes_raise_the_interpreters_badarg` (non-cons, non-tuple, `element(0, T)`/`element(9, {a})`, non-immediate index);
- `the_bitwise_and_shift_gc_bifs_are_lowered_to_the_bifs_value`, `the_bitwise_fast_path_agrees_with_the_bif_over_a_small_grid`, `the_bsr_count_bounds_are_exact_on_both_tiers` (`0`/`64`/negative/register counts), `the_declined_bitwise_and_shift_shapes_are_the_interpreters_outcome` (bignum/float);
- `byte_size_is_lowered_to_the_bit_size_rounded_up` / `length_is_lowered_to_the_proper_list_walk` / `the_collecting_value_bifs_are_lowered_on_both_tiers`, with `the_declined_value_bif_shapes_raise_the_interpreters_badarg` and `the_declined_collecting_value_shapes_raise_the_interpreters_badarg` (non-bitstring, improper list, a bignum constant);
- the Y-operand shapes (`the_y_operand_shapes_are_lowered_and_agree_across_the_tiers`, `the_value_bif_y_operand_shapes_agree_across_the_tiers`, `the_bitwise_gc_bif_y_operand_shape_agrees_across_the_tiers`);
- the three structural pins `the_lowered_bif_fast_paths_record_no_static_deopt` / `the_lowered_value_bif_fast_paths_record_no_static_deopt` / `the_lowered_bitwise_gc_bifs_record_no_static_deopt`.

## `../emit/test/refusal_census.rs`
`the_refusal_attribution_census_is_printed` — see `../census/CONTEXT.md` for the instrument itself and its self-maintaining pools.

## `../emit/spill_tests.rs` (the call-site spill discipline)
A write-then-helper stores the dirty register once, an inlined write-nothing instruction does not reset the set, a fully-deopted one does reset it, an empty `spill_sets` table falls back to the whole `written` set, and a corpus consistency pin checks `regalloc.rs::is_inlined`.
`one_run_yields_both_the_written_and_the_full_cache_spill_counts` (**4** written / **8** full-cache stores from a 4-slot cache with a 2-element write set at two sites).

## `../emit/link.rs` (the exit spill)
`a_done_exit_emits_exactly_the_spill_it_skips_less` — `Required`-vs-`Done` byte-length equality on all three exits.
`../tests.rs`'s `every_exit_spills_the_cached_registers_before_leaving_the_frame`, `a_tail_exit_spills_pops_the_frame_and_then_jumps_to_the_successor` and `a_deallocate_spills_before_calling_the_helper`.

## `../emit/compile.rs` (the continuation sub-blocks)
`the_continuation_sub_blocks_scale_linearly_not_quadratically` (both layouts) and `the_disjoint_sub_blocks_still_run_every_call_ext`.

## `../emit/heap.rs` (the frame-changing republish)
`a_frame_changing_helper_emits_no_separate_refresh_call`.

## `../census.rs` (the instrument pins)
`the_register_cache_traffic_census_over_the_corpus_is_printed` and `the_per_kind_emitted_code_size_table_is_printed`; their tables, corpus discovery and gate commands are `../census/CONTEXT.md`.

## `../emit/test/refusal_census.rs` and the static counters
See `../census/CONTEXT.md`.
`../counters.rs` is `#[cfg(test)]`-only, so a counter pin must compile cache-free through `emit::compile_function` (as `../call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt` does).

## Constraints
- Every suite here is `#[cfg(test)]` and in-node; none may change a production result.
- Tests needing the system toolchain or `erl` skip loudly when it is absent, so a green run without it proves nothing about corpus parity.
- Run through the wrapper (`$HOME` is read-only in the sandbox): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib` for this node's suites.

## See Also
- `../CONTEXT.md` → the tier contract, the two layouts, the routing table.
- `../census/CONTEXT.md` → the deopt/execution/code-size instruments these suites print.
- `../measure/CONTEXT.md` → every figure produced here and the levers it chose.
- `../../../CONTEXT.md` → the crate-level test strategy and the `tests/jit*.rs` tier-equivalence gate.
