# `tests/` — the `beam_term` integration suites

## Intent
Crate-level integration tests: everything that must consume the crate **only through its public API**
(`beam_term::prelude::*`), plus the live-`erl` oracle cross-checks and the deterministic allocation pins.
In-module `#[cfg(test)] mod tests` in each `src/*.rs` owns that module's raw-word pins; this directory owns
the cross-module and public-surface behaviour.

## Test Strategy (per-file inventory)
| File | Pins |
|---|---|
| `layout.rs` | the executable spec of the frozen bit patterns (`make_small(0) == 0xF`, `make_atom(0) == 0xB`, `NIL == 0x3B`, `make_arityval(1) == 0x40`, cons tag `0x1`, boundary sweeps) via the public API, which also validates the flat re-exports |
| `abi.rs` | `Eterm` is one machine word: `size_of`/`align_of` == `usize`, `Copy`, transparent raw round-trip, `NIL` default (what `beam_abi` transmutes against) |
| `non_term.rs` | a word that is not a term never crashes an entry point — the total/fallible policy per API |
| `internal_ref.rs` | the M1 4-word internal-reference block literally (raw header `0xD0`) + round-trip, typed errors, 4-word size, `=:=`/ordering |
| `atom_witness.rs` | `snapshot`/`appended_since` are allocation-free (counting `#[global_allocator]`) and O(1)-shaped (4 vs 200 000 atoms), plus clone-divergence/replaced/shrunken-table soundness at 50 000 atoms |
| `atom_fork.rs` | `AtomTable::fork` through the public API: index preservation, name-address identity, the `clone` sensitivity control, independent append streams both ways, the witness false both ways, empty/fork-of-fork, the lower-then-merge protocol |
| `intern_alloc.rs` | interning allocation counts: one `Arc<str>` per novel name, 0 for `lookup`/`name`/a known-name `intern`, `seed` of 3000 names within a budget derived from the store's constants (`NAMES + seed_slack()`, i.e. one string per name plus bounded *container* allocations), `fork` **0 at every table size** (empty/300/3000), the fork-then-append copy-on-write step bounded and flat across two sizes, and `clone` Θ(table) as the sensitivity control |
| `map_alloc.rs` | `cmp` of two flat maps allocates **0**; `write_map` of a 32-pair input allocates **1** and is word-for-word `write_flatmap`'s output; a >32-key pair is the sensitive control; a hand-built unsorted flat map still compares exactly |
| `map_build_equivalence.rs` | **the `write_map` word-equality harness**: every matrix row (0/1/31/32/33/34/40/100/1000 keys, the dedup crossover, `0.0`/`-0.0` vs `1`/`1.0` distinctness, insertion-order permutations, a 20 000-key row) built twice — live writer vs `mod retired`, a frozen verbatim copy of the pre-sort-and-collapse algorithm — compared word for word after `normalize` block-relativizes every tagged in-buffer pointer, plus `GOLDEN_*` vectors, the overflow sentinel beyond `map_words_capacity`, shape and the readers |
| `map_words_capacity.rs` | the reservation-vs-written measurement for `map_words_capacity` plus the sound-bound and untouched-tail guards |
| `map_cow_equivalence.rs` | **the incremental (path-copying) writers' equivalence, structural-safety and scaling harness** — long because it carries four harnesses (op-script sweep, canonical renderer, copier's structural walk, sizing pin) plus the 11-class fixture pool. *Equivalence:* one deterministic op script (value replace, insert, present/absent remove, the flat↔HAMT conversions both ways, delete-to-empty) at 0/1/2/31/32/33/34/64/250/2 500 keys over atoms, small ints, bignums, an on-heap and a `refc` binary, tuples, lists, an `EXPORT_EXT` fun, a nested map, `1` vs `1.0` and a low-nibble collision group, driven through `write_map` **and** through `map_put_cow`/`map_remove_cow`, requiring `=:=` identity plus equal `map_size`/`map_pairs`/`map_pairs_storage_order`/`map_get` and an identical `render_map` (equal stored pair sequence + equal node shape = the `term_to_binary`-bytes claim, stated without the `beam_codec` dependency cycle). *Structural safety:* walks every reachable object through `layout::object_words`/`for_each_child_slot`, requiring the result block's objects to price **exactly** the words written, every off-block object to be reachable from the source (off-path subtrees shared by word, never copied), the flat value-replace's keys-tuple word to be the source's own, both conversions to share no node, `size_object` to agree with the *sizing* walk, and the copy walk's delta to be exactly one 3-word `BinRef` per `refc` binary (the two enumerators' documented divergence). *Scaling:* reserves exactly `*_words_required` under a sentinel tail, bounds it by `*_words_capacity`, requires n = 2 500 to exceed n = 250 by at most one 18-word node and a 32-put fold to stay within `32 × 293` words (measured 33.8 words/op at n = 250 vs the rebuild's 1 033 = 31×; 47.2 vs 9 282 = 197× at n = 2 500), and records the one arm whose plan is a bound rather than an exact write (the flat→HAMT conversion: 711 planned, 127 written). One `#[ignore]`d test prints `map_put` vs `map_put_cow` ns/op and asserts nothing |
| `map_hash.rs` | HAMT pair-**storage** order vs a live `erl` (two offline-pinned vectors + 33/40/50/64/100-key mixed maps) |
| `oracle.rs` | live-`erl` map size/get/pairs at the 31/32/33/40/100-key flat/hash boundary, bitstring parts/bits, bignum `eq`/`cmp`/`cmp_arith`/`size_object` across the 2-/3-limb boundary, and flat-map stored key order vs `maps:to_list/1` |
| `number_oracle.rs` | `big_to_f64` vs `float/1` as IEEE-754 bits over a bit-length sweep + randomised limbs (`ERL_TIMEOUT = 60s`) |
| `compare_terms.rs` | the public `cmp`/`eq`/`cmp_arith` surface: documented class order, name-based atom ordering, numeric exactness, structural tuple/list/binary/pid/port/fun ordering, non-term hardening |
| `compare_class_order.rs` | the **full** 18-rung cross-class ladder (every adjacency and the whole `i<j ⇒ Less`/`i==j ⇒ Equal`/`i>j ⇒ Greater` matrix), the rank-order-vs-numeric split between `cmp` and `cmp_arith`, every fun kind on the fun rung, a refc binary on the bitstring rung, the six mixed internal/external boundary pairs, and a live-`erl` oracle matrix |
| `compare_external.rs` | the external pid/port/reference and native-record structural arms with a live-`erl` check |
| `compare_oracle.rs` | arithmetic/exact comparison vs `==`/`=:=`/`<`/`>`, including the `>2^53` asymmetry |
| `record_ref_hash.rs` | offline `(make_hash2, make_hash, definition_hash)` triples for a native record, the reference shapes (structural properties only — reference numbering is per-VM) + a real `-record` fixture cross-check |
| `size_bounded.rs` | the bounded/cycle-safe walk: cycles, budget inclusivity, non-terms ⇒ `Ok(0)`, a 100 000-element list, exact flat/HAMT sizes, per-occurrence counting |
| `size_policy.rs` | the `DefaultSizingPolicy` differential over a 27-term corpus (literals, refc binaries, sub-binaries, flat/HAMT maps, all three fun kinds, external pid/port/ref, native record) plus each policy axis as a mutation-sensitivity control (literal sharing flips the count, flattening flips `8` ⇒ the window, a shared cyclic cell is `Ok(0)` against the default's `Err(TermGraphCyclic)`) |
| `size_layout_agreement.rs` | the **whole header space** — every `BoxedTag` subtag plus the reserved `0x04`/`0x3C`, over the `0/1/3` arity sweep and each shape-owned interior — asserting per row that `layout::object_words`, `size_object`, `size_object_bounded` (exact budget fits, one word under refuses), the `*_with(&DefaultSizingPolicy)` forms and the sizing-child run all agree with the hand-derived count, that `for_each_child_slot` differs only for `ErlSubBits`, and that real constructors reproduce the table; named rows pin the one deliberate divergence (literal `tuple0()` ⇒ `0` vs structural `1`) |
| `size_scratch.rs` | the `SizingScratch` contract: equivalence with the scratch-free forms, 0 allocations / 0 bytes per call when warm, and no state leaking between unrelated graphs |

## Constraints
- **Oracle suites skip loudly, never fail, when `erl` is absent**, and speak a text protocol (a dev-dependency
  on `beam_codec` would cycle).
- **Nothing here asserts on wall-clock time.** The host is shared: allocation counts, word counts and
  retired-instruction figures are the load-independent evidence; timing shapes go to `../examples/`.
- `map_build_equivalence.rs` is an **equivalence gate, not a golden re-pin**: a dedup rewrite must keep the
  file unchanged and green.
- The counting `#[global_allocator]` pattern (a `std::alloc::System` wrapper with counters armed **per
  thread**) is deliberate: libtest's harness must never perturb a measurement.
- Exact-allocation pins are deliberate and bidirectional: a *reduction* fails them exactly like a regression,
  so the owner re-derives the constant.

## Run recipe
`erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_term`
(never `cd`; the wrapper sets a writable `CARGO_HOME`). A single suite:
`... -p beam_term --test compare_class_order`.

## Routing Table
- `layout.rs`, `abi.rs`, `non_term.rs`, `internal_ref.rs` → the frozen-layout / ABI / non-term contract pins
- `atom_witness.rs`, `atom_fork.rs`, `intern_alloc.rs` → `AtomTable` witness, fork and interning allocation pins
- `map_alloc.rs`, `map_build_equivalence.rs`, `map_words_capacity.rs`, `map_hash.rs` → the map builder, its
  capacity reservation and its stored key order
- `oracle.rs`, `number_oracle.rs`, `compare_oracle.rs`, `compare_external.rs`, `map_hash.rs` → the live-`erl`
  oracle cross-checks
- `compare_terms.rs`, `compare_class_order.rs` → the term-order surface
- `record_ref_hash.rs` → `phash2`/`phash` for the record and reference shapes (internal refs hash by their `id` word, external refs by their wire numbers, a C-layout ref block is the typed `BadRef`)
- `size_bounded.rs`, `size_policy.rs`, `size_layout_agreement.rs`, `size_scratch.rs` → the sizing walk
