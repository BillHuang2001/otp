# `beam_gc` — the memory-accounting map

## Intent
This file answers three questions with one map:
what `erlang:memory/0,1` actually sums in this runtime,
which of those figures this crate owns and what it can prove about them,
and what a reporter must never add together twice.
It is written from the reporting side, so the reader is a runtime owner wiring a figure or an agent deciding whether a number disagreeing with RSS is a bug in this crate.
Current state only; measured figures are dated by the artifact or the run they come from.

## The reported total, category by category
Everything below is the runtime in `beam_rt` (`src/context/memory_hook.rs`), the only producer of these nine keys.
`total = processes + system` is an identity the producer preserves by construction, so `total` can never drift from the two top-level parts — but a *part* can drift from the footprint it claims to describe.

| key | what it is | formula today | allocate path | owner |
|---|---|---|---|---|
| `total` | `processes + system` | the identity | — | `beam_rt` |
| `processes` | every live pid's **reserved** heap: nursery-chain capacity + mature capacity | `Σ_pid (HeapObservable::heap_size_words + total_heap_size_words − heap_size_words) × 8` | a process's `beam_alloc::Heap` spaces, over the `Standard` allocator | **this crate** (`HeapFootprint::capacity_bytes`) |
| `processes_used` | every live pid's **occupied** heap | `Σ_pid (young().used_words() + mature_used_words()) × 8` — `young()` is the **current** nursery space only | same | **this crate** (`HeapFootprint::used_bytes`; the `young()` read is the hole — see *Known issues*) |
| `system` | `atom + binary + code + ets + emulator` | the sum of the five below | — | `beam_rt` |
| `atom` / `atom_used` | the one shared atom table: one index word per atom plus the name text | `WORD_BYTES × atoms.len() + Σ name.len()` | `beam_term::AtomTable` | `beam_term` |
| `binary` | **live payload bytes** of the off-heap binaries | `Σ live_bytes` over `AllocType::Binary` | `beam_alloc::RefcBinary` payloads (this crate registers them; `beam_process` owns the roster) | `beam_alloc` registry |
| `code` | each loaded module's `.beam` image size | `Σ fs::metadata(path).len()` over the code server's loaded names | `beam_rt` code server (file paths, not allocations) | `beam_rt` |
| `ets` | the ETS registry's table count, **one index word each** — a slot count, not a footprint | `registry.len() × 8` | `beam_bifs` table registry (`beam_ets` interiors are allocated from `AllocType::Ets`) | `beam_bifs` |
| `emulator` | everything allocated outside a process heap | `Σ live_bytes` over **every** allocator type except `Binary`, `Standard`, `EHeap` | `beam_alloc` registry (`Ets`, `Literal`, `LongLived`, `ShortLived`, `Temporary`, `FixedSize`, `Driver`, `System`) | `beam_alloc` registry |

Three consequences worth stating outright:
`processes` is a **reserved capacity**, so it does not shrink when a mutator goes idle — only a collection (which re-sizes the spaces) moves it.
`emulator` excludes `Standard` because a process's heap spaces come from that allocator here, and `EHeap` because it is the nominal process-heap allocator; counting either would double count a process heap already in `processes`.
`binary` and `ets` are the two keys that are *not* footprints: `binary` is live payload only (carrier retention and headers invisible), `ets` is a count of tables.

## What one process's footprint is — the one-call seam
`HeapFootprint::of(collector, off_heap)` is the single call a reporter makes; it is result-free and allocation-free, and it answers "the bytes this crate accounts for this process's collector plus its off-heap store".
It has three parts, and they are not interchangeable:

1. **Reserved heap capacity** — `capacity_bytes()`, the nursery chain's and mature generation's capacity in words × 8.
   Every byte of it is a live `beam_alloc` heap block, so it is what the `processes` figure charges.
   Its occupancy counterpart is `used_bytes()` (the `processes_used` figure).
   Both are **chain-summed**: a space a `grow_nursery` retired is still counted, because it still holds live words until a collection evacuates the chain.
2. **Live off-heap payload bytes** — `off_heap_payload_bytes()`, the apparent size of every live MSO plus every live refc binary's payload length.
   These bytes are **already** `erlang:memory(binary)`: a reporter that adds this field to `processes` *on top of* `binary` double counts them.
   The two answer different questions ("what does this process hold off-heap" vs "how many off-heap payload bytes exist") and belong side by side, never summed.
3. **The off-heap roster's slot array** — `off_heap_slot_bytes()`, the `Vec` behind the roster whose positions are stable for its whole lifetime, so a released slot keeps its position and its bytes.
   **No allocator type records these bytes**: the `Vec` goes through the process's global allocator, not `beam_alloc`, which is exactly why they need their own field.

`heap_bytes()` is therefore the **allocator-visible** subset (1 + 2) and can never exceed the allocators' own live bytes.
`total_bytes()` is everything this crate accounts for (1 + 2 + 3).
Neither is an RSS estimate — they are reserved capacity plus payload.

## Categories this crate owns
- A process's **heap capacity and occupancy**, across the whole nursery chain and the mature generation (`HeapObservable`, `HeapFootprint`).
- A process's **off-heap roster**: live payload/apparent bytes, live entry count, and the roster's own reserved slot array (`OffHeap::overhead_bytes`, `len`, `slot_count`, `slot_bytes`).
- The **trigger** at which that roster is worth reclaiming (`BinVheap`, the C `bin_vheap_sz`) and the reachability-based reclaim itself (`offheap.rs`, the C `sweep_off_heap`) — both report through, and are not reported by, the figures above.
- Nothing else: this crate is handed a collector and an off-heap roster, so everything below is structurally invisible to it.

## Categories this crate cannot see
- **Process mailboxes and message queues as bytes.** The queue is `beam_message`; a process's `Queue` capacity never enters a footprint.
- **Allocator carrier, mseg and free-block retention.** `beam_alloc::registry::all_stats().live_bytes` is *live payload*, so a carrier that keeps a freed block mapped is invisible to every key — and to this crate.
- **Size-class rounding and allocation headers.** A `RefcBinary` payload of N bytes occupies N + control header rounded to a size class (measured: 4 096 bytes → 4 144 live allocator bytes), so `off_heap_payload_bytes` is a lower bound on the allocation.
- **Any allocation made through the Rust global allocator** — every `Vec`/`HashMap` in the runtime, this crate's own collector index structures, and the roster's slot array (part 3 above is visible *to this crate* only because it measures that array itself).
- **Literal areas and code**, beyond the `.beam` file sizes the `code` key reports.
- **The runtime's own text/data/BSS/heap arenas and the JIT compiled-block cache** (`beam_rt` / `beam_engine`), which is the term that dominates a `beam_rt` RSS.
- **ETS table interiors as a footprint** — only the ×8 slot count is reported.
- **Port/driver buffers and distribution buffers** (`beam_io`, `beam_dist`).

## Known issues
- **`processes_used` drops every retired nursery space.** Reading `collector.young().used_words()` reads the **current** space only, and the nursery is a chain once `grow_nursery` has run, so each retired space contributes 0 occupied words while its live terms are still there.
  `HeapObservable::heap_used_words`/`total_heap_used_words` are the chain-summed readings; the runtime's handoff is in *Handoffs* below.
  `beam_process::Process::heap_used_words()` has the same current-space-only read.
- **`ets` is a slot count, not a footprint**, so a table with a million rows reports the same 8 bytes as an empty one.
- **No key counts the off-heap roster's slot array**, so the bytes in part 3 above are reported by nothing today.

## Why a reported total can fall while RSS rises (the m16 A/B)
`erts_rust/tools/bench/results/m16-ab-{old,new}-r{1,2,3}.json` is a rust-only A/B of two `beam_rt` binaries: `old` = the m15-era runtime at commit `a34ca61332`, `new` = the HEAD of that session.
The metric and the OS figure move in opposite directions on exactly one case, `baseline` (`vm_memory_total_bytes` is probed by a separate `-eval` run, whose peak RSS is not the same process, so the pair is per-case, not per-process):

| side | `vm_memory_total_bytes` | peak RSS of the compute runs |
|---|---|---|
| `old` (r1/r2/r3) | 13 220 989 / 13 222 861 / 13 222 861 | 133 189 632 / 133 246 976 / 133 079 040 |
| `new` (r1/r2/r3) | 6 788 542 / 6 788 542 / 6 788 542 | 171 352 064 / 171 356 160 / 171 294 720 |

Both figures are deterministic per binary (only `processes` wobbles, in whole-heap-size steps of 1 872–6 024 B across repeated runs), so the halving is not noise; every other case in the same A/B either held flat or fell.
The nine keys, read from each binary with `beam_rt -noshell -eval 'erlang:memory().'` (one release build per side, modal value of 20 / 12 runs):

| key | old (m15-era, `a34ca61332`) | new (that session's HEAD) | Δ |
|---|---|---|---|
| `processes` | 6 908 216 | 445 904 | **−6 462 312** |
| `processes_used` | 274 080 | 199 808 | −74 272 |
| `system` | 6 314 645 | 6 342 638 | +27 993 |
| `atom` = `atom_used` | 202 493 | 203 410 | +917 |
| `binary` | 0 | 0 | 0 |
| `code` | 6 025 408 | 6 053 012 | +27 604 |
| `ets` | 152 | 152 | 0 |
| `emulator` (the remainder of `system`) | 86 592 | 86 064 | −528 |
| `total` | 13 222 861 | 6 788 542 | **−6 434 319** |

Both identities close exactly: the `processes` + `system` deltas sum to the `total` delta, and `atom + code + ets + emulator` sums to the `system` delta.

**The category that moved is `processes` — the reserved process-heap capacity, by −6.46 MB of the −6.43 MB total.** Nothing else moved by more than 28 KB.
`system` is not heap-sized by a collection: its two movers are `code` (+27 604 B, a slightly larger loaded-module image set) and `atom` (+917 B, two more interned names), both boot-time facts, and `emulator` was 87 KB on both sides.
The old `processes` was ~96 % reserved capacity around a 0.27 MB live set (6 908 216 reserved vs 274 080 occupied), which is what a collector that does not collect a process down leaves behind; the new side reserves 445 904 B around a 0.20 MB live set.
That is an **accounting** change in the honest direction — the runtime stopped charging itself for heap space a collection had already reclaimed — and it cannot move RSS, because a retired space is handed back to the allocator and whether the OS reclaims the mapping is the allocator's and the OS's business.
So the runtime's reported figure and its RSS are not even measuring the same process (the `vm_memory_total_bytes` probe is its own `-eval` run), and the 5.33× "lower" ratio must not be read as a footprint win.
See `../MEASURED.md` for this crate's own cadence measurements (collection count, copy volume, footprint) rather than the runtime's totals.

## Handoffs
Owned by other nodes — each is a proposal with the exact current call and the exact replacement.

1. **`beam_rt/src/context/memory_hook.rs`, `RuntimeMemory::new`** (the `processes_used` hole).
   Before: `process_used += (collector.young().used_words() + collector.mature_used_words()) as u64 * word;`
   After: `process_used += collector.total_heap_used_words() as u64 * word;`
   Before: `process_bytes += collector.total_heap_size_words() as u64 * word;`
   After: `process_bytes += footprint.capacity_bytes() as u64;` with `let footprint = beam_gc::HeapFootprint::of(collector, process.owned_off_heap());` — one call per process, no formula re-derived at the reporting site.
   The hook needs handoff 3 for `owned_off_heap`; without it, `HeapFootprint::of(collector, &beam_gc::OffHeap::new())` is the same call minus the off-heap halves.
2. **`beam_rt/src/context/memory_hook.rs`, `MemoryHook::memory`** (the uncounted roster array).
   Today `system = atom + binary + self.code + self.ets + emulator`. `emulator` is derived from `all_stats()`; the roster's slot array is in no allocator type, so a reporter wanting it adds `Σ_pid footprint.off_heap_slot_bytes()` as a named term of the `emulator` half — never to `processes` (payload double count) and never to `binary` (it is not a payload).
3. **`beam_process/src/process.rs`** — expose the roster for a reporter:
   `pub fn owned_off_heap(&self) -> &beam_gc::OffHeap { &self.owned_binaries }` (the field already exists; only the accessor is missing), so `HeapFootprint::of` can be called without re-deriving the payload bytes.
4. **`beam_process/src/process.rs`, `Process::heap_used_words`** (the same current-space-only read).
   Before: `self.collector.young().used_words()`
   After: `self.collector.heap_used_words()`
5. **`beam_process/src/process.rs`, the `owned_binary_slots` mirror** — `beam_gc::OffHeap::slot_count()` now exists, so the mirror field and its `debug_assert` can be replaced by `self.owned_binaries.slot_count()`.

## Rules for anyone reporting these figures
- Report **capacity** (`processes`) and **occupancy** (`processes_used`) as separate figures; never present one as evidence about the other.
- Never add `off_heap_payload_bytes` to `processes` while `binary` is also reported.
- Never treat any of these keys as RSS, and never derive a footprint from a delta in a reported key — the m16 A/B above is the worked example of a key that halved while the footprint rose.
- A figure with no counterpart in this runtime stays an honest 0; an invented number is worse than a missing key.

## See Also
- `../CONTEXT.md` → the crate seam shape (`HeapFootprint`, `HeapObservable`, `OffHeap`) and `MEASURED.md` headlines.
- `../MEASURED.md` → this crate's measured cadence, copy volume, footprint and off-heap retention ladders.
- `../../beam_rt/src/context/memory_hook.rs` → the only producer of the nine keys (read-only here).
- `../../beam_alloc/src/registry.rs` → `all_stats()`, the allocator-type live bytes behind `binary` and `emulator`.
- `../../../tools/bench/RESULTS.md` → the m16 session record and every load caveat that travels with it.
