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
`Probe::TermToBinary` is the default: the `term_to_binary/1` image of every term this group builds is order-stable — the group's maps are integer-keyed, and integer keys have a single canonical order — so a map's `~w` key order can never leak into a golden.
Forcing a collection with `erlang:garbage_collect/1` or `/2` and then observing the surviving term
is the cleanest possible invariant check: the expression is pure, so *any* difference in the answer
is a collector bug (a dropped, duplicated, mis-forwarded or mis-sized live term), not an environment
difference.
The `erlang:garbage_collect` return value (`true`) is an oracle value for every arity — `/0`, `/1`
and the `/2` option form (`[]`, `[{type, minor}]`, `[{type, major}]`) — so it is pinned directly
(A.7), together with the `/2` form's `error:badarg` refusal of an unknown `type`.
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
`erlang:process_info(self(), binary)` is pinned by **shape** only: the item is tagged `binary`, its
value is a list, and every element is a three-integer tuple `{Ref, Size, Refc}`.
The five `gc/module_*` cases drive the same properties through **compiled `erlc` bytecode** and the
runtime's `run` action, where allocation and collection are driven by the engine rather than by the
`-eval` tree-walker; a module-run golden is the integer the function returns.

## Deliberately NOT pinned — internal counter, invariant, or capability gap

`erlang:process_info(self(), memory)`'s **byte total** is not pinned (measured at this revision: the oracle answers `13712` where the Rust runtime answers `1864`, each stable over repeated runs): it is a per-runtime heap-accounting figure, i.e. an internal counter, not an oracle value.
`erlang:process_info(self(), garbage_collection)`'s **counter and limit values** (`minor_gcs`, `min_heap_size`, `fullsweep_after`, `min_bin_vheap_size`) are not pinned: `minor_gcs` is live and the limits are per-runtime allocator policy, so only the key set, the field types and `max_heap_size` being a map are.
The `max_heap_size` option **map image** is not pinned as a plain `term_to_binary/1` image: the two runtimes order its atom keys differently (the oracle emits `error_logger, include_shared_binaries, kill, size` in 6 of 6 runs, the Rust runtime `kill, error_logger, include_shared_binaries, size` in 6 of 6), so a golden on it would be a `mismatch`.
That ordering is the multi-key atom-keyed map interior recorded in `divergences.md` (A.5), so it is a property of the runtime's ETF encoder rather than of the collector, and it reproduces on the smallest possible term: the plain image of `#{b => 2, a => 1}` comes out `b, a` in 5 of 8 oracle launches and `a, b` in the other 3, because the oracle's own order is not stable across `-eval` VM starts.
The `max_heap_size` map's **`[deterministic]`** image IS pinnable: `term_to_binary/2`'s `[deterministic]` sorts the keys into canonical term order on both runtimes, and it agrees for that map and for `#{b => 2, a => 1}` (the `a`, `b` image, 6 of 6 oracle runs and 4 of 4 here).
The `wire`/`wire_extra` groups pin no multi-key atom-keyed image at all: they carry the single-key atom-keyed images (where the order is vacuous), the order-stable float/list/binary-keyed multi-key images (`wire/map_ext_canonical_key_order_float_list_binary`, `wire/map_ext_binary_key_order`), and multi-key maps otherwise only through order-free relations (`maps:size/1`, a sorted `maps:to_list/1`, and `=:=` against a recomposed term).
`erlang:process_info(self(), binary)` **is** served by both runtimes and is oracle-compared by its **shape** (`gc/process_info_binary_item_shape`): the item is tagged `binary`, its value is a list, and every element is a three-integer tuple `{Ref, Size, Refc}` (vacuously true for the Rust runtime's empty list).
Its **entry list** is not pinned, because each entry is `{Ref, Size, Refc}` carrying a per-runtime heap **address** — measured at this revision, the oracle lists 2 entries for a plain `-eval` process and 5 (the 100 000-byte one included) after `binary:copy(<<1>>, 100000)`, while the Rust runtime answers `{binary,[]}` in both shapes — so only the tag, the list type and the element shape are comparable.
`erlang:garbage_collect/1`'s **reduction cost**, the nursery/mature sizes and any GC trace output are
not pinned: none of them is observable through the black-box CLI contract the harness uses.
The `gc/*` cases therefore assert nothing about *how* the collector works — only that every value it
is handed comes back unchanged and that the documented `process_info/2` shapes are intact.
