# Khepri eunit parity — sweep notes
Sweep of the **Khepri** eunit suite (`/home/bill/Source/khepri`, third-party, read-only) under the
system ERTS (oracle) and under the Rust runtime (`beam_rt`), driven by this crate
(`suites run --target khepri`). Raw evidence: `khepri/report.json` (schema `suites/1`) and
`khepri/summary.md` (runtime commit `71ffbbb379`, tier `default`); the re-verified per-class probes with
their full observation tables live in `khepri-diagnostics/FINDINGS.md`, next to this file. What these
notes add over the artifact is the attribution: the first failing test, the smallest failing expression
and the owning crate for every row that is not `agree`.
**Headline.** 51 candidate modules: 43 run real eunit tests on the oracle (`1500` passed / `1` failed),
8 run none. Tally: **21 `agree`, 8 `agree-fail-identical`, 2 `divergence`, 20 `rust-gap`**, rust
`403` passed / `1` failed. Every one of the 20 `rust-gap` rows is one of two runtime defects: **19
block on the nested-run pump (RC-E)** and **1 dies by SIGSEGV on a stale compiled match spec (RC-F)**.
The single rust test failure left is the performance assertion of **RC-D** (`keep_while_conditions`); the
second `divergence` verdict, `stored_procs`, is the oracle's own pre-existing failure (its rust cell is
the same RC-E block).
## Environment
- Oracle: system `erl`/`erlc` on `$PATH` — OTP `29.1.2`, erts `17.1.1` (the tree pins erts `17.1`).
- Rust: `erts_rust/target/release/beam_rt`, workspace commit `71ffbbb379` (sources unchanged since).
- Khepri `2ac09291bac7`, staged copy `${TMPDIR}/erts-suites-stage/khepri/proj`; `rebar3 as test compile`
  reused (not rebuilt). Code path: 11 `-pa` entries (every `_build/test/lib/*/ebin` plus
  `_build/test/lib/khepri/test`), identical on both sides.
- Drive: `eunit:test('<module>',[verbose]), halt(0).`; `--jobs 4`, one OS process and one fresh cwd per
  module (`target::fresh_dir`: a `_test.<module>` tree left behind otherwise reddens both sides).
- Host: 16 logical CPUs, **shared**; loadavg before/after the sweep `2.76 6.12 18.63` /
  `3.98 5.95 17.43`. Only counts and same-host paired ratios are portable.
Throughout, `sys P/F` and `rust P/F` are `passed/failed` as parsed from eunit's own summary. `unsupported`
means the runtime refused the whole run with a typed capability gap (no counts exist); `error` means the
process died or printed no parseable summary.
## Per-module table (all 51 candidate modules)
| module | sys P/F | rust P/F | verdict | root-cause class |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | unsupported | rust-gap | **RC-E** |
| `advanced_get` | 13/0 | 13/0 | agree | — |
| `advanced_put` | 40/0 | unsupported | rust-gap | **RC-E** |
| `advanced_tx_delete` | 39/0 | error (SIGSEGV) | rust-gap | **RC-F** |
| `advanced_tx_get` | 15/0 | 15/0 | agree | — |
| `advanced_tx_put` | 43/0 | unsupported | rust-gap | **RC-E** |
| `app` | 36/0 | 36/0 | agree | — |
| `async_option` | 20/0 | unsupported | rust-gap | **RC-E** |
| `bang_functions` | 85/0 | unsupported | rust-gap | **RC-E** |
| `cluster_SUITE` | no_tests | no_tests | agree-fail-identical | — (common_test suite) |
| `conditions` | 23/0 | 23/0 | agree | — |
| `cth_log_redirect_any_domains` | no_tests | no_tests | agree-fail-identical | — (common_test suite) |
| `db_info` | 24/0 | unsupported | rust-gap | **RC-E** |
| `delete_command` | 12/0 | 12/0 | agree | — |
| `display_tree` | 11/0 | 11/0 | agree | — |
| `export` | 56/0 | unsupported | rust-gap | **RC-E** |
| `export_erlang` | 63/0 | unsupported | rust-gap | **RC-E** |
| `export_helper` | no_tests | no_tests | agree-fail-identical | — (helper module) |
| `favor_option` | 5/0 | unsupported | rust-gap | **RC-E** |
| `fence` | 6/0 | 6/0 | agree | — |
| `helpers` | no_tests | no_tests | agree-fail-identical | — (helper module) |
| `keep_while_conditions` | 27/0 | 26/1 | divergence | **RC-D** (`?assert(DeletionTime < 1000)`) |
| `machine_code_called_from_ra` | 15/0 | 15/0 | agree | — |
| `mod_used_for_transactions` | no_tests | no_tests | agree-fail-identical | — (helper module) |
| `non_versioned_commands` | 15/0 | 15/0 | agree | — |
| `path_operations` | 51/0 | 51/0 | agree | — |
| `pattern_tree` | 5/0 | 5/0 | agree | — |
| `prefix_tree` | 4/0 | 4/0 | agree | — |
| `projections` | 134/0 | unsupported | rust-gap | **RC-E** |
| `prop_path_operations` | no_tests | no_tests | agree-fail-identical | — (property module) |
| `prop_state_machine` | no_tests | no_tests | agree-fail-identical | — (property module) |
| `protect_against_dups_option` | 13/0 | 13/0 | agree | — |
| `put_command` | 19/0 | 19/0 | agree | — |
| `queries` | 16/0 | 16/0 | agree | — |
| `return_indirect_deletes_option` | 15/0 | unsupported | rust-gap | **RC-E** |
| `root_node` | 22/0 | 22/0 | agree | — |
| `sigils` | 4/0 | 4/0 | agree | — |
| `simple_delete` | 42/0 | 42/0 | agree | — |
| `simple_get` | 94/0 | unsupported | rust-gap | **RC-E** |
| `simple_put` | 38/0 | unsupported | rust-gap | **RC-E** |
| `simple_tx_delete` | 46/0 | unsupported | rust-gap | **RC-E** |
| `simple_tx_get` | 106/0 | unsupported | rust-gap | **RC-E** |
| `simple_tx_misc` | 45/0 | unsupported | rust-gap | **RC-E** |
| `simple_tx_put` | 47/0 | unsupported | rust-gap | **RC-E** |
| `stored_procs` | 12/1 | unsupported | divergence | **RC-E** (rust cell); the failure is oracle-side |
| `test_ra_server_helpers` | no_tests | no_tests | agree-fail-identical | — (helper module) |
| `timeout` | 6/0 | 6/0 | agree | — |
| `triggers` | 118/0 | unsupported | rust-gap | **RC-E** |
| `tx_funs` | 42/0 | 42/0 | agree | — |
| `tx_sprocs` | 33/0 | unsupported | rust-gap | **RC-E** |
| `utils` | 7/0 | 7/0 | agree | — |
## Totals
| | rows that ran a test | tests passed | tests failed |
|---|---|---|---|
| oracle (`erl`, erts-17.1.1) | 43 | **1500** | **1** (`stored_procs:crashing_sproc_stacktrace_test`) |
| rust (`beam_rt`, erts-17.1) | 22 | **403** | **1** (`keep_while_conditions`, RC-D) |

21 rows are fully green on rust (`agree`), and no rust pass corresponds to an oracle failure; the single
rust failure (`keep_while_conditions`) is an oracle pass, and §RC-D owns it.
The 1 097 oracle tests rust does not execute — 1 501 minus the 404 it does — all sit behind the 21 rows
whose rust cell carries no counts: the 19 `rust-gap` RC-E rows, RC-F's `advanced_tx_delete`, and
`stored_procs`, whose rust cell is the same RC-E block.
No RC-E cell contributes counts: the runtime refuses the run before eunit finishes, so their
oracle-passing tests are `unknown`, not failing. RC-E is intermittent by nature — the same module can
pass and block across runs — so a row that reports counts on one sweep can report `unsupported` on the
next without any runtime change (the earlier committed sweep, `git show
9e860feef6:erts_rust/tools/suites/results/khepri/report.json`, reads rust `412/1` with 20 `agree`).
## Root causes (first failing test, smallest failing expression, owning crate)
The classes are the ones named in `khepri-diagnostics/FINDINGS.md`, which carries the per-probe
observation tables and the exact commands; this section states the attribution and points there for the
raw rows.
**RC-A does not appear**: the improper-iodata `file:write`/`pwrite` walk, the `dets` lifecycle and the
on-disk `.dets` bytes are oracle-identical on this tree (`dets_probe.erl`, md5 `d9259f29…` both sides).
**RC-B does not appear either**: the three heap-sizing `process_flag/2` flags now agree, so every `ra`
fixture sets up; what survives of RC-B is the `-eval` process's birth `trap_exit` (`true` on the oracle,
`false` here), which no Khepri row reaches. Owning crate: `beam_rt` (the `-eval` bootstrap path).
### RC-E — the nested-run pump wall (20 rows, the whole `rust-gap` group but one)
Every blocked row carries the identical first divergence `opcode: run: process blocked (no message
available)`. First failing point: the fixture's first `ra`/`gen_statem` call that needs a reply from a
*timer-parked* peer (e.g. `simple_get`'s 10th test; `db_info` at test 4).
Smallest repro: `block_wall_probe:eunit_here(simple_get)` → `beam_rt: unsupported: …blocked…`, exit 3,
while `block_wall_probe:eunit_spawned(simple_get)` → `All 94 tests passed`; the oracle is green on both.
The block is the `-eval` process's nested machine parked in a `receive` whose peer is `Waiting`
(`gen_statem:loop/3`, `gen_server:loop/5` with a `send_after` armed, `gen_batch_server:loop_wait/2`,
`dets:open_file_loop2/2`) while the pump's round ran 0 slices.
Owning crate: **`beam_rt`** — `context/pump.rs::pump_once:604` (`ran == 0 => Ok(None)`) under
`pump_until_returned:130`, with the round in `runtime/scheduler.rs::pump_scheduler_boot:309`
(`TimerWait::ImminentWithin(TIMER_PUMP_SLICE)` = 100 ms); the text is `beam_engine`'s
`error.rs::blocked:146`. Fix direction: count a wake of the run target as progress and wait out the
earliest deadline (bounded) before folding to `blocked()`; the structural fix is to run the `-eval`
program on a scheduler process.
### RC-F — `advanced_tx_delete` dies by SIGSEGV (1 row)
The only row whose rust status is `error`: the process dies by **SIGSEGV** (exit 139, 3/3 runs) in the
third fixture, `delete_non_existing_node_using_legacy_ret_test_` (`test/advanced_tx_delete.erl:36`), after
the two passing tests at `:22`/`:30` — so no eunit summary exists to parse.
That fixture is the one that `meck`s `khepri_machine` (`test_ra_server_helpers:setup/2`); the crash is
`ets:match_spec_run/2` walking a compiled spec whose stored raw term aliases the compiling process's heap
(`beam_bifs::ets::specs::match_spec_run_2` → `beam_ets::select::compile` → `list_elements` →
`beam_term::list::car`, per the core backtrace in FINDINGS.md).
Owning crates: **`beam_bifs`** (`ets.rs::insert_spec`/`spec_raw` must not store a caller-heap alias) plus
**`beam_ets`** (`select/mod.rs::list_elements` must reject a non-cons word instead of dereferencing the
C bit trick's header case).
### RC-C — ETF encode of a term containing two different local funs (no Khepri row)
`display_tree` now agrees 11/0, and every single-closure shape encodes byte-identically (bare fun, one fun
in a tuple/list/map, a named fun, an `erl_eval:exprs/2` closure). What survives is a term with **two
different** lambdas: `term_to_binary({F1, F2})` → oracle `175`, rust `error:unsupported_local_fun`
(the encoder carries one closure identity, not one per fun). Owning crates: **`beam_bifs`**
(`binconv.rs::term_encoder`) over **`beam_codec`**'s single-`FunIdentity` `Encoder`.
### RC-D — a keep-while delete fan-out is ~210× slower (performance, not semantics)
First failing test: `keep_while_conditions:delete_node_with_many_dependents/0` (the assertion
`?assert(DeletionTime < 1000)`), the row's only rust test failure.
Smallest repro: `keep_while_fanout_probe:main/0` — 2 500 dependents on one node, one
`#delete{path = [target]}`: oracle `27/27/28/27` ms, rust `5 688/5 677/5 650/5 749` ms, measured
ABBA-ordered (`sys,rust,rust,sys` ×2) in a 10.8–15.1 loadavg window, `deleted_count` `2 502` on both
sides; the 100-node chain is ≈23× and still under its assertion.
The cost is per-dependent keep-while work, so the lever is an index lookup, not a faster loop.
Owning crate: **`beam_ets`** (the keep-while condition sweep), with the engine tier as a multiplier.
## Unknown, not "passing"
The RC-E rows (and `stored_procs`' rust cell) execute no test on the rust side, so the oracle-passing
tests behind them are `unknown`, never failures. Two further caveats: RC-E is intermittent (a single
module run can pass and block on consecutive runs), and `advanced_tx_delete` crashed rather than parsed,
so its `error` status says nothing about the tests after the third fixture.
## Re-deriving this
Every count above is re-derivable by re-running `suites run --target khepri` (a full sweep, milestone-only)
or by `--module <name>` for one row; every millisecond quoted for RC-D is an indicative host observation,
never a benchmark. Per-class verdicts and the probe templates that re-measure them live in
`khepri-diagnostics/FINDINGS.md`.
