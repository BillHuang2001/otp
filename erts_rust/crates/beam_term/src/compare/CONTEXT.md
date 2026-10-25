# `compare/` — the layout-aware arms of `erts_cmp`

## Intent
The submodules of `../compare.rs`: the comparison arms that need per-thing layout knowledge and would
push that file past the ~1000-line guideline.
`../compare.rs` owns the entry points, `class_rank`, `CmpCtx` and `cmp_impl`; each file here owns one arm.
Ground truth: `../../../../erts/emulator/beam/utils.c` (`erts_cmp_compound`), `erl_utils.h` (`erts_cmp`).

## API Surface
### `bits.rs` — the bitstring arm
`cmp_bits` (the `erts_cmp_bits` port): when both bases are byte-aligned, compare the whole bytes as one
`&[u8]` compare, then the tail bits from the top of the next byte; the bit loop runs only for a
non-byte-aligned base.
Bits are MSB-first inside each byte (the `bitstring.rs` payload convention).

### `identity.rs` — the mixed internal/external identity arms
`LocalNodeId { sysname: Eterm (an atom), creation: u32 }` is the runtime's own node (`erts_this_node`),
constructed with `LocalNodeId::new(sysname_atom, creation)`.
`cmp_mixed` decides a mixed internal/external pid/port/ref pair field by field, in the C's order; `cmp_ref_ids`
is the trim-leading-zeros big-integer id compare the external arm shares.
The C's `CMP_NODES` is ported as `(sysname atom **name**, creation)` — node order is name-based, which no
hash reproduces, so `atoms` stays a parameter of the whole walk even though atoms otherwise compare cheaply.

### `maps.rs` — the `MAP_SUBTAG` arm
`cmp_maps` takes the `CmpCtx` for its **values**; its **keys** stay on the canonical `cmp_flatmap_keys`
order, because a map's stored key order must not depend on which node asks.
Two flat maps whose keys are non-descending in that order are compared straight out of storage with **no
allocation** (`thing::flatmap_parts`); any other pair — a hash map, or an order-preserving producer's
unsorted flat map — uses the collecting `map_pairs_flatmap_order` fallback over the same key-then-value
sequence, so the fast path and the fallback still compare one rule.

### `numbers.rs` — exact mixed numeric compare
`pub(super) cmp_numbers(...)` is the `mixed_types` port of `erts_cmp_compound` plus `erts_float_comp` and the
private `double_to_big`, for a pair of *different* numeric classes: small↔float inside the lossless
`±(2^53−2)` window else exact integer subtraction; big↔float by magnitude bands, lossless `big_to_f64`, then
exact `double_to_big` + bignum.
This is why `cmp_arith` never rounds both sides to `f64`: `1 == 1.0`, `-0.0 == +0.0`,
`9007199254740993 > 9007199254740992.0`.

## Constraints
- **`local: None` is the documented no-context convention**, not a bug: with no `LocalNodeId` a mixed
  internal/external pair of one category is decided by `class_rank` (`external_*` below its internal
  counterpart). With a `local`, the same-category pair is decided **before** the rank shortcut.
- Two conventions the C asserts or cannot reach: a **same-node port** pair (the C raises
  `ERTS_INTERNAL_ERROR`) compares `internal_port_data` with the external 64-bit number; the M1 internal
  reference's single 60-bit id is expressed as the id array `[id as u32, (id >> 32) as u32]` (high word at
  the high index) so it goes through the same `cmp_ref_ids` trim/compare as the external arm.
  A same-node, same-field pid/ref pair falls through to `Equal` (the C's `CMP_NODES` no-op then `pop_next`).
- **A map's key order is canonical across nodes**; only its values may depend on `CmpCtx`.
- `unsafe` (deref of term payload words) stays confined to the marked arms; every deref site applies the
  crate's `is_term_word`/`is_boxed_term` guard first (`../../../CONTEXT.md` "Notes for Agents").

## Handoffs (owned by other nodes — escalate writes to the parent)
- **A comparison that must agree with `erts_cmp_compound` needs the local node, and only `beam_bifs` supplies it.**
  `compare.rs`'s `local_node_id(ctx)` resolves this runtime's own `(sysname atom, creation)` once per BIF call through the `LocalNode` hook, and `cmp_arith_node`/`cmp_exact_node` switch to `cmp_arith_with_local_node`/`cmp_with_local_node` only when `access::is_mixed_identity_pair` says the pair can be decided by the node — `<`/`>`/`=/=`/`==`, `erts_internal:cmp_term/2`, and the ordering `lists`/`convert` functions, which thread the same `Copy` value through a whole sort.
  Two nodes deliberately do NOT adopt it: `beam_process` carries typed `ProcessId`/`PortId` handles and never orders pid terms, so it has no site reaching these entry points, and `beam_ets`' `ordered_set` keeps `cmp_arith` because a table's ordering must not depend on which node asks (`cmp_flatmap_keys` stays canonical for the same reason).

## Test Strategy
`../../tests/compare_class_order.rs` pins the full 18-rung ladder, every field-precedence step, the
`local: None` ladder, the 18×18 no-change matrix and the oracle's own verdicts.
`../../tests/compare_external.rs` covers the external/record arms with a live-`erl` check;
`../../tests/compare_oracle.rs` cross-checks the arithmetic mixed numeric path against `==`/`=:=`/`<`/`>`;
`../../tests/compare_terms.rs` covers the class order, name-based atom ordering and non-term hardening;
`../../tests/map_alloc.rs` proves the flat-vs-flat map compare allocates **0**.
Oracle suites skip loudly (never fail) when `erl` is absent.

## Routing Table
- `bits.rs` → the bitstring comparison arm (`cmp_bits`, `erts_cmp_bits`)
- `identity.rs` → `LocalNodeId`, `cmp_mixed` (mixed internal/external pid/port/ref field arms), `cmp_ref_ids`
- `maps.rs` → the `MAP_SUBTAG` arm (`cmp_maps`: allocation-free flat-vs-flat fast path + collecting fallback)
- `numbers.rs` → exact `mixed_types` numeric compare + private `double_to_big`
- `../compare.rs` → the parent: entry points, `class_rank`, `CmpCtx`, `cmp_impl`, `cmp_funs`, `fallback_cmp`
