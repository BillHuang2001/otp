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
`common/oracle.rs` is the shared `erl`/`escript` plumbing.
- **A genuinely absent oracle is a LOUD SKIP that COUNTS AS A PASS**: `oracle::skip()` prints
  `[oracle] … SKIPPED …` to stderr and returns, so `cargo test` reports the test as *passing*.
- `BEAM_ETS_REQUIRE_ORACLE=1` turns that skip into a failure (`oracle::required()`); **no in-repo
  script or CI sets it** (`.github/workflows/erts_rust.yml` runs plain `cargo test --workspace`).
  CI does install OTP (`erlef/setup-beam`, otp-version '28'), so the oracle tests normally run there;
  locally without `erl` they silently pass.
- `$ERTS_ORACLE` set to a non-executable path is a HARD error (a misconfig must not read as "no oracle").
- `ets_continuation.rs` duplicates the oracle plumbing + gate locally rather than using `common::oracle`
  (functionally equivalent; drift risk).

## Routing Table
- `ets_basic.rs` → insert/lookup/delete/delete_object/delete_all_objects/take/tab2list, insert_list atomicity, memory-bounded churn per kind/interior
- `ets_order.rs` → `ordered_set` key-order (incl. arithmetic `1 == 1.0` keys), `first`/`next`/`last`/`prev`
- `ets_nav.rs` → navigation on every kind (absent anchors, `'$end_of_table'` → `None`)
- `ets_bag.rs` → `bag`/`duplicate_bag` multi-object semantics + `lookup_element_all`
- `ets_select.rs`, `select.rs`, `match_spec_run.rs` → match-spec compile/exec, `select*`/`match*`, `fun2ms`-shaped specs
- `ets_continuation.rs` → limited/continuation forms (own `escript` oracle cross-check)
- `ets_update.rs` → `update_counter`/`update_element` operands
- `ets_replace.rs` → `select_replace/2` + body-runtime-failure (`EXIT`) rule; oracle renderings quoted inline (offline-derived — pin tests, not live cross-checks) + `recorded_divergence_*` for the oracle's stricter static body check
- `ets_info.rs` → `ets:info` 15-field proplist
- `ets_ownership.rs` → `give_away`/`heir`/`setopts`/owner-death
- `ets_rename.rs` → `ets:rename/2`
- `ets_access.rs` → access matrix (`public`/`protected`/`private`) over the read/write surface
- `ets_options.rs` → `TableOptions` parsing/reporting
- `ets_fixtable.rs` → `safe_fixtable/2` deferred reclamation
- `ets_reclamation.rs` → the index key-anchor invariant + memory-accounting stress (see `../CONTEXT.md` Known Issues / Design Decisions)
- `ets_shared.rs` → `Send`/`Sync`, the `SharedTable` lock, no-lost-update concurrency, guard-scoped read aliasing note
- `ets_concurrency.rs` → lock independence, serial equivalence, no-lost-update check-then-act, live-oracle winner count
- `differential.rs` + `differential/` → the differential battery (embedded table + live oracle); `battery/` and `erl_program.rs`
- `common/mod.rs` → shared term/table helpers; `common/oracle.rs` → the live-`erl` plumbing and skip gate

## Notes for Agents
- The aliasing-hazard evidence tests (`a_stale_raw_term_reads_as_a_different_object_after_arena_churn`,
  `copy_out_survives_guard_drop_and_arena_churn`) live in `../src/table/view/tests.rs`, **not** here —
  `ets_shared.rs` only documents the aliasing constraint in its module docs.
- Concurrency and reclamation tests are coupled to the store's per-size free-list reuse (they force a
  freed run to be recycled); they fail loudly if that policy changes, they do not pass falsely.
- `differential.rs::rust_battery_matches_the_embedded_oracle_table` always runs (no oracle needed), so
  the erl-derived expectation table stays checked even on a machine without Erlang.
