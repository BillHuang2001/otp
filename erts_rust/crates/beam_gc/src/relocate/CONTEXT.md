# `beam_gc` — Region-Generic Relocating Compaction (`src/relocate/`)

## Intent
The seam that lets a **host-owned word region** — a `beam_alloc::ReservedHeap`, a pre-reserved `Vec<Eterm>`, an
mmap'd arena, `beam_engine`'s term arena — be compacted by the same Cheney copy engine, without a `Collector`, a
`Nursery` or a `HeapSpace`.
The implementation is the sibling module `../relocate.rs` (the driver) with `../space.rs`'s `FromSet` and
`../copy.rs`'s `EvacDest`/marker primitives; this node owns the seam's contracts plus its in-module suite
(`tests.rs`).
`../../CONTEXT.md` (crate) states the user-facing seam shape; `../CONTEXT.md` owns the collector design decisions;
this file owns what the relocating compactor itself promises.

## The seam
- **The copy engine is driven over a destination the HOST owns.** `Relocator`/`compact` run the same Cheney
  protocol (forward through `forward_slot` → in-place marker → scan the destination) with the from-set built from
  **raw word ranges** and the destination supplied by the caller; nothing on the collection path reads this
  surface, and the copies are accounted by `copy.rs::forward_accounted` — the **one** site `Copier::forward_one`
  also delegates to — so a relocation and a collection cannot count differently.
- **`FromSet::from_ranges(ranges: &[(usize, usize)])` is the `unsafe` constructor, and its `# Safety` list is the
  load-bearing part.** Each `(base_address, used_words)` must be word-aligned; its byte extent must be readable
  **and writable** (a copier forwards in place) for as long as the set is in use; `used_words` must be a **prefix
  of a real live allocation**, never a fabricated or previously-owned extent (an over-stated extent makes the
  copier rewrite words no term owns, an under-stated one strands live terms outside the evacuation); the ranges
  must be pairwise disjoint; no other thread may free, reallocate or mutate those words while a copy is in
  flight; and **completeness** (no live term outside the ranges) is the caller's claim, exactly as it is for a
  root slice (`../../CONTEXT.md`, "What frees term memory").
  Zero-length entries are skipped and every entry goes through the private `insert`, so the sorted-by-`lo`
  invariant and `locate`'s "single interpreter of the range geometry" property are preserved rather than
  re-implemented; alignment and disjointness are `debug_assert!`s, so a release build trusts the caller.
  `ranges()` is `pub` (half-open **byte** pairs, sorted by `lo`) beside `range_count()`/`words()`/`is_empty()`,
  and `words()` still over-counts a range registered before its region was filled.
- **`copy.rs::EvacDest` is the public destination seam, and `Target` with it.** The four methods
  (`classify_boxed`/`classify_cons`/`can_reserve`/`reserve`) carry the implementation contract: a method may
  dereference a pointer only **after** establishing the destination owns that memory (containment before the
  header read; `Foreign` is the only correct answer for a foreign address), `reserve` is called only for a
  request `can_reserve` admitted and may not alias the source (the marker is written *into* the source object),
  and `classify_boxed` must answer `Forwarded` for an object already moved — which is what makes a revisit a
  follow rather than a second copy.
  The one **defaulted** method, `copied_top() -> Option<*mut Eterm>`, is the walkability declaration a Cheney
  destination scan needs: `Some` means copies land in one contiguous run that advances monotonically in
  reservation order (`impl EvacDest for Copier` answers `Some(self.to.top())`, the bump pointer the scan is
  seeded from), and the default `None` declares the destination unwalkable — which is how the line-granular
  mark-region space keeps its own block-structured driver.
  `forward_slot`/`forward_accounted`/`move_size`/`for_each_ref` stay `pub(crate)`: a host supplies a destination
  and calls the driver, never the marker scheme itself.
- **`LinearDest` is the turnkey destination for a flat host region** (`LinearDest::new(&from, base,
  capacity_words)` — unsafe: word-aligned, writable, exclusively-owned words that stay valid and unmoved until
  the relocator finishes, and **no overlap with any from-range**, the overlap test being what the marker relies
  on), then `base`/`capacity_words`/`used_words`/`available_words`.
  Its classification is `Copier`'s rule verbatim (`!from.contains(ptr)` ⇒ `Foreign`; else `is_header(*ptr)` ⇒
  `Live`, otherwise `Forwarded` for a boxed word and `is_non_value(*ptr)` ⇒ `Forwarded`/`Live` for a cons),
  `reserve` bumps `used` and refuses with the typed `heap_exhausted(words)`, and `copied_top` is
  `Some(base + used)`.
- **`Relocator` is the multi-slice driver; `compact` is its atomic single-slice form.**
  `Relocator::new` records the destination's `copied_top()` as the **scan origin** and returns
  `BeamError::BadArg` when it is `None`, mutating nothing; `new_checked` adds the preflight
  (`!dest.can_reserve(required_words)` is `heap_exhausted(required_words)` before the first word moves), which is
  what makes a **multi-root-slice** compaction atomic (pass the exact copy volume, `live_words_in` summed over
  every slice; an over-estimate only causes a conservative refusal).
  `forward_roots` is **idempotent and callable once per root slice**: a slot already rewritten names the
  destination, which classifies `Foreign`, so a second pass over the same slice copies nothing and a shared
  subterm reached from two slices is copied once, while an immediate, a bare header word and a reference outside
  every range are left byte-identical.
  `complete` runs the destination scan to completion — `while scan < dest.copied_top()`, per word on
  `primary_tag`: `Header` ⇒ `move_size` + `for_each_child_slot` forwarding + `scan.add(total)`, `Boxed`/`List` ⇒
  one reference word + `scan.add(1)`, `Immed1` ⇒ `scan.add(1)` — and reports `Relocation { stats,
  dest_words_used }`, with `words_reclaimed = from.words() - words_copied` and the two collection counters **`0`**
  (a relocation is not a collection of a process heap: it advances no `minor_gcs`/`fullsweep` cadence); for a
  linear destination `dest_words_used == stats.words_copied` — the host's new used length.
- **The failure semantics are a hard edge and are documented as one.** Every `new`/`new_checked` failure mutates
  nothing; a `heap_exhausted` (or a `reserve` error) raised from `forward_roots`/`complete` leaves the source
  **partially forwarded** — some header words hold markers, some roots are rewritten — with the destination
  holding the copies made so far, which is **not recoverable**, and the source must not be read afterwards.
  The two sound patterns are therefore `compact` (one root slice, preflighted from the exact live walk, atomic)
  and `live_words_in` over every root slice → `Relocator::new_checked(from, dest, total)` → one `forward_roots`
  per slice → `complete`; `compact`'s atomicity stops at the preflight, so a destination whose `can_reserve`
  lied and refused only at `reserve` carries the same partial-forwarding hazard.
- **`live_words_in` and `for_each_reachable_in` are the public walking helpers, and they are thin wrappers over
  the existing walkers** — `live(from, roots).words` and `for_each_reachable_view` (`../live.rs`,
  `../reachable.rs`) — so there is no second walker to keep in step.
  `live_words_in` is **exactly** the `words_copied` a relocation of the same from-set and roots reports (the walk
  sizes objects by the copier's own `move_size`, counts each once and skips exactly the references the copier
  calls `Foreign`), which is why it is the correct `required_words`; summed over **overlapping** slices it
  over-estimates (a shared object counted twice), the safe direction, since an over-estimate causes only a
  conservative refusal.
  `for_each_reachable_in` reports each reachable **boxed** object once through a safe `ReachableObject` (cons
  cells are traversed but not reported — they carry no header), read-only and iterative, so a foreign arena can
  be inspected immediately before a relocation over the same ranges.

## Test Strategy
- `tests.rs` is the in-module suite: the synthetic `Region` fixture (a plain `Vec<Eterm>` standing in for a
  `ReservedHeap`/arena, with its own `from_set()`/`snapshot()`), `from_ranges`' zero-length skip, sort-by-`lo`
  and geometry equality with the `add(&HeapSpace)` path; `LinearDest`'s classification, reservation and typed
  refusal; `compact` copying exactly the live graph with the roots rewritten, the garbage left behind and the
  counters exact; and the two hard edges — **root-slice idempotence** (`forward_roots` twice on one slice, and
  over two overlapping slices, equals one call, with a shared subterm copied once) and **atomicity**
  (`compact`/`new_checked` refusing an undersized destination with every source word and root term
  byte-identical, and `new` answering `BadArg` for a destination with no `copied_top`).
- The out-of-crate acceptance suite is `../../tests/region_relocation.rs` (11 cases over a two-region grid,
  catalogued in `../../tests/CONTEXT.md`), written out of crate so it can only use the public API.

## Routing Table
- `tests.rs` → the seam's in-module suite (fixture geometry, `LinearDest`, `compact`, idempotence, atomicity,
  the `BadArg` refusal)
- `../relocate.rs` → the driver (`Relocator`, `compact`, `Relocation`, `LinearDest`)
- `../space.rs` → `FromSet` incl. the `unsafe from_ranges` raw-range constructor and `locate`
- `../copy.rs` → `EvacDest`/`Target` and the `pub(crate)` marker primitives
- `../live.rs` → `live`/`live_words_in`, the exact copy volume
- `../reachable.rs` → `for_each_reachable_view`/`for_each_reachable_in`, the read-only walk
- `../../tests/region_relocation.rs` → the acceptance suite (sibling crate-level `tests/` node)
