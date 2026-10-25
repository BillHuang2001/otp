# `beam_message` / `tests` — the integration suites and their pinned claims

## Intent
The crate's contract-area integration suites: one file per area, each asserting the property named below and nothing else.
The instruments (`traversals`, `counters`, `perf`, `send_perf`, `identity`) also carry figures into `../PERF.md`; they assert **correctness only** — never a timing threshold, a walk count or an allocation count.
Every suite is a **self-check** (structural `compare::eq`, word-count arithmetic, refcounts, exact counters): no test here or in `../src/` invokes `erl`/`erlc`. Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness.

## API Surface (what a suite may use)
Only the crate's public seam (see `../CONTEXT.md`) plus `beam_term`/`beam_alloc`/`beam_gc` directly; `tests/identity.rs` and `tests/fun.rs` also use `beam_codec` (`encode_term`/`decode_term`/`Encoder`/`FunIdentity`), which is why `beam_codec` is a declared dependency.
`tests/traversals.rs` and `tests/counters.rs` install a counting `#[global_allocator]`, so they take a `MEASUREMENT_LOCK` mutex (a poisoned lock is recovered, not propagated) — the allocator is process-wide.

## Constraints
- A suite may not assert timing thresholds, walk counts or allocation counts; the deterministic counters may only be asserted **identical across `REPEATS`** (or against an exact model, e.g. the byte-touch identity).
- A new behaviour needs a suite (or a case in one) before it ships; a closed gap deletes its assertion rather than being annotated.
- Files target ~1000 lines; each suite states its pinned claim in a module doc comment.

## Test Strategy (how to run)
- Whole crate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message`.
- One instrument, with its output: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message --test traversals -- --nocapture`(likewise `--test counters|identity|perf|send_perf`).
- Workspace regression: `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace --all-targets` (and `test … --workspace --no-fail-fast`).
- Always through `../../scripts/cargo.sh` (`$HOME` is read-only in the sandbox); never `cd`; never touch `../../tools/`.

## Routing Table (file → pinned claim)
- `copy.rs` → flat-copy correctness: nested-graph equality, a `100_000`-element deep list, literal sharing by identity, MSO refcount + slot rewrite, exhaustion, a 40-key HAMT trie
- `classes.rs` → term-class completeness for the off-heap-free classes, each `compare::eq` + exact `dst.used() == size_object(term)`
- `guards.rs` → non-term/cycle guards + the destination-relocation invariant (bare header words ⇒ typed `BadArg` writing nothing; forged cycles ⇒ prompt `BadArg`; every non-shared compound subterm proven inside the destination; and the descent set a constant-delta rebase would have to cover)
- `sizing.rs` → sized-then-copied exactness over a 20-shape corpus (exact-capacity accept; one-word-short/zero refusal with `Alloc(Message)` and `used() == peak_used() == 0`; `tuple0()` by identity) — incl. `a_one_word_short_destination_is_refused_before_any_write`
- `destination_pricing.rs` → the **divergence** contract where the copier's price is *not* `size_object` (a literal root and a literal child at 0 / 3 words, a no-handle refc binary at its flattened window, a handled one at the 8-word pair, a mixed graph one word short), each pin run on both copy routes
- `single_pass.rs` → the empty-off-heap-table single-pass contract (clean + dirty destination, no-op refusal, off-heap refcount rollback, non-empty table still sized) — incl. `a_refused_copy_into_a_dirty_heap_leaves_the_live_words_intact`
- `message_size.rs` → the logical-size surface (`heap_words()` vs `size_object`/`total_used_words()`, `logical_binary_bytes()` incl. `0` for a fragmented message) + `with_payload` preservation
- `envelope_relocation.rs` → `set_from`/`set_token` replace exactly the two term words + `with_from` + a gather/relocate/scatter round-trip + the debug-only enqueue tripwire
- `share.rs` → sharing-preserving evidence (an `N = 8`-fold `1000`-element subterm: `2N + 2K` vs `2N + N·2K`; structural equality; a no-sharing graph at the flat size; a shared MSO container/refc binary refcounted once)
- `binary.rs` → refc-binary payload sharing (`orig` fix-up, `dst.used() == ERL_REFC_BITS_SIZE`) + small-`ErlHeapBits` copy-by-value + match-context flattening (no source aliasing) + the no-handle flattening
- `external.rs` → external pid/port/ref verbatim copy + node linkage/refcounting (one entry per distinct node, count 2 for two references); each copy asserted `compare::eq` its original (whole-graph and elementwise) plus the not-equal direction on a differing id — pinning that `beam_term::compare`'s external arms are structural, not blanket — alongside the `beam_term::thing` accessors and the node identity
- `fun.rs` → closure copy from the frozen `beam_term::fun` layout (`make_fun`/`make_fun3` block + env graphs, `=:=`, byte-identical `NEW_FUN_EXT`, callability, `env[0]` fixed into the destination) + the `External` 4-word block
- `fragments.rs` → the fragment-model evidence (`send` vs `send_fragmented` atomicity + the no-cleanup retry, `send_fragmented_after_refusal` delivering straight from a held refusal — fragment sized to the exact figure, the `fragment_capacity_words` floor, the non-word-multiple `size_object` fallback — `move_into` payload + off-heap transfer incl. a node reference, the multi-fragment chain, accounting) — incl. `a_failed_move_into_leaves_the_chain_intact`
- `queue.rs` → `send`/`receive` + FIFO queue integration + the `QueueCursor` scan/remove path + `a_rejected_send_is_a_total_no_op`
- `alloc.rs` → the allocation-failure classification contract (`requested == words * size_of::<Eterm>()`, heap untouched, the predicate's accept/reject split)
- `traversals.rs` → the structural instrument: per send shape the census vs **written** objects/words, residency, blocks, bytes, the full allocation-size histogram and `ns_min`, asserted identical across `REPEATS` over five shapes (literal twin, heap twin, 1-word-short, full-heap fragment, `ets_copy` 233-word nursery); plus `report_a_warm_preflight_allocates_nothing`, the cold → warm → shrunk-cold block-count pin over a `Mode::Refused` window (the two cold figures equal, the warm one strictly smaller because only the copy's fix-up worklist allocates)
- `counters.rs` → the deterministic counter harness over the fits path (per-shape words/objects/header overhead, blocks, bytes, bytes touched, the copy-vs-enqueue split, the asserted byte-touch model `2 · words · size_of::<Eterm>()`)
- `identity.rs` → copy-boundary **term identity** (correctness, never a figure): every corpus shape `compare::eq` *and* byte-identical `term_to_binary` across the copy, which is what proves the literal send is *shared*, not dropped
- `perf.rs` → the dependency-free copy-cost measurement (ns/word, ns/object) + the single-pass-vs-sized preflight-share probe
- `send_perf.rs` → the dependency-free per-send measurement over the real `beam_message::send` path (clean vs dirty destination, five shapes)

## Notes for Agents
- `traversals.rs` and `counters.rs` must stay the *only* consumers of the counting global allocator, and they must both take `MEASUREMENT_LOCK`; a third suite taking it is fine, a third suite installing its own allocator is not.
- The instruments assert `identical across REPEATS` deliberately: a stray concurrent allocation then fails the run instead of inflating a count. Do not loosen that to a tolerance.
- `guards.rs`'s descent-set walk is the specification for any future constant-delta rebase; keep it complete when a new term shape gains interior pointers.
