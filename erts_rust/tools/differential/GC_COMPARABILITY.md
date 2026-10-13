# `gc` corpus group — oracle comparability

Scope: the `gc/*` cases in `src/corpus/gc.rs` (contracts A.1 + A.7), the differential harness's
coverage of the per-process garbage collector.
The collector itself is internal, so every case pins the collector's **observable
consequences** — the values it must not change, the shapes of the fields it owns, and the return
value of the BIF that drives it — and never a quantity that only a *particular* collector
implementation produces.
`differential run gc/<id>` prints `[AGREE]` for every case in the group, under the default
baseline-JIT tier **and** under `BEAM_RT_TIER=interpreter`.

## Pinned — and why it is oracle-comparable

The oracle is the system `erl` (OTP 29.1.1 / erts-17.1); a pinned golden is that VM's
**single-line stdout** for the wrapped expression.
`Probe::TermToBinary` is the default: the canonical `term_to_binary/1` image of the result is
order-stable, so a map's `~w` key order can never leak into a golden.
Forcing a collection with `erlang:garbage_collect/1` and then observing the surviving term is the
cleanest possible invariant check: the expression is pure, so *any* difference in the answer is a
collector bug (a dropped, duplicated, mis-forwarded or mis-sized live term), not an environment
difference.
The `erlang:garbage_collect/1` and `/0` return value (`true`) is an oracle value — both runtimes
answer it, so it is pinned directly (A.7).
`lists:sum/1`, `hd/1`, `element/2`, `maps:size/1`, `maps:get/2`, `byte_size/1`, `binary:at/2` and a
`=:=` against an independently rebuilt value are all pure consumers of the surviving term, so each
one reduces "the collector preserved this term" to a fixed **integer / boolean / atom**.
Sharing and identity are pinned by holding one term twice (a list, a refc binary, a reference, a
pid, a fun) and comparing the two elements — a copier that ever made two copies of one live subterm
would answer `false` where the oracle answers `true`.
The interior shapes the copier must move are covered individually: a cons list, a nested
tuple/list/map/binary, a 16-key integer flatmap (its canonical `MAP_EXT` image is pinned
byte-exactly), a 40-key integer map (the HAMT interior, pinned by `maps:size/1`), an off-heap refc
binary, a sub-binary window (`binary:part/3`), a bignum and a fun closure.
Collector-stressing programs whose *result* is a stable integer are pinned (a fold that allocates
and discards 500 inner lists, 2 000 allocation-heavy comprehension rounds, a self-recursive named
fun, a 200 000-element range summed to a bignum, and a spawned process's 50 000-element sum
received by the parent) — they exercise many collections end to end while keeping the golden exact.
`erlang:process_info(self(), memory)` is pinned by **shape** only: its key is the atom `memory`,
its value is a positive integer.
The `erlang:process_info(self(), garbage_collection)` proplist is pinned by **structure** only: the
documented key set (`fullsweep_after`, `max_heap_size`, `min_bin_vheap_size`, `min_heap_size`,
`minor_gcs`), the membership of `min_heap_size`, the integer *type* of its fields and the fact that
`max_heap_size` is an option map.
The five `gc/module_*` cases drive the same properties through **compiled `erlc` bytecode** and the
runtime's `run` action, where allocation and collection are driven by the engine rather than by the
`-eval` tree-walker; a module-run golden is the integer the function returns.

## Deliberately NOT pinned — internal counter, invariant, or capability gap

`erlang:process_info(self(), memory)`'s **byte total** is not pinned (the oracle answers `21616`
where the Rust runtime answers `3728`): it is a per-runtime heap-accounting figure, i.e. an
internal counter, not an oracle value.
`erlang:process_info(self(), garbage_collection)`'s **counter and limit values** (`minor_gcs`,
`min_heap_size`, `fullsweep_after`, `min_bin_vheap_size`) are not pinned: `minor_gcs` is live and
the limits are per-runtime allocator policy, so only the key set, the field types and `max_heap_size`
being a map are.
The `max_heap_size` option **map image** is not pinned either, and this one is a **divergence, not a
volatility**: `term_to_binary/1` of that map orders its atom keys differently in the two runtimes
(the oracle emits `error_logger, include_shared_binaries, kill, size`; the Rust runtime emits
`kill, error_logger, include_shared_binaries, size`), so pinning the image would be a `mismatch`.
It is the multi-key atom-keyed map divergence recorded in `divergences.md` (A.5).
The same divergence reproduces on the smallest possible term
(`term_to_binary(#{b => 2, a => 1})`), so it is a property of the runtime's ETF encoder, not of the
collector; it is left out of this group for that reason (the `wire` group pins no multi-key
atom-keyed image — only single-key ones and float/list/binary-keyed images).
`erlang:garbage_collect/2` (e.g. `erlang:garbage_collect(self(), [{type, minor}])`) is an **honest
Rust capability gap** — the CLI exits `3` with
`beam_rt: unsupported: erlang:garbage_collect/2 is a known BIF that is not implemented` — so it must
not enter the corpus until it agrees: an un-allowlisted gap makes `gate.sh` exit `3`.
`erlang:process_info(self(), binary)` is a second capability gap for the same reason (the Rust
runtime exits `3` with `beam_rt: unsupported: process_info: binary …`), so it is left out as well.
`erlang:garbage_collect/1`'s **reduction cost**, the nursery/mature sizes and any GC trace output are
not pinned: none of them is observable through the black-box CLI contract the harness uses.
The `gc/*` cases therefore assert nothing about *how* the collector works — only that every value it
is handed comes back unchanged and that the documented `process_info/2` shapes are intact.
