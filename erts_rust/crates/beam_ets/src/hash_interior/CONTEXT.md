# `beam_ets::hash_interior` — the chained-bucket hash interior

## Intent
The chained hash index behind **`bag`** and **`duplicate_bag`**, and the opt-in `SetInterior::Chained` (a `set` defaults to `../champ_interior.rs`).
A bucket array (`Vec<Vec<Entry>>`) indexed by `beam_term::hashing::term_hash` and searched with `beam_term::compare::eq`; nothing here re-implements term order or hashing.
Public for testing and reuse, but **not** a cross-crate seam: dependents go through `crate::Table`.

## API Surface
- `mod.rs` — `struct HashInterior { kind, keypos, buckets, len }` and `struct Entry { key, objs }`.
  Constructors `new(kind)` (default keypos 1, for the degenerate `object == key` callers the unit tests and `interior_bench` drive directly) and `with_keypos(kind, keypos)` (what a `Table` always uses).
  Inherent: `take(key, atoms)`, `update_counter(..)`, `update_element(..)`, `memory(atoms)`, `bucket_count()` (the `ets:slot/2` `nactive` boundary — this interior's bucket-array length), `keys()`.
  The `Interior` impl: `insert` (**returns whether the object was retained**, so the caller reclaims a rejected copy), `insert_new`, `delete`, `delete_object`, `lookup`, `member`, `lookup_element(+s)`, `size`, `iter_objects(+_rev)`, `keys`, `take`, `slot` (the bucket-array override), `first_key`/`last_key`/`next_key`/`prev_key` (with the hash-kind aliasing — `last`/`prev` ARE `first`/`next`), `memory`, `bucket_chains` (the occupancy histogram behind `ets:info(Tab, stats)`), `update_counter`, `update_element`.
- `capacity.rs` — the bucket array's growth/release policy (no `unsafe`).
- `tests.rs` — the interior's unit tests; the shrink/churn ones live in `capacity.rs`, next to the policy they pin.

## Kind rules
- `set` keeps at most one object per key (an upsert replaces it).
- `bag` rejects a duplicate **object** within one key (`eq`-compared; `insert` returns `false` and the caller reclaims its copy).
- `duplicate_bag` keeps a multiset — no duplicate rejection at all.
- `delete_object` removes **every** equal object (the oracle's behaviour) and then re-derives a surviving entry's stored key term from one of its remaining objects — `keypos`' only use, because a stored key aliases a table-owned object that may just have been removed.

## Capacity policy (`capacity.rs`)
- Constants: `INITIAL_BUCKETS = 8` (a power of two, so the modulo index distributes evenly; the array is never empty afterwards), grow at a `3/4` object load (`LOAD_FACTOR_NUM`/`_DEN`), shrink below `1/8` (`SHRINK_NUM`/`_DEN`), per-entry `objs` surplus released past a `4×` slack (`OBJS_SHRINK_FACTOR`).
- **The `1/8` … `3/4` band is a 6× hysteresis band:** a stable-size insert/delete churn never rebuilds the array, so one delete cannot undo what a whole series of inserts paid for.
- Every rebuild goes through `rehash_into(target, atoms)`, which allocates the new array at **exactly** `target` and re-inserts every entry at `term_hash(key) % target`; stored capacity therefore tracks the live contents instead of a high-water allocation.
- `maybe_grow` doubles through it while the load passes `3/4` (`len` is the **object** count — what `size` reports — so the multi kinds grow a little earlier than the entry count alone would, which is harmless).
- `maybe_shrink` halves while the load stays under `1/8` (one long teardown reaches a proportional bucket count instead of paying a rebuild per deletion) and never goes below `INITIAL_BUCKETS`.
- `trim_objs(objs)` releases a single key's `objs` slack once `capacity > 4 * len` — the per-key counterpart of `maybe_shrink`, with the same hysteresis intent.

## Constraints
- Errors are `beam_common::{Result, BeamError}`; a malformed object/key is `badarg`. No crate-local error enum, no `unwrap` on runtime data.
- No `unsafe` in `capacity.rs` or `tests.rs`; the one `unsafe` block is `mod.rs`'s destructive counter-element write, with its `// SAFETY:` rationale.
- Term order/hash/size come from `beam_term` — never re-implemented.
- `memory()` counts the index's own words (bucket vectors + entries); the term payload belongs to the table's `TermStore`.
- `pos == keypos` (which would rewrite a key) is `crate::Table`'s rule, not this interior's.

## Test Strategy
- Unit tests over the `Interior` surface and the kind rules (`tests.rs`), plus the capacity policy's growth/shrink hysteresis and churn (`capacity.rs`, next to the constants they pin).
- Cross-interior equivalence: `../interior_parity.rs` drives one shared LCG script through this interior and the CHAMP one and requires **identical traces** — the opt-in `Chained` `set` and the CHAMP default are observationally equal.
- Live-oracle evidence lives in the crate's integration suites (`tests/ets_bag.rs`, `tests/ets_slot_oracle.rs`, `tests/ets_capacity.rs`, crate-root-relative) and the differential battery — never here.

## Routing Table
- `mod.rs` → the type, bucket lookup, the inherent `take`/counter/element ops, the `Interior` impl (incl. the `slot` and `bucket_chains` overrides)
- `capacity.rs` → `INITIAL_BUCKETS`/`initial_buckets`, `grow`/`rehash_into`/`maybe_grow`/`maybe_shrink`, `trim_objs` + the shrink/churn unit tests
- `tests.rs` → the remaining unit tests

## See Also
- `../champ_interior.rs` → the **default** `set` interior.
- `../../CONTEXT.md` → the crate's `Interior` seam, the A.1 copy boundary and the `ets:slot/2` contract (paths in this file are relative to `src/hash_interior/`; `tests/…` is crate-root-relative).
- `../../ETS_SURFACE.md` → the `ets:*` coverage census; `../../PERF.md` §2/§3 → the measured memory/interior figures.
