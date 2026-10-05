# `beam_alloc` — Crate Integration Tests

## Intent
Cross-module integration tests for the `beam_alloc` crate, exercising the **public** API only
(`Allocator`/`AllocRef`, `CarrierAllocator`, `Heap`, `SizeClasses`, `AllocStats`, the registry).
Unit tests that reach into implementation internals live in each module's `#[cfg(test)] mod tests`
(see `../CONTEXT.md` Routing Table).

## API Surface
- `allocator.rs` → allocator-seam integration: alloc/dealloc round trips across the size spectrum,
  alignment (8/16/…/4096), thousands of mixed-size allocations forcing many carriers, the
  slab-vs-dedicated threshold split, large-carrier release on free, `realloc` grow/shrink preserving
  contents, stats deltas, the `max_footprint` ⇒ `AllocKind::Carrier` path, custom `SizeClasses`,
  all `Strategy` variants, `default_allocator()`/`all_stats()`, and a `Heap` over a real
  `CarrierAllocator`.
- `heap.rs` → `Heap` integration: geometry (`capacity`/`used`/`available`/`peak_used`/`reset`),
  `alloc_words`/`alloc_tuple`/`cons`/`tuple`, pointer stability (never reallocates), exhaustion ⇒
  `AllocKind::Heap` with zero words consumed, and `Drop` returning the block. Uses a **test-local**
  `Allocator` (there is no dependency on the carrier allocator here).

## Constraints
- Public API only — no `pub(crate)`/internal access.
- Tests must not panic on the runtime path; use `beam_common::{Result, BeamError}`/`AllocKind`.
- Run with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_alloc`
  (sandbox `$HOME` is read-only — always the wrapper; never `cd`).

## Notes for Agents
- The crate's compatibility gate against real ERTS is the sibling `../../tools/differential/` harness,
  not these tests.
- No leak assertions rely on the build having a leak checker; a Miri/leak-check run is meaningful but
  not part of the M1 gate.
