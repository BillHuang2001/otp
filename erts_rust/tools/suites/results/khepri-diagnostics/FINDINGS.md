# Khepri divergence classes RC-A…RC-H — current state

Measurement/diagnosis only: nothing in this directory changes a runtime crate, and `/home/bill/Source/khepri` is never written (the staged copy `$TMPDIR/erts-suites-stage/khepri/proj` is used read-only).
Every row below is re-measured against the current HEAD runtime; the sweep row table is `results/khepri/report.json` (+ `summary.md`).
Verdict vocabulary: `fixed` · `still present` · `changed class` (the same class, a different surviving shape).

## Environment and method

| item | value |
|---|---|
| sweep recorded here | `results/khepri/{report.json,summary.md}` — runtime commit `e98a5e1e231706a13b103f8c18c218ecb8d0d212`, tier `default` |
| rust runtime | `erts_rust/target/release/beam_rt` (release, same commit), `system_info(version)` → `30/17.1` |
| oracle | system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` (the tree pins erts `17.1`, so the oracle's patch differs by design) |
| khepri | `2ac09291bac7`, staged `rebar3 as test compile` **reused**, not rebuilt |
| code path (both sides, identical) | 11 `-pa` entries: `aten cf cth_readable gen_batch_server horus khepri meck proper ra seshat` `*/ebin` + `_build/test/lib/khepri/test` |
| drive (identical both sides) | `eunit:test('<module>',[verbose]), halt(0).` |
| probes | `erlc -o $TMPDIR/kdiag/beams results/khepri-diagnostics/*.erl` (oracle `erlc`), then each side in its **own** throwaway cwd: `<runtime> -noshell -pa <beams> <khepri -pa…> -eval '<probe>:main(), halt(0).'` |
| host | 16 logical CPUs, **shared**; the loadavg columns in `report.json` are the honest unit |

Only counts and same-host paired ratios are portable; the millisecond columns are host observations, never benchmark claims.
The system `erl` prints **nothing** when a `-eval` expression raises (it exits 1 silently), so every probe wraps each expression in `try … catch`; a probe row that dies without a line is a crash, not a harness gap.

## Headline

| tally | prior (`8e8e0ad5d`) | now (`e98a5e1e2`) |
|---|---|---|
| sys | 1500 / 1 | 1500 / 1 |
| rust | 1437 / 3 | **1497 / 4** |
| agree / divergence / rust-gap | 39 / 2 / 1 | **40 / 2 / 0** |

`bang_functions` moved from `rust-gap` (RC-G) to **agree** (`85/0` both sides).
`projections` moved from `133/1` (RC-H `nif_not_loaded`) to `132/2` (RC-H new shape, below).
`keep_while_conditions` is unchanged at `26/1` (RC-D, performance).

## Verdicts

| # | Check (smallest expression) | Oracle (`erl`) | Rust (`beam_rt`) | Verdict |
|---|---|---|---|---|
| RC-A | `file:write(Fd, [<<"ab">> \| <<"cd">>])`, both `pwrite` shapes, `dets` lifecycle | `ok` | `ok` | **fixed** |
| RC-A | md5 of `raw_probe.dat` / `dets_probe.dets` | `827f51cc…` / `d9259f29…` | same two md5s | **fixed** |
| RC-B | `process_flag(min_heap_size, 233)` / `min_bin_vheap_size, 46422` / `fullsweep_after, 0` | `233` / `46422` / `65535` | same | **fixed** |
| RC-B | `process_info(self(), trap_exit)` in the **`-eval` process** | `{trap_exit,true}` | `{trap_exit,false}` | **still present** (side observation, no sweep row) |
| RC-C | `term_to_binary({F1, F2})` / `external_size({F1, F2})` with **two different** lambdas | `175` / `175` | `beam_rt: unsupported: local_fun` (exit 3) | **still present** (side observation, no sweep row) |
| **RC-D** | 2 500 keep-while dependents, one `#delete{path = [target]}` | 27/27/28/27 ms | 5 688/5 677/5 650/5 749 ms | **still present** (≈210×, performance) |
| **RC-E** | `eunit:test('simple_get',[verbose])` in the `-eval` process | `All 94 tests passed` | `All 94 tests passed` | **fixed** |
| **RC-F** | `eunit:test('advanced_tx_delete',[verbose])` | 39/0 | 39/0 | **fixed** |
| **RC-G** | `binary:part(B, 0, byte_size(B) - 1)` in `seshat:labels_to_binary/1` (`bang_functions`) | 85 passed / 0 failed | 85 passed / 0 failed | **fixed** |
| **RC-H** | `crypto:strong_rand_bytes(16)` / `crypto:hash(sha, …)` | 16 random bytes / digest | 16 random bytes / digest | **fixed** (the `nif_not_loaded` shape is gone) |
| **RC-H** | `projections` `projection_with_custom_horus_options_test_` (Register the projection) | `134/0` | `132/2` | **changed class** — `throw:{horus,call_to_unexported_function,#{mfa => {crypto,strong_rand_bytes,1}}}` |

## RC-D — still present (keep-while delete fan-out, performance)

`keep_while_fanout_probe:main/0` builds 2 500 dependents on one node and deletes it; measured ABBA (`sys,rust,rust,sys` ×2) so the load drift is paired out.

| scenario | oracle | rust | ratio |
|---|---|---|---|
| `delete` of the parent (2 500 dependents) | 27/27/28/27 ms | 5 688/5 677/5 650/5 749 ms | **≈210×** |
| `deleted_count` | 2 502 | 2 502 | agree |
| the suite's `?assert(DeletionTime < 1000)` | passes | **fails** (`keep_while_conditions` reads 26/1 vs the oracle's 27/0) | — |

The cost is per-dependent work in the keep-while sweep, not the delete's result set; the owning crate is **`beam_ets`** (the `keep_while` condition index walk) with the engine tier as a multiplier — the shape is a full scan per dependent, so the fix must be an index lookup, not a faster loop.
This is the only khepri row that fails on the current runtime **for a result**, and it is performance, not correctness: both sides agree on `deleted_count`.

## RC-G — `bang_functions` now agrees (fixed)

The `binary:part/3` `badarg` that cut `bang_functions` short during `ra_log_wal` startup is gone: the sweep now reads `bang_functions` **85/0 on both sides** (was `cancelled` at 24/85).
The canonical `binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510)` agrees, and the `seshat:labels_to_binary/1` shape that used to raise is no longer reachable as a wrong result.
Owning crate: **`beam_bifs`** (`binary:part/3` boundary shape).

## RC-H — the crypto NIF loads; `projections` survives with a new shape

The `nif_not_loaded` wall is gone: `crypto:strong_rand_bytes(16)` returns 16 random bytes and `crypto:hash(sha, …)` returns the oracle digest on this runtime (the `-on_load` module hook now loads the crypto NIF), and the isolated sequence `code:ensure_loaded(crypto), erlang:function_exported(crypto, strong_rand_bytes, 1)` answers `true` on both runtimes.

The surviving shape is earlier in `projections`: at the "Register the projection" step
(`projections:1130: -projection_with_custom_horus_options_test_/0-fun-7-`) horus throws
`{horus, call_to_unexported_function, #{mfa => {crypto, strong_rand_bytes, 1}}}`.
Horus's `ensure_function_is_exported/4` (`horus.erl:2850`) decides a called function is exported by `code:ensure_loaded(Module)` then `erlang:function_exported(Module, Name, Arity)`; in the suite's load order that second call answers `false` for `crypto:strong_rand_bytes/1` where the oracle answers `true`, even though the direct call and the isolated probe agree.
Owning seam: **`beam_rt`/`beam_bifs`** — the crypto `-on_load`/NIF export registration is not yet visible to `erlang:function_exported/3` at the point horus introspects during suite startup (the module is loaded but the NIF stub is not registered as exported in that order).

## Side observations (not RC classes, reported rather than acted on)

1. **Small-map key order can differ** (`maps:to_list/1`, `maps:keys/1`, and therefore `~p` of an atom-keyed map): map iteration order is undefined by the language and both runtimes order small maps by atom *index*, so the difference comes from the atom-table insertion order each runtime assigns while loading a module's atoms (owning crate: `beam_term`).
2. The `file:open/2` port descriptor map differs in shape (`owner`/`fd_registry_id` keys, `#Ref` values) — an internal representation; every *documented* observable agrees.
3. The system `erl` exits 1 **silently** when a `-eval` expression raises, so a probe that dies mid-run without a line is an escape, not a missing observation; probes here catch their own expressions.

## Committed probes

| file | class | run line |
|---|---|---|
| `iodata_probe.erl` | RC-A | `… -eval 'iodata_probe:main(), halt(0).'` |
| `dets_probe.erl` | RC-A bytes | same form, own cwd each side, then `md5sum raw_probe.dat dets_probe.dets` |
| `process_flag_probe.erl` | RC-B flags | `… -eval 'process_flag_probe:main(), halt(0).'` |
| `trap_exit_probe.erl` | RC-B `trap_exit`/`spawn_opt` | `… -eval 'trap_exit_probe:main(), halt(0).'` |
| `ra_wall_probe.erl` | RC-B in situ | `… <khepri -pa…> -pa <beams> -eval 'ra_wall_probe:main(), halt(0).'` |
| `external_size_probe.erl` | RC-C | `… -eval 'external_size_probe:main(), halt(0).'` |
| `keep_while_fanout_probe.erl` | RC-D | `erlc -DTEST -I <staged khepri proj> -o <beams> …`; `… <khepri -pa…> -eval 'keep_while_fanout_probe:main(), halt(0).'` (also `scaling/0`) |
| `block_wall_probe.erl` | RC-E | `… <khepri -pa…> -eval 'block_wall_probe:eunit_here(simple_get), halt(0).'` vs `eunit_spawned(simple_get)` |
| `block_wall_khepri.erl` | RC-E | `… <khepri -pa…> -eval 'block_wall_khepri:fixtures(12), halt(0).'` |
| `block_wall_fixture.erl` | RC-E | 30 self-contained spawn/receive fixtures, `… -eval 'block_wall_fixture:main(), halt(0).'` |
| — | RC-H | `-eval 'code:ensure_loaded(crypto), io:format("~p",[erlang:function_exported(crypto, strong_rand_bytes, 1)])'` → `true` (isolated); the suite's horus introspection order is the failing path |
