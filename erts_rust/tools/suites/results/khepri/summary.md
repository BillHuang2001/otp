# Test-suite parity sweep

Generated at `1793263701` (Unix seconds) by `suites run --target khepri --jobs 4 --out results/khepri`.

## Environment

- oracle: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1`
- rust runtime: `/home/bill/Source/otp/.genesis/workers/worker_T1_A648/erts_rust/target/release/beam_rt` — `30/17.1`
- rust workspace commit: `71ffbbb37971d0b193e0dc74b60b6c448874349e`
- tier: `default`
- loadavg before/after: `9.06 10.67 10.47 97/1596 5` / `14.85 12.52 11.17 18/954 3603` on 16 logical CPUs
- the host is **shared**: only pass/fail counts are portable, never a wall time

### Target `khepri` (eunit)

- source: `/home/bill/Source/khepri` (khepri) at `2ac09291bac7`
- staged copy: `/tmp/erts-suites-stage/khepri/proj`
- build: `/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin/rebar3 as test compile` — ok (reused a completed build)
- build detail: reused a completed `rebar3 as test compile` in the staged copy
- tally: measured 43 · agree 20 · agree-fail-identical (excluded) 8 · divergence 2 · oracle-only-fail 0 · rust-gap 21 · not-run 0
- totals: sys 1500/1 passed/failed · rust 412/1 passed/failed

Notes:
- drive (identical on both sides): `eunit:test('{module}',[verbose]), halt(0).` — the module name is substituted per row
- `rebar3 as test compile` is used for the build only; the numbers come from running `eunit:test/2` directly on both runtimes
- code path: 11 entries (every `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test`)
- jobs: 4 concurrent module runs (each in its own OS process and working directory)
- per-module working directory: `/tmp/erts-suites-stage/khepri/cwd` (khepri's tests create `_test.<name>` dirs relative to the cwd; both sides of a module share one)
- sys: `cd /tmp/erts-suites-stage/khepri/cwd/advanced_delete && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''advanced_delete'\'',[verbose]), halt(0).'`
- rust: `cd /tmp/erts-suites-stage/khepri/cwd/advanced_delete && /home/bill/Source/otp/.genesis/workers/worker_T1_A648/erts_rust/target/release/beam_rt -noshell -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''advanced_delete'\'',[verbose]), halt(0).'`

| row | sys (P/F) | rust (P/F) | verdict | first divergence / note |
|---|---|---|---|---|
| `advanced_delete` | 33/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `advanced_get` | 13/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `advanced_put` | 40/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `advanced_tx_delete` | 39/0 | error | rust-gap | ======================== EUnit ======================== module 'advanced_tx_delete' advanced_tx_delete:22: -delete_non_existing_node_test_/0-fun-4-...[0.097 s] … |
| `advanced_tx_get` | 15/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `advanced_tx_put` | 43/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `app` | 36/0 | 36/0 | agree |  |
| `async_option` | 20/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `bang_functions` | 85/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `cluster_SUITE` | no_tests | no_tests | agree-fail-identical | a common_test suite: it carries no eunit tests (0/0 would be a lie) |
| `conditions` | 23/0 | 23/0 | agree |  |
| `cth_log_redirect_any_domains` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `db_info` | 24/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `delete_command` | 12/0 | 12/0 | agree |  |
| `display_tree` | 11/0 | 11/0 | agree |  |
| `export` | 56/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `export_erlang` | 63/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `export_helper` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `favor_option` | 5/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `fence` | 6/0 | 6/0 | agree |  |
| `helpers` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `keep_while_conditions` | 27/0 | 26/1 | divergence | keep_while_conditions: delete_node_with_many_dependents: **error:{assert,[{module,keep_while_conditions}, |
| `machine_code_called_from_ra` | 15/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `mod_used_for_transactions` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `non_versioned_commands` | 15/0 | 15/0 | agree |  |
| `path_operations` | 51/0 | 51/0 | agree |  |
| `pattern_tree` | 5/0 | 5/0 | agree |  |
| `prefix_tree` | 4/0 | 4/0 | agree |  |
| `projections` | 134/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `prop_path_operations` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `prop_state_machine` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `protect_against_dups_option` | 13/0 | 13/0 | agree |  |
| `put_command` | 19/0 | 19/0 | agree |  |
| `queries` | 16/0 | 16/0 | agree |  |
| `return_indirect_deletes_option` | 15/0 | 15/0 | agree |  |
| `root_node` | 22/0 | 22/0 | agree |  |
| `sigils` | 4/0 | 4/0 | agree |  |
| `simple_delete` | 42/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `simple_get` | 94/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `simple_put` | 38/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `simple_tx_delete` | 46/0 | 46/0 | agree |  |
| `simple_tx_get` | 106/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `simple_tx_misc` | 45/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `simple_tx_put` | 47/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `stored_procs` | 12/1 | unsupported | divergence | opcode: run: process blocked (no message available) |
| `test_ra_server_helpers` | no_tests | no_tests | agree-fail-identical | no eunit tests (common_test suite or helper module) |
| `timeout` | 6/0 | 6/0 | agree |  |
| `triggers` | 118/0 | unsupported | rust-gap | opcode: run: process blocked (no message available) |
| `tx_funs` | 42/0 | 42/0 | agree |  |
| `tx_sprocs` | 33/0 | 33/0 | agree |  |
| `utils` | 7/0 | 7/0 | agree |  |

Recorded invocations (the two sides get the *same* `-pa` set and the *same* `-eval`):

```
# eunit advanced_delete [sys]
cd /tmp/erts-suites-stage/khepri/cwd/advanced_delete && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''advanced_delete'\'',[verbose]), halt(0).'
# eunit advanced_delete [rust]
cd /tmp/erts-suites-stage/khepri/cwd/advanced_delete && /home/bill/Source/otp/.genesis/workers/worker_T1_A648/erts_rust/target/release/beam_rt -noshell -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''advanced_delete'\'',[verbose]), halt(0).'
# rebar3 as test compile [build]
cd <stage>/proj && /nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin/rebar3 as test compile
```

