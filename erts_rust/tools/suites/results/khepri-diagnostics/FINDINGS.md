# Khepri divergence classes RC-A…RC-F — current state

Measurement/diagnosis only: nothing in this directory changes a runtime crate, and `/home/bill/Source/khepri` is never written (the staged copy `$TMPDIR/erts-suites-stage/khepri/proj` is used read-only).
Every row below is re-measured with the committed probes; the sweep row table is `results/khepri/report.json` (+ `summary.md`).
Verdict vocabulary: `fixed` · `still present` · `changed class` (the same class, a different surviving shape).

## Environment and method

| item | value |
|---|---|
| sweep recorded here | `results/khepri/{report.json,summary.md}` — runtime commit `71ffbbb37971d0b193e0dc74b60b6c448874349e`, tier `default` |
| rust runtime | `erts_rust/target/release/beam_rt` (release, same commit), `system_info(version)` → `30/17.1` |
| oracle | system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` (the tree pins erts `17.1`, so the oracle's patch differs by design) |
| khepri | `2ac09291bac7`, staged `rebar3 as test compile` **reused**, not rebuilt |
| code path (both sides, identical) | 11 `-pa` entries: `aten cf cth_readable gen_batch_server horus khepri meck proper ra seshat` `*/ebin` + `_build/test/lib/khepri/test` |
| drive (identical both sides) | `eunit:test('<module>',[verbose]), halt(0).` |
| probes | `erlc -o $TMPDIR/kdiag/beams results/khepri-diagnostics/*.erl` (oracle `erlc`), then each side in its **own** throwaway cwd: `<runtime> -noshell -pa <beams> <khepri -pa…> -eval '<probe>:main(), halt(0).'` |
| host | 16 logical CPUs, **shared**; 1-min loadavg observed 10.79–128 during this session (RC-D measured ABBA-ordered in a 10.8–15.1 window) |

Only counts and same-host paired ratios are portable; the millisecond columns are host observations, never benchmark claims.
The system `erl` prints **nothing** when a `-eval` expression raises (it exits 1 silently), so every probe wraps each expression in `try … catch`; a probe row that dies without a line is a crash, not a harness gap.

## Verdicts

| # | Check (smallest expression) | Oracle (`erl`) | Rust (`beam_rt`) | Verdict |
|---|---|---|---|---|
| RC-A | `file:write(Fd, [<<"ab">> \| <<"cd">>])`, both `pwrite` shapes, `dets` lifecycle | `ok` | `ok` | **fixed** |
| RC-A | md5 of `raw_probe.dat` / `dets_probe.dets` (12 / 5 933 bytes) | `827f51cc…` / `d9259f29…` | same two md5s | **fixed** |
| RC-B | `process_flag(min_heap_size, 233)` | `233` | `233` | **fixed** |
| RC-B | `process_flag(min_bin_vheap_size, 46422)` | `46422` | `46422` | **fixed** |
| RC-B | `process_flag(fullsweep_after, 0)` | `65535` | `65535` | **fixed** |
| RC-B | `spawn_opt(Fun,[{min_heap_size,233}])` / `[{fullsweep_after,0}]` / `[{min_bin_vheap_size,46422}]` | `233` / `0` / `46422` | same | **fixed** |
| RC-B | `ra_log_wal:init/1` flag preface (`process_info(self())`, one spawned process) | `[{trap_exit,false},{message_queue_data,on_heap},{min_bin_vheap_size,46422},{min_heap_size,233}]` | identical | **fixed** |
| RC-B | `process_info(self(), trap_exit)` in the **`-eval` process** | `{ok,true}` | `{ok,false}` | **still present** |
| RC-B | `process_flag(trap_exit, true)` in the `-eval` process | `true` | `false` | **still present** |
| RC-B | `process_flag(trap_exit, true)` in a plain `spawn/1` process | `false` | `false` | agree |
| RC-C | `erlang:external_size(fun() -> ok end)` / `({x, fun() -> ok end, y})` / `([fun() -> ok end])` / `(#{k => fun() -> ok end})` | `87` / `95` / `93` / `95` | same | **fixed** |
| RC-C | `term_to_binary` of the same five shapes (incl. a named `fun Fact/1`) | `95` / `89` / `92` / `93` / `95` | same | **fixed** |
| RC-C | `term_to_binary({F1, F2})` / `external_size({F1, F2})` with **two different** lambdas | `175` / `175` | `error:unsupported_local_fun` | **still present** (changed class: only multi-closure terms) |
| RC-C | a closure built by `erl_eval:exprs/2` | `138` | `138` | agree |
| RC-C | `byte_size(term_to_binary(fun() -> ok end))` typed into the `-eval` walker | `138` | `beam_rt: unsupported: local_fun` (exit 3) | **still present** (changed class) |
| RC-C | `display_tree` eunit module (in situ) | 11 passed / 0 failed | 11 passed / 0 failed | **fixed** |
| RC-D | 2 500 keep-while dependents, one `#delete{path = [target]}` | 27/27/28/27 ms | 5 688/5 677/5 650/5 749 ms | **still present** (≈210×) |
| RC-D | that delete's `?assert(DeletionTime < 1000)` (`test/keep_while_conditions.erl`) | passes | **fails** (`keep_while_conditions` 26/1) | **still present** |
| RC-D | deleted node set of the same delete | 2 502 nodes | 2 502 nodes | agree (latency only) |
| RC-D | 100-node dependency chain | 3/2/2/2 ms | 47/46/47/48 ms (≈23×) | slower, assertion passes |
| **RC-E** | `eunit:test('simple_get',[verbose])` in the `-eval` process | `All 94 tests passed` | `beam_rt: unsupported: opcode: run: process blocked (no message available)`, exit 3 | **still present** — 20 sweep rows |
| **RC-F** | `eunit:test('advanced_tx_delete',[verbose])` | 39 passed / 0 failed | **SIGSEGV** (exit 139, 3/3), no summary | **still present** — 1 sweep row |

## RC-A — fixed (iodata walk + on-disk bytes)

`iodata_probe.erl` prints `ok` / `ok` / `ok` / `ok` / `{ok, iodata_probe_tab}` on both runtimes.
`dets_probe.erl` extends that to the bytes: the raw writes produce a 12-byte `raw_probe.dat` with the same md5 on both sides, and `dets:open_file/insert/sync/lookup/info/close/reopen` produces a 5 933-byte `.dets` file with the same md5 on both sides.
What to run to see it: `dets_probe:main()` each side in its own cwd, then `md5sum raw_probe.dat dets_probe.dets`.

## RC-B — the three heap-sizing flags are fixed; the `-eval` process's birth `trap_exit` is still present

The fix in `crates/beam_rt/src/context/process_bridge.rs` serves `min_heap_size`, `min_bin_vheap_size` and `fullsweep_after` (old-value return; the 233 / 46 422 floors; no floor for `fullsweep_after`), and it is **bookkeeping only** — the values are recorded, not yet fed to a GC-policy input.
Consequence: `ra_log_wal:init/1`'s flag preface completes, so RC-B is no longer a Khepri wall and 29 previously-blocked modules now run (the `app`, `conditions`, `projections`, `simple_tx_delete`, … rows are green or blocked by RC-E instead).
The surviving shape is the `-eval` process's *birth* flag: on the oracle that process is born with `trap_exit = true`, on this runtime with the spawn default `false`, so `process_flag(trap_exit, true)` answers `false` (a plain `spawn/1` process agrees).
Owning crate: **`beam_rt`** — the CLI/`-eval` bootstrap path under `crates/beam_rt/src/eval*/` never sets `trap_exit` on the evaluator process.
What to run to see it: `trap_exit_probe:main()` / `process_flag_probe:main()` on both sides.

## RC-C — single-closure encodings are fixed; multi-closure terms and a walker-made closure are still present

Every single-fun shape now encodes byte-identically (bare fun, one fun inside a tuple/list/map, a named `fun Fact/1`, and a closure built by `erl_eval:exprs/2`), and `display_tree` agrees 11/0 in the sweep.
The two surviving shapes are (a) a term containing **two different** lambdas — `term_to_binary({F1, F2})` and `external_size({F1, F2})` answer `175` on the oracle and `{error,unsupported_local_fun}` here, because the encoder carries **one** closure identity rather than one per fun — and (b) a closure created by the `-eval` tree-walker itself, which is not a compiled module fun at all (`beam_rt: unsupported: local_fun`).
Owning crates: **`beam_bifs`** (`crates/beam_bifs/src/binconv.rs::term_encoder`) over **`beam_codec`**'s `Encoder` (a single `FunIdentity` slot) — the term walk must carry one identity per fun (or delegate the whole encoding to `beam_codec`).
For (b) the seam is `crates/beam_rt/src/eval/` (the walker's closure term must become a `beam_codec`-encodable identity, as `erl_eval:exprs/2`'s already is).
What to run to see it: `external_size_probe:main()` on both sides.

## RC-D — still present (keep-while delete fan-out, performance)

`keep_while_fanout_probe:main/0` builds 2 500 dependents on one node and deletes it; measured ABBA (`sys,rust,rust,sys` ×2) so the load drift is paired out.

| scenario | oracle | rust | ratio |
|---|---|---|---|
| `insert` of the 2 500 dependents | 9–12 ms | ≈2 139–2 197 ms | ≈200× |
| `delete` of the parent (2 500 dependents) | 27/27/28/27 ms (10–11 µs each) | 5 688/5 677/5 650/5 749 ms (2 260–2 299 µs each) | **≈210×** |
| `deleted_count` | 2 502 | 2 502 | agree |
| 100-node dependency chain | 3/2/2/2 ms | 47/46/47/48 ms | ≈23× |
| the suite's `?assert(DeletionTime < 1000)` | passes | **fails** (one row of `keep_while_conditions`; the sweep reads 26/1 vs the oracle's 27/0) | — |

The cost is per-dependent work in the keep-while sweep, not the delete's result set; the owning crate is **`beam_ets`** (the `keep_while` condition index walk) with the engine tier as a multiplier — the shape is a full scan per dependent, so the fix must be an index lookup, not a faster loop.
What to run to see it: `keep_while_fanout_probe:main()` on both sides (`scaling/0` for the chain).

## RC-E — the nested-run pump wall (new; 20 of the 21 rust-gap rows)

Every blocked row carries the identical first divergence `opcode: run: process blocked (no message available)` — a typed `beam_rt` capability gap, not a wrong result.
The block is **route-dependent**, which is the decisive experiment:

| what is driven | oracle | rust |
|---|---|---|
| `eunit:test('simple_get',[verbose])` from the `-eval` process (the sweep's route) | `All 94 tests passed` | `beam_rt: unsupported: opcode: run: process blocked (no message available)`, exit 3, after 9 stdout `ok` lines |
| `block_wall_probe:eunit_here(simple_get)` — same, explicit | `All 94 tests passed` | same block, exit 3 |
| `block_wall_probe:eunit_spawned(simple_get)` — eunit driven from a **spawned** process | `All 94 tests passed` | `All 94 tests passed`, `ok`, exit 0 (≈8 s) |
| `block_wall_probe:leak(30)` — 30 manual `test_ra_server_helpers` fixtures, no eunit | n/a | completes, `process_count` flat at 65 |
| `block_wall_probe:eunit_gen(12)` — 12 generated fixtures | 12/12 | blocked at fixture 4/5/6/7 on three runs, passed on a fourth |

`each_test/1` (every generator run on its own) and `eunit_twice/1` complete, so no fixture is broken and nothing leaks — the block is the *pump*, and it is intermittent (the same command passes and fails across runs).

### Blocked process pair and what it waits for

- **Blocked:** the `-eval` process (`ProcessId { number: 35, serial: 4 }` in the trace), running a nested machine; it is parked in a `receive` on the reply to a call it made into the store (`gen_statem:call`/`gen_server:call` through `khepri`/`ra`).
- **Peer that owes the reply:** the store's own server processes. At the block every one of them is `Waiting`: `gen_statem:loop/3` (the ra server), `gen_server:loop/5` with a `send_after` armed (`khepri_machine`, `khepri_cluster`), `gen_batch_server:loop_wait/2` (`ra_log_wal`), `dets:req/2` + `dets:open_file_loop2/2`.
- The pump's last round ran **0** slices (`result=Ok(27)` earlier, then `ran=0`), delivered **0** port messages, and `states` shows the run target as the only `Runnable` process — so the reply has nowhere to be produced *within the round*, and the pump folds the round to `blocked()` instead of waiting out the peer's timer.

### Owning crate and fix seam

- Text: **`beam_engine`** — `crates/beam_engine/src/error.rs:146 pub fn blocked()` (single definition of the string).
- Emitter: **`beam_rt`** — `crates/beam_rt/src/context/pump.rs::pump_once:571`, the `if ran == 0 { return Ok(None); }` arm at `:604` (and the `!self.may_pump_scheduler()` refusal at `:578`), reached from `run_export_blocking:268` / `run_label_blocking:356` → `pump_until_returned:130`.
- The round it cannot advance: `crates/beam_rt/src/runtime/scheduler.rs::pump_scheduler_boot:309`, whose progress test is bound by `TimerWait::ImminentWithin(TIMER_PUMP_SLICE)` with `TIMER_PUMP_SLICE = 100 ms` (`scheduler.rs:74`).
- Fix direction: treat a wake of the run target (a plain send into the nested machine's mailbox) as progress, and/or wait out the earliest deadline once (bounded) before folding to `blocked()`; the structural fix is to run the `-eval` program on a scheduler process instead of on a nested machine.
- Measurement-side mitigation (this crate's scope): drive eunit from a **spawned** process in `src/khepri.rs` (`observe`/`drive`) — `eunit_spawned/1` proves that route is green.

What to run to see it: `block_wall_probe:eunit_here(simple_get)` (blocks) vs `block_wall_probe:eunit_spawned(simple_get)` (green) with the khepri `-pa` set; `block_wall_khepri:fixtures(N)` and `block_wall_fixture.erl` give the checkpointed variants used to locate the fixture.

The 20 rows it explains: `advanced_delete`, `advanced_get`, `advanced_put`, `advanced_tx_get`, `advanced_tx_put`, `async_option`, `bang_functions`, `db_info`, `export`, `export_erlang`, `favor_option`, `machine_code_called_from_ra`, `projections`, `simple_delete`, `simple_get`, `simple_put`, `simple_tx_get`, `simple_tx_misc`, `simple_tx_put`, `triggers` — plus `stored_procs`' rust cell, which carries the same text (that row is a divergence only because the oracle has one failure of its own, 12/1).
One wall, not several: the landing points differ per module (fixture count, which `ra` call is parked) but the emitter, the round budget and the route dependency are identical.

## RC-F — `advanced_tx_delete` dies by SIGSEGV on a stale compiled match spec (1 row)

The row's rust status is `error`, not `unsupported`: **the runtime process dies by SIGSEGV** (exit 139, reproduced 3/3 full-module runs; the shell reports `Segmentation fault`, and `coredumpctl` holds a SIGSEGV core per run).
stdout stops after the two passing tests at `advanced_tx_delete:22` and `:30`, so there is no eunit summary for the harness to parse (`src/eunit.rs::parse` → `Unknown` → `Status::Error` via `target::unclassified`).
The crash happens in the **third** fixture, `delete_non_existing_node_using_legacy_ret_test_` (`test/advanced_tx_delete.erl:36`), whose setup passes `#{machine_version => MacVer}` → `test_ra_server_helpers:setup/2` → `meck:new(khepri_machine, [passthrough])` + `meck:expect(khepri_machine, version, fun() -> MacVer end)`.
That fixture run **alone** passes, so the module-level run is the reliable repro.
The core backtrace (`gdb <beam_rt> <core>`, symbols present):

```
#0 beam_term::list::car            crates/beam_term/src/list.rs:86
#1 beam_ets::select::list_elements crates/beam_ets/src/select/mod.rs:227
#2 beam_ets::select::compile       crates/beam_ets/src/select/mod.rs:242
#3 beam_ets::select::run::match_spec_run  crates/beam_ets/src/select/run.rs:69
#4 beam_bifs::ets::specs::match_spec_run_2 crates/beam_bifs/src/ets/specs.rs:165
```

Mechanism: `ets:match_spec_compile/1` stores the **raw spec term verbatim** as a bare `Eterm` in `TableRegistry::specs` (`crates/beam_bifs/src/ets.rs::insert_spec:614`, read back by `spec_raw:624`), i.e. a word that aliases the compiling process's heap — the documented M1 substitution (`crates/beam_bifs/src/ets/specs.rs:84-94`, "sound only for a same-process run on a never-relocating bump arena").
`meck` compiles the spec once (`meck_args_matcher:new/1`, spec `[{{[]},[],['$_']}]`) and runs it much later from `meck_proc` state, so the read-back dereferences stale words: under one timing they decode as a non-list and the call answers `badarg`, under another the walk lands on a header-shaped word and `beam_term::list::car/1` dereferences it.
The amplifier is `beam_ets`'s walk: `list_elements` uses the C bit trick (`is_list` ⇔ `x & 2 == 0`, which is **true for a `TAG_PRIMARY_HEADER` word**, `crates/beam_term/src/list.rs:52-70`) instead of a tagged-pointer check, so a garbage word is dereferenced rather than rejected.

Two seams, both needed:

- **`beam_bifs`** — `insert_spec`/`spec_raw` must not store a caller-heap alias: deep-copy the spec onto a runtime-owned heap (or store its ETF bytes and re-decode per run), so a handle is valid in any process and across collections.
- **`beam_ets`** — `crates/beam_ets/src/select/mod.rs::list_elements:222` must require a non-nil **list word** (pointer tag checked, header words rejected) so a bad term is `badarg`, never a segfault; the same walk backs `ets:select/2`, so this hardens the whole select family.

What to run to see it: `cd $CWD/advanced_tx_delete && $RT -noshell <khepri -pa…> -eval "eunit:test('advanced_tx_delete',[verbose]), halt(0)."` — expect exit 139; `coredumpctl list beam_rt` then `gdb $RT <core>` for the backtrace.

## Side observations (not RC classes, reported rather than acted on)

1. **Small-map key order can differ** (`maps:to_list/1`, `maps:keys/1`, and therefore `~p` of an atom-keyed map): the previous-value map of `process_flag(max_heap_size, #{size => 0})` prints `#{error_logger,… , include_shared_binaries,… , kill,… , size,…}` on the oracle and `#{kill,… , error_logger,… , include_shared_binaries,… , size,…}` here.
   Map iteration order is undefined by the language and both runtimes order small maps by atom *index*, so the difference comes from the atom-table insertion order each runtime assigns while loading a module's atoms, not from map code (owning crate: `beam_term`).
   It is a false-mismatch risk for any differential case that compares `~p` of an atom-keyed map.
2. The `file:open/2` port descriptor map differs in shape (`owner`/`fd_registry_id` keys, `#Ref` values) — an internal representation; every *documented* observable agrees.
3. The system `erl` exits 1 **silently** when a `-eval` expression raises, so a probe that dies mid-run without a line is an escape, not a missing observation; probes here catch their own expressions.
4. Nothing else in the probes disagreed.

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
| `block_wall_khepri.erl` | RC-E | `… <khepri -pa…> -eval 'block_wall_khepri:fixtures(12), halt(0).'` (stderr checkpoints `ck/2`, so ordering is not read from buffered stdout) |
| `block_wall_fixture.erl` | RC-E | 30 self-contained spawn/receive fixtures, `… -eval 'block_wall_fixture:main(), halt(0).'` |
| — | RC-F | no probe: the repro is the module run itself (`eunit:test('advanced_tx_delete',[verbose])`), plus the core backtrace above |
