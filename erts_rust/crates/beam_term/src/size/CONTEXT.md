# `size/` — the sizing walk's working memory and its hasher

## Intent
The two buffers/hasher pieces of the sizing walk that are large enough to own their own modules.
`../size.rs` owns the walk itself (`size_object`, `size_object_bounded`, the `SizingPolicy` parameterisation
and the recursive `classify`/`descend`/`push_frame_children` steps); this node owns *where its buffers come
from* and *how the path set is hashed*.
Ground truth for what is being counted: `../../../../erts/emulator/beam/copy.c` (`size_object_x`).

## API Surface
### `scratch.rs` — `SizingScratch`
The caller-owned, reusable working memory of one walk: the explicit `Vec<Step>` DFS stack, the
`HashSet<usize>` of compound objects on the **current** DFS path (the cycle guard), and — for the
`OverLimit` arm of the bounded form only — a `done` set for the acyclicity pass.
Constructors: `new` (== `Default`, empty buffers, no allocation until the first walk needs one),
`with_capacity` (pre-size for the payloads you expect), `shrink_to_fit` (return an outlier graph's
high-water-mark buffers to the allocator).
It is **opaque** — `pub(super)` fields, no accessor exposing a buffer — so a walk's layout stays an
implementation detail; a caller can only construct one, hand `&mut` to the `*_with_scratch` entry points, and
shrink it.
A scratch carries **no** state between calls: every walk clears the buffers before it starts, so a scratch
used on a cyclic graph and then an acyclic one answers each correctly, and the walk stays a pure function of
`(term, limit, policy)`.

### `address_hash.rs` — `AddressHashBuilder`/`AddressHasher`
The path set's hasher: the classic `FxHash` shape (one multiply by an odd 64-bit constant, then one fold of
the high half into the low half), not SipHash.
The fold is not decoration: heap terms are word-aligned, so the low three bits of every address are zero and
an odd multiply preserves trailing zeros; without the fold, `hashbrown`'s low-bit bucket selection would only
ever pick every eighth bucket.
Membership-only ⇒ a collision costs one probe and can never give a wrong answer, and the keys are addresses
this process itself produced, so there is no untrusted input to flood the table with.

## Contracts (per-call cost of the walk)
- The buffers are **cleared, never dropped**, per call, so `size_object_with_scratch` /
  `size_object_bounded_with_scratch` are **allocation-free once the scratch is warm** (a bigger graph grows
  the buffers once and retains the capacity).
  `size_object`/`size_object_with`/`size_object_bounded`/`size_object_bounded_with` keep their signatures and
  delegate with a fresh `SizingScratch`: same count, same verdict, one or two allocations per call, and
  `../../tests/size_scratch.rs` pins that the two forms always agree.
- The buffers are moved into frame locals for the walk (`with_taken_buffers`) because LLVM cannot prove
  `&mut scratch.field` disjoint from the raw `*mut Eterm` payload reads.
- `size_object_bounded_with`'s `OverLimit` arm re-walks the graph through `is_acyclic` (a `done` set on top of
  the path set) to tell `Cyclic` from `TooLarge`, so a consumer that then re-runs the walk for the exact
  count (the shape `beam_message::copy::preflight_words` uses) pays **three** graph traversals; the fitting
  case is one. Over a warm scratch those are three traversals of the *same* buffers, not three sets of
  allocations.
- Each boxed object's slots are enumerated **once** (`classify` → `for_each_sizing_child_slot`);
  `push_frame_children` re-reads the same slots by pointer rather than calling the enumerator again.
- **Only objects the walk descends into are put on the path.** `descend` pushes the object's `Exit` marker
  and its visitable children and reports how many it pushed; `0` (every child slot is an immediate/non-term)
  pops the `Exit` again and leaves the object off the path set — an object with no visitable child can be on
  no cycle, and no verdict or count changes.
  This is the leaf case (a tuple of immediates), i.e. most compound objects in a message payload, and it
  halves path-set traffic there. It is **not** a visited-set: sharing between sibling positions is still
  counted once per occurrence.
- The path-set insert sits **after** the `is_shared` check and before the `total > limit` bail — which is why
  `Cyclic` stays reachable under a finite limit.
- Termination on a finite `limit` follows from every object costing ≥ 1 word under `DefaultSizingPolicy`, but
  a caller policy may return `Some(0)` for a compound that has children, so the **path set** (not the limit)
  is the walk's termination guarantee.
- `SizingPolicy` exposes exactly two hooks and **no** enter/exit/over-limit callback, so a caller can dedup
  globally (a `RefCell<HashSet>` in `beam_bifs`' `SharedOnce`) but cannot implement path-based cycle
  detection itself.

## Handoffs (owned by other nodes — escalate writes to the parent)
- **`beam_message` is the policy's production consumer, and the pitfalls there are mutation-proven.** `beam_message/src/copy_reference.rs` overrides both axes on `size_object_bounded_with` — `is_shared` = the copier's own literal/immediate rule, `object_words` = the flattened window `heap_bits_size_words(end - start)` for a refc binary whose payload **no source off-heap handle owns**, else the default — and keeps its retired hand-written walk only as the equivalence oracle.
  Never price a destination with the bare `size_object`/`size_object_bounded`: the default descends into literals (over-count ⇒ a copy that succeeds today is refused) and prices the refc pair unconditionally (under-count ⇒ a partial write before the typed refusal).
  Its agreement test `copy_reference::tests::a_forged_zero_arity_record_header_prices_the_structural_one_word` pins both walks answering the structural `Some(1)` on a forged zero-arity record header.

## Test Strategy
`../../tests/size_scratch.rs` — the scratch-backed and scratch-free entry points agree over the
`size_policy.rs` corpus (literals, refc binaries, sub-binaries, flat/HAMT maps, all three fun kinds, external
pid/port/ref, native record, a cyclic cell, a 100 000-element list) × four policies × limit sweeps, with a
fresh *and* a carried scratch; a counting `#[global_allocator]` proves a warm scratch is **0 allocations / 0
bytes per call** where the scratch-free form scales with the graph; and one scratch serves unrelated graphs
without leaking state (cyclic then acyclic).
`../../tests/size_bounded.rs` / `size_policy.rs` / `size_layout_agreement.rs` own the counting and
policy semantics.
`../../examples/size_walk_bench.rs` is the in-process A/B (`retired` pre-change copy vs `fresh` scratch-free
vs `scratch` warm), interleaved per call with a rotating order, reporting ns/call, the ratios, per-call
allocation counts and `/proc/loadavg` before/after.

## Routing Table
- `scratch.rs` → `SizingScratch` (opaque, reusable, allocation-free when warm)
- `address_hash.rs` → `AddressHashBuilder`/`AddressHasher` (FxHash-style address mix)
- `../size.rs` → the walk itself: `size_object`/`size_object_bounded`, the `SizingPolicy` axes
  (`size_object_with`/`size_object_bounded_with`/`DefaultSizingPolicy`), the scratch entry points, and
  `classify`/`descend`/`push_frame_children`
