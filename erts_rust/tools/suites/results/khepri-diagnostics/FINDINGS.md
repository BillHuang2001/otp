# Khepri divergence classes RC-A…RC-H — current state

Measurement/diagnosis only: nothing in this directory changes a runtime crate, and `/home/bill/Source/khepri` is never written (the staged copy `$TMPDIR/erts-suites-stage/khepri/proj` is used read-only).
Every row below is re-measured with the committed probes against the current HEAD runtime; the sweep row table is `results/khepri/report.json` (+ `summary.md`).
Verdict vocabulary: `fixed` · `still present` · `changed class` (the same class, a different surviving shape).

## Environment and method

| item | value |
|---|---|
| sweep recorded here | `results/khepri/{report.json,summary.md}` — runtime commit `8e8e0ad5d96a11931ff6972779a3b5adbe90bfca`, tier `default` |
| rust runtime | `erts_rust/target/release/beam_rt` (release, same commit), `system_info(version)` → `30/17.1` |
| oracle | system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` (the tree pins erts `17.1`, so the oracle's patch differs by design) |
| khepri | `2ac09291bac7`, staged `rebar3 as test compile` **reused**, not rebuilt |
| code path (both sides, identical) | 11 `-pa` entries: `aten cf cth_readable gen_batch_server horus khepri meck proper ra seshat` `*/ebin` + `_build/test/lib/khepri/test` |
| drive (identical both sides) | `eunit:test('<module>',[verbose]), halt(0).` |
| probes | `erlc -o $TMPDIR/kdiag/beams results/khepri-diagnostics/*.erl` (oracle `erlc`), then each side in its **own** throwaway cwd: `<runtime> -noshell -pa <beams> <khepri -pa…> -eval '<probe>:main(), halt(0).'` |
| host | 16 logical CPUs, **shared**; the loadavg columns in `report.json` are the honest unit |

Only counts and same-host paired ratios are portable; the millisecond columns are host observations, never benchmark claims.
The system `erl` prints **nothing** when a `-eval` expression raises (it exits 1 silently), so every probe wraps each expression in `try … catch`; a probe row that dies without a line is a crash, not a harness gap.

## Verdicts

| # | Check (smallest expression) | Oracle (`erl`) | Rust (`beam_rt`) | Verdict |
|---|---|---|---|---|
| RC-A | `file:write(Fd, [<<"ab">> \| <<"cd">>])`, both `pwrite` shapes, `dets` lifecycle | `ok` | `ok` | **fixed** |
| RC-A | md5 of `raw_probe.dat` / `dets_probe.dets` (12 / 5 933 bytes) | `827f51cc…` / `d9259f29…` | same two md5s | **fixed** |
| RC-B | `process_flag(min_heap_size, 233)` / `min_bin_vheap_size, 46422` / `fullsweep_after, 0` | `233` / `46422` / `65535` | same | **fixed** |
| RC-B | `spawn_opt(Fun,[{min_heap_size,233}])` / `[{fullsweep_after,0}]` / `[{min_bin_vheap_size,46422}]` | `233` / `0` / `46422` | same | **fixed** |
| RC-B | `ra_log_wal:init/1` flag preface (`process_info(self())`, one spawned process) | `[{trap_exit,false},{message_queue_data,on_heap},{min_bin_vheap_size,46422},{min_heap_size,233}]` | identical | **fixed** |
| RC-B | `process_info(self(), trap_exit)` in the **`-eval` process** | `{trap_exit,true}` | `{trap_exit,false}` | **still present** |
| RC-B | `process_flag(trap_exit, true)` in the `-eval` process | `true` | `false` | **still present** |
| RC-C | `erlang:external_size(fun() -> ok end)` / in a tuple / list / map | `87` / `95` / `93` / `95` | same | **fixed** |
| RC-C | `term_to_binary` of the same single-fun shapes (incl. a named `fun Fact/1`) | `95` / `89` / `92` / `93` / `95` | same | **fixed** |
| RC-C | `term_to_binary({F1, F2})` / `external_size({F1, F2})` with **two different** lambdas | `175` / `175` | `beam_rt: unsupported: local_fun` (exit 3) | **still present** (multi-closure terms only) |
| RC-C | a closure built by `erl_eval:exprs/2` | `138` | `138` | agree |
| RC-D | 2 500 keep-while dependents, one `#delete{path = [target]}` | 27/27/28/27 ms | 5 688/5 677/5 650/5 749 ms | **still present** (≈210×) |
| RC-D | that delete's `?assert(DeletionTime < 1000)` (`test/keep_while_conditions.erl`) | passes | **fails** (`keep_while_conditions` 26/1) | **still present** |
| **RC-E** | `eunit:test('simple_get',[verbose])` in the `-eval` process | `All 94 tests passed` | `All 94 tests passed` | **fixed** (the nested-run pump now waits out peer timers) |
| **RC-F** | `eunit:test('advanced_tx_delete',[verbose])` | 39 passed / 0 failed | 39 passed / 0 failed | **fixed** (the stale compiled match spec no longer dereferences a freed word) |
| **RC-G** | `binary:part(B, 0, byte_size(B) - 1)` in `seshat:labels_to_binary/1` during `ra_log_wal` startup (`bang_functions`) | 85 passed / 0 failed | `badarg` (exit 3, 24 passed first) | **new** (a `binary:part/3` edge shape) |
| **RC-H** | `crypto:strong_rand_bytes(16)` in `projection_with_custom_horus_options_test_` (`projections`) | 16 random bytes | `{nif_not_loaded,module,crypto,…}` | **new** (the crypto NIF is not loaded) |

## RC-A — fixed (iodata walk + on-disk bytes)

`iodata_probe.erl` prints `ok` / `ok` / `ok` / `ok` / `{ok, iodata_probe_tab}` on both runtimes.
`dets_probe.erl` extends that to the bytes: the raw writes produce a 12-byte `raw_probe.dat` with the same md5 on both sides, and `dets:open_file/insert/sync/lookup/info/close/reopen` produces a 5 933-byte `.dets` file with the same md5 on both sides.

## RC-B — the three heap-sizing flags are fixed; the `-eval` process's birth `trap_exit` is still present

The fix in `crates/beam_rt/src/context/process_bridge.rs` serves `min_heap_size`, `min_bin_vheap_size` and `fullsweep_after` (old-value return; the 233 / 46 422 floors; no floor for `fullsweep_after`), and it is **bookkeeping only** — the values are recorded, not yet fed to a GC-policy input.
The surviving shape is the `-eval` process's *birth* flag: on the oracle that process is born with `trap_exit = true`, on this runtime with the spawn default `false`, so `process_flag(trap_exit, true)` answers `false` (a plain `spawn/1` process agrees).
Owning crate: **`beam_rt`** — the CLI/`-eval` bootstrap path under `crates/beam_rt/src/eval*/` never sets `trap_exit` on the evaluator process.

## RC-C — single-closure encodings are fixed; multi-closure terms and a walker-made closure are still present

Every single-fun shape encodes byte-identically (bare fun, one fun inside a tuple/list/map, a named `fun Fact/1`, and a closure built by `erl_eval:exprs/2`), and `display_tree` agrees 11/0 in the sweep.
The two surviving shapes are (a) a term containing **two different** lambdas — `term_to_binary({F1, F2})` and `external_size({F1, F2})` answer `175` on the oracle and `beam_rt: unsupported: local_fun` here, because the encoder carries **one** closure identity rather than one per fun — and (b) a closure created by the `-eval` tree-walker itself, which is not a compiled module fun at all.
Owning crates: **`beam_bifs`** (`crates/beam_bifs/src/binconv.rs::term_encoder`) over **`beam_codec`**'s `Encoder` (a single `FunIdentity` slot); for (b) the seam is `crates/beam_rt/src/eval/`.

## RC-D — still present (keep-while delete fan-out, performance)

`keep_while_fanout_probe:main/0` builds 2 500 dependents on one node and deletes it; measured ABBA (`sys,rust,rust,sys` ×2) so the load drift is paired out.

| scenario | oracle | rust | ratio |
|---|---|---|---|
| `insert` of the 2 500 dependents | 9–12 ms | ≈2 139–2 197 ms | ≈200× |
| `delete` of the parent (2 500 dependents) | 27/27/28/27 ms (10–11 µs each) | 5 688/5 677/5 650/5 749 ms (2 260–2 299 µs each) | **≈210×** |
| `deleted_count` | 2 502 | 2 502 | agree |
| 100-node dependency chain | 3/2/2/2 ms | 47/46/47/48 ms | ≈23× |
| the suite's `?assert(DeletionTime < 1000)` | passes | **fails** (`keep_while_conditions` reads 26/1 vs the oracle's 27/0) | — |

The cost is per-dependent work in the keep-while sweep, not the delete's result set; the owning crate is **`beam_ets`** (the `keep_while` condition index walk) with the engine tier as a multiplier — the shape is a full scan per dependent, so the fix must be an index lookup, not a faster loop.

## RC-E — the nested-run pump wall is fixed (was 20 of the 21 rust-gap rows)

The wall was `opcode: run: process blocked (no message available)` — a typed `beam_rt` capability gap, not a wrong result — emitted when the pump folded a round to `blocked()` after `ran == 0` instead of waiting out the earliest peer timer.
The fix (`beam_rt: wait out peer timers in the nested-run pump`) treats a wake of the run target as progress and waits out the earliest deadline once (bounded) before folding, so the `-eval` process's nested machine now advances.
Consequence in the new sweep: every previously-blocked module runs to completion — the 19 former RC-E rows (`advanced_delete`, `advanced_put`, `advanced_tx_get`, `advanced_tx_put`, `async_option`, `bang_functions`, `db_info`, `export`, `export_erlang`, `favor_option`, `projections`, `return_indirect_deletes_option`, `simple_get`, `simple_put`, `simple_tx_delete`, `simple_tx_get`, `simple_tx_misc`, `simple_tx_put`, `triggers`) plus `advanced_get`, `machine_code_called_from_ra`, `simple_delete` all now report counts, and `stored_procs` now runs and fails **identically** on both sides (12/1, the oracle's own `crashing_sproc_stacktrace_test` failure) — it moved out of the RC-E block into `agree-fail-identical`.
Two of the formerly-blocked modules now expose a **later** wall, recorded as RC-G (`bang_functions`) and RC-H (`projections`) below.
What to run to see it: `block_wall_probe:eunit_spawned(simple_get)` (green before) and `block_wall_probe:eunit_here(simple_get)` (now also green) with the khepri `-pa` set.

## RC-F — `advanced_tx_delete` no longer dies by SIGSEGV (fixed)

The row's rust status was `error` (SIGSEGV, exit 139) on a stale compiled match spec: `ets:match_spec_compile/1` stored the raw spec term as a bare `Eterm` aliasing the compiling process's heap, and `meck` ran it much later, dereferencing freed words through `beam_ets::select::list_elements`'s header-word-aliasing list check.
Both seams landed: `beam_bifs` deep-copies the compiled spec (`d9c5263cb6`, `insert_spec`/`spec_raw` no longer store a caller-heap alias), and `beam_ets::select::list_elements` requires a non-nil **list word** so a bad term is `badarg`, never a segfault.
The new sweep reads `advanced_tx_delete` **39/0 agree** (was `error` with no parseable summary).

## RC-G — `bang_functions`: `binary:part/3` raises `badarg` during `ra_log_wal` startup (new)

Surfaced by the RC-E fix: `bang_functions` now runs past the pump and its rust cell becomes `cancelled` (24 of 85 tests pass, then eunit cancels the rest).
First divergence: `seshat:labels_to_binary/1` (`_build/default/lib/seshat/src/seshat.erl:386`) calls `binary:part(LabelsBin0, 0, byte_size(LabelsBin0) - 1)` on the binary built by `maps:fold` appending `<<"k=\"v\",">>` segments, and that `binary:part/3` raises `badarg` — reached from `seshat:register_counter/5` → `seshat:new_counter/5` while the `ra_log_wal` child of `ra_system` starts up, so `helpers:start_ra_system/1` throws `{failed_to_start_child,ra_log_wal,{badarg,…}}`.
The canonical `binary:part(<<"z",(binary:copy(<<255>>,1024))/binary>>,1,510)` *does* agree, so this is a narrower shape than the D2 sub-binary window; the exact offending label term is not yet minimized to a one-liner (the repro is the module run itself).
Owning crate: **`beam_bifs`** (`binary:part/3`).

## RC-H — `projections`: the crypto NIF is not loaded (new)

`projections:1146` `projection_with_custom_horus_options_test_` fails the `?assertMatch` "The projection contains the expected record": the projection function calls `crypto:strong_rand_bytes/1` (via the custom `horus` options), which raises `{nif_not_loaded,module,crypto,…}` here (the oracle returns 16 random bytes).
This is the known crypto-seam gap (the `crypto.so` load path is not wired into this runtime), not a new runtime defect; it is the first khepri module whose projection path needs `crypto`.
Owning crate: **`beam_bifs`**/`beam_abi` (the crypto NIF load seam).

## Side observations (not RC classes, reported rather than acted on)

1. **Small-map key order can differ** (`maps:to_list/1`, `maps:keys/1`, and therefore `~p` of an atom-keyed map): the previous-value map of `process_flag(max_heap_size, #{size => 0})` prints `#{error_logger,… , include_shared_binaries,… , kill,… , size,…}` on the oracle and `#{kill,… , error_logger,… , include_shared_binaries,… , size,…}` here.
   Map iteration order is undefined by the language and both runtimes order small maps by atom *index*, so the difference comes from the atom-table insertion order each runtime assigns while loading a module's atoms, not from map code (owning crate: `beam_term`).
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
| `block_wall_probe.erl` | RC-E | `… <khepri -pa…> -eval 'block_wall_probe:eunit_here(simple_get), halt(0).'` vs `eunit_spawned(simple_get)`; also `main/0`, `leak/1`, `census/0`, `eunit_gen/1` |
| `block_wall_khepri.erl` | RC-E | `… <khepri -pa…> -eval 'block_wall_khepri:fixtures(12), halt(0).'` (stderr checkpoints `ck/2`) |
| `block_wall_fixture.erl` | RC-E | 30 self-contained spawn/receive fixtures, `… -eval 'block_wall_fixture:main(), halt(0).'` |
| — | RC-G | no probe: the repro is the module run itself (`eunit:test('bang_functions',[verbose])`), `seshat:labels_to_binary/1` at `seshat.erl:386` |
| — | RC-H | `-eval 'catch crypto:strong_rand_bytes(16)'` → `{nif_not_loaded,module,crypto,…}` |
