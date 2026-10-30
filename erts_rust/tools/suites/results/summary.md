# Test-suite parity sweep

Generated at `1793390485` (Unix seconds) by `suites run --target stdlib --suite binary_module_SUITE --runtime all --json`.

## Environment

- oracle: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1`
- rust runtime: `/home/bill/Source/otp/.genesis/workers/worker_T1_A21/erts_rust/target/release/beam_rt` — `30/17.1`
- rust workspace commit: `6423575cf85cf33ea85ee73c8826d177d5930ea2`
- tier: `default`
- loadavg before/after: `2.86 5.40 4.70 3/856 4` / `8.99 5.94 5.12 17/1011 397` on 16 logical CPUs
- the host is **shared**: only pass/fail counts are portable, never a wall time

### Target `stdlib` (ct)

- source: `/home/bill/Source/otp/.genesis/workers/worker_T1_A21/lib/stdlib/test` (otp) at `6423575cf85c`
- staged copy: `/tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/src`
- build: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erlc -o <stage>/ebin -I <stage>/src <suite>.erl …` — ok
- build detail: compiled 1 suites + 6 helpers + runner with the oracle's erlc
- tally: measured 20 · agree 14 · agree-fail-identical (excluded) 1 · divergence 5 · oracle-only-fail 0 · rust-gap 0 · not-run 0
- totals: sys 19/1 passed/failed · rust 14/6 passed/failed

Notes:
- drive (identical on both sides): `ct_runner:run('{suite}', {case_timeout_ms}, {suite_budget_ms}, "{data_root}"), halt(0).`
- each case runs in its own `spawn_monitor` with a per-case timeout; `init_per_group`/`end_per_group`/`init_per_testcase` hooks are NOT executed
- cases that fail identically on both sides (missing `ct` config/data dirs, group-hook dependencies) are `agree-fail-identical`: excluded from both numerators, never counted as passes
- per-case timeout: 300 s · suite budget: 600 s (the runner stops launching cases when the budget expires and prints `SUITE_DEADLINE <not-run>`, so the remaining cases are reported as *not measured*, never as failures)
- binary_module_SUITE: 20 cases — sys 19/1 pass/fail, rust 14/6 pass/fail

| row | sys (P/F) | rust (P/F) | verdict | first divergence / note |
|---|---|---|---|---|
| `binary_module_SUITE:ac_simd_root_skip` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:badargs` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:bin_to_list` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:check_no_invalid_read_bug` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:copy` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:doctests` | 1/0 | 0/1 | divergence | {exception,error,{1,errors},{ct_doctest,run_module_docs,3,...}} |
| `binary_module_SUITE:encode_decode` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:error_info` | 1/0 | 0/1 | divergence | 'exit <structured reason>' |
| `binary_module_SUITE:guard` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:hex_encoding` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:interesting` | 0/1 | 0/1 | agree-fail-identical | {exception,error,{badmatch,false},{binary_module_SUITE,do_interesting,1,...}} |
| `binary_module_SUITE:join` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:list_to_bin` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:longest_common_trap` | 1/0 | 0/1 | divergence | {exception,error,unsupported_bif,{erlang,bump_reductions,[...],...}} |
| `binary_module_SUITE:parts` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:random_ref_comp` | 1/0 | 0/1 | divergence | timeout |
| `binary_module_SUITE:random_ref_fla_comp` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:random_ref_sr_comp` | 1/0 | 1/0 | agree |  |
| `binary_module_SUITE:referenced` | 1/0 | 0/1 | divergence | {exception,error,{badmatch,1023},{binary_module_SUITE,referenced,1,...}} |
| `binary_module_SUITE:scope_return` | 1/0 | 1/0 | agree |  |

Recorded invocations (the two sides get the *same* `-pa` set and the *same* `-eval`):

```
# ct binary_module_SUITE [sys]
cd /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/cwd/binary_module_SUITE && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/ctl -eval 'ct_runner:run('\''binary_module_SUITE'\'', 300000, 600000, "/tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/src"), halt(0).'
# ct binary_module_SUITE [rust]
cd /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/cwd/binary_module_SUITE && /home/bill/Source/otp/.genesis/workers/worker_T1_A21/erts_rust/target/release/beam_rt -noshell -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/ctl -eval 'ct_runner:run('\''binary_module_SUITE'\'', 300000, 600000, "/tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/stdlib/src"), halt(0).'
# erlc [build]
/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erlc -o <stage>/ebin -I <stage>/src <suite>.erl …
```

