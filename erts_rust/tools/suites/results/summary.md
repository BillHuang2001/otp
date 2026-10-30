# Test-suite parity sweep

Generated at `1793376607` (Unix seconds) by `suites run --target khepri --module triggers`.

## Environment

- oracle: `/nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl` — `29/17.1.1`
- rust runtime: `/home/bill/Source/otp/.genesis/workers/worker_T1_A5/erts_rust/target/release/beam_rt` — `30/17.1`
- rust workspace commit: `b0099f43e31275a3802a8cd4702574002a765b85`
- tier: `default`
- loadavg before/after: `0.83 3.02 3.47 1/862 1722` / `0.75 2.20 3.11 1/814 1867` on 16 logical CPUs
- the host is **shared**: only pass/fail counts are portable, never a wall time

### Target `khepri` (eunit)

- source: `/home/bill/Source/khepri` (khepri) at `2ac09291bac7`
- staged copy: `/tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj`
- build: `/nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin/rebar3 as test compile` — ok (reused a completed build)
- build detail: reused a completed `rebar3 as test compile` in the staged copy
- tally: measured 1 · agree 1 · agree-fail-identical (excluded) 0 · divergence 0 · oracle-only-fail 0 · rust-gap 0 · not-run 0
- totals: sys 118/0 passed/failed · rust 118/0 passed/failed

Notes:
- drive (identical on both sides): `eunit:test('{module}',[verbose]), halt(0).` — the module name is substituted per row
- `rebar3 as test compile` is used for the build only; the numbers come from running `eunit:test/2` directly on both runtimes
- code path: 11 entries (every `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test`)
- jobs: 4 concurrent module runs (each in its own OS process and working directory)
- per-module working directory: `/tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/cwd` (khepri's tests create `_test.<name>` dirs relative to the cwd; both sides of a module share one)
- sys: `cd /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/cwd/triggers && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''triggers'\'',[verbose]), halt(0).'`
- rust: `cd /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/cwd/triggers && /home/bill/Source/otp/.genesis/workers/worker_T1_A5/erts_rust/target/release/beam_rt -noshell -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''triggers'\'',[verbose]), halt(0).'`

| row | sys (P/F) | rust (P/F) | verdict | first divergence / note |
|---|---|---|---|---|
| `triggers` | 118/0 | 118/0 | agree |  |

Recorded invocations (the two sides get the *same* `-pa` set and the *same* `-eval`):

```
# eunit advanced_delete [sys]
cd /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/cwd/advanced_delete && /nix/store/lljrlcd2cqv64a6xz4cyaz723x2nwpnl-erlang-29.1.2/lib/erlang/erts-17.1.1/bin/erl -noshell -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''advanced_delete'\'',[verbose]), halt(0).'
# eunit advanced_delete [rust]
cd /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/cwd/advanced_delete && /home/bill/Source/otp/.genesis/workers/worker_T1_A5/erts_rust/target/release/beam_rt -noshell -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/aten/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cf/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/cth_readable/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/gen_batch_server/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/horus/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/khepri/test -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/meck/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/proper/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/ra/ebin -pa /tmp/genesis/task_3a94b23b77a88346/erts-suites-stage/khepri/proj/_build/test/lib/seshat/ebin -eval 'eunit:test('\''advanced_delete'\'',[verbose]), halt(0).'
# rebar3 as test compile [build]
cd <stage>/proj && /nix/store/z2nzp267fq9mnd51rccz5a47x35q9xb6-rebar3-3.27.1/bin/rebar3 as test compile
```

