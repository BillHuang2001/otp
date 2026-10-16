# `beam_ets` — Integration Tests

## Intent
Crate-level integration tests that drive `beam_ets` through its **public API** only
(`use beam_ets::…`), plus the **differential ETS battery** against the system `erl`. These are the
crate's "evidence" layer; in-module unit tests live next to the code under `../src/`.
Read together with `../CONTEXT.md` (crate API + design) and `../ETS_SURFACE.md` (coverage census).

## Test Strategy
- One file per concern, each `mod common;` for shared helpers.
- Most tests assert **exact** values (sizes, counters, rendered tables) — not ordinals/probabilities.
- `differential.rs` is the A.1/A.8 compatibility evidence: the same ETS operation battery runs under
  the live `erl` and under `beam_ets`, compared as canonical text renderings.

## Oracle gate (important)
`common/oracle.rs` is the crate's **single** oracle gate and the one home for the shared
`erl`/`erlc`/`escript` plumbing every live cross-check goes through.
- **The oracle is REQUIRED by default**: `oracle::required()` is `true` unless the environment
  explicitly opts out, so a genuinely absent Erlang **FAILS** the calling test — with a message that
  names the check and the opt-out — instead of skipping it. A machine without `erl` can therefore no
  longer turn the differential evidence into a vacuous pass.
- **Opt-out (explicit):** `BEAM_ETS_ALLOW_NO_ORACLE=1` restores the loud skip (`[oracle] … SKIPPED …`
  on stderr, test reported as passing) for an environment that genuinely has no Erlang.
  `BEAM_ETS_REQUIRE_ORACLE=1` still **forces** the requirement and overrides the opt-out.
- `$ERTS_ORACLE` set to a non-executable path is a HARD error in every mode (a misconfig must not read
  as "no oracle").
- With `erl`/`escript` on `PATH` the cross-checks run; the `-p beam_ets` suite reports **0 skips**.
- `differential.rs` and `ets_continuation_direction.rs` keep **no local gate or plumbing**: both
  resolve their tools through `common::oracle` (`resolve_oracle`/`resolve_escript` built on
  `find_in_path`/`is_executable`, plus `run_bounded`/`run_escript` and `required`/`skip`).
  `ets_continuation.rs` holds only fixed-expectation pins (its live battery lives in the `_direction`
  sibling), so it has no gate to consolidate.

## Routing Table
- `ets_basic.rs` → insert/lookup/delete/delete_object/delete_all_objects/take/tab2list, insert_list atomicity, memory-bounded churn per kind/interior
- `ets_order.rs` → `ordered_set` key-order (incl. arithmetic `1 == 1.0` keys), `first`/`next`/`last`/`prev`
- `ets_nav.rs` → navigation on every kind (absent anchors, `'$end_of_table'` → `None`); plus the `take`+re-anchoring battery (`take_reanchoring_matches_the_recorded_oracle_figures` offline + `take_reanchoring_agrees_with_the_live_oracle` — after `take`, a hash kind's `next`/`prev` on the taken key is `badarg` while `ordered_set` reads it as a range bound)
- `ets_bag.rs` → `bag`/`duplicate_bag` multi-object semantics + `lookup_element_all`
- `ets_select.rs`, `select.rs`, `match_spec_run.rs` → match-spec compile/exec, `select*`/`match*`, `fun2ms`-shaped specs
- `ets_continuation.rs` → limited/continuation forms: fixed-expectation pins (its live-oracle battery is in the `_direction` sibling, so it needs no gate)
- `ets_continuation_direction.rs` → the direction/identity half of the continuation family + the shared live-oracle battery (gated through `common::oracle`)
- `ets_update.rs` → `update_counter`/`update_element` operands
- `ets_replace.rs` → `select_replace/2` + body-runtime-failure (`EXIT`) rule; oracle renderings quoted inline (offline-derived — pin tests, not live cross-checks) + `static_check_*` pinning the oracle's static body check (`db_match_keeps_key`): a rejected body is `badarg` and leaves the table unchanged; `repeated_identical_replacement_on_a_duplicate_bag_is_stable` pins 100 repeats of an identical `duplicate_bag` replacement against the recorded oracle line (its storage-side floor lives in `../src/table/tests.rs`, where the store's own accounting is reachable)
- `ets_info.rs` → `ets:info` 15-field proplist
- `ets_ownership.rs` → `give_away`/`give_away_with_gift`/`heir`/`setopts`/owner-death; inc. the identity/pid-gap pin `give_away_from_an_identityless_caller_follows_owner_identity` (a `None` caller is the owner of an ownerless table; a `None`/non-owner caller on an owned table is `badarg`), the gift obligation `give_away_carries_the_gift_to_the_new_owner`, and the live-oracle pin `give_away_transfer_observations_match_the_live_oracle` (a spawned recipient sees `{'ETS-TRANSFER', Tid, FromPid, Gift}` — atom `Tid` for a `named_table`, ref otherwise — and the table's memory does not grow)
- `ets_rename.rs` → `ets:rename/2`
- `ets_access.rs` → access matrix (`public`/`protected`/`private`) over the read/write surface
- `ets_options.rs` → `TableOptions` parsing/reporting
- `ets_fixtable.rs` → `safe_fixtable/2` deferred reclamation; plus the deterministic, always-on split-crossing walk tests (`a_split_crossing_ascending_walk_interleaved_with_mutations_visits_every_key_once`, `a_split_crossing_descending_walk_interleaved_with_mutations_visits_every_key_once`, `an_absent_key_anchor_is_a_range_bound_across_the_split_boundary`) — a `first`+`next` / `last`+`prev` chain over 100 keys (strictly > 2 × `ORDER = 32`) interleaved with mutation, sharing one walk driver
- `ets_reclamation.rs` → the index key-anchor invariant + memory-accounting stress (see `../CONTEXT.md` Known Issues / Design Decisions); plus `ets_info_memory_is_in_the_oracles_ballpark` (100k objects, `ets:info(T,memory)` same order of magnitude as the live oracle, ratio bounded by 10×, all 4 kinds)
- `ets_shared.rs` → `Send`/`Sync`, the `SharedTable` lock, no-lost-update concurrency, guard-scoped read aliasing note
- `ets_concurrency.rs` → lock independence, serial equivalence, no-lost-update check-then-act, live-oracle winner count
- `differential.rs` + `differential/` → the differential battery (embedded table + live oracle); `battery/` and `erl_program.rs`
- `ets_ordered_oracle.rs` → the **randomised** `ordered_set` differential trace vs the live oracle: a 73-entry mixed-type key pool (ints incl. `0`/negatives, the arith-collapsing `1`/`1.0` and `2`/`2.0`, floats, atoms, binaries, tuples, lists) rendered as **pool indices** on both sides, **60** op slots (34 insert + `insert_new`/`delete`/`delete_object`/`take`/`delete_all_objects`/`lookup`/`member`/`first`/`last`/`next`/`prev`/`select`/`select_count`/`match_object`/`select_reverse`/`foldl`/`update_counter`/`update_element`/`foldr`/`lookup_element/3`/`lookup_element/4` + a `next`/`prev` full-walk slot, an absent-anchor walk slot, and an `insert_block` key-boundary slot that forces a split), 3 seeds × 600 steps, compared line-by-line (`S=`/`T=` in interior order, never sorted/`R=`) after every step, peak 68 live keys; `#[ignore]`d, the escript writes its trace to a file (`argv[1]`) because `run_bounded`'s pipes would deadlock on a trace this size
- `common/mod.rs` → shared term/table helpers; `common/oracle.rs` → the crate's single oracle gate (required by default) + the live-`erl` plumbing (`find_in_path`/`is_executable`/`resolve_escript`/`run_escript`/`run_bounded`/`required`/`skip`)

## Notes for Agents
- The aliasing-hazard evidence tests (`a_stale_raw_term_reads_as_a_different_object_after_arena_churn`,
  `copy_out_survives_guard_drop_and_arena_churn`) live in `../src/table/view/tests.rs`, **not** here —
  `ets_shared.rs` only documents the aliasing constraint in its module docs.
- Concurrency and reclamation tests are coupled to the store's per-size free-list reuse (they force a
  freed run to be recycled); they fail loudly if that policy changes, they do not pass falsely.
- `differential.rs::rust_battery_matches_the_embedded_oracle_table` always runs (no oracle needed), so
  the erl-derived expectation table stays checked even on a machine without Erlang.
