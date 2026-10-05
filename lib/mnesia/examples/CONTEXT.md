# Mnesia Examples & Benchmarks

## Intent
Standalone Mnesia demo and benchmark programs shipped with the application, plus symlinks to the tutorial sources that also serve as `doc/src` input.
Used for manual experimentation with Mnesia APIs and exercised by the test suite; not part of the build of `mnesia/ebin`.

## API Surface (programs and what each demonstrates)
Top level (`lib/mnesia/examples/`):
- `company.erl` + `company.hrl` — the classic Mnesia "company" tutorial: `employee`, `dept`, `project`, `manager` (bag), `at_dep`, `in_proj` (bag) tables. Demonstrates transactions, bag tables, `qlc` queries, `raise/2` vs `bad_raise/2` (retried vs aborted transactions), `over_write/2`, `filter/2`, `dotimes/2`, `bench1/0`, `dist_init/0`.
- `company_o.erl` + `company_o.hrl` — `ordered_set`/collapsed-schema variant: the employee record embeds dept and projects; adds `sinit/0` (`mnesia:create_schema/1`).
- `bup.erl` — backup/restore demos on `mnesia:traverse_backup/6`: `view/2` (dump a backup), `change_node_name/5` (rewrite schema node names), `test/1`, plus `count/1`, `display/1`.
- `mnesia_meter.erl` — metering helper: `go/0`, `go/1` (replica type), `go/2` (replica type + nodes), `repeat_meter/2`; times transactions on distributed nodes.
- `mnesia_tpcb.erl` — full TPC-B implementation for Mnesia: `init/1`, `start/0,1,2`, `run/1`, `stop/0`, `verify_tabs/0`, `config/2`, driver/reporter internals, and the fragment/conflict/dist/replica helpers (`conflict_test/1`, `dist_test/1`, `replica_test/1`, `sticky_replica_test/1`, `remote_test/1`, `remote_frag2_test/1`) the test suite calls directly.
- `DATA` — sample table data in Mnesia external text format (for `mnesia:load_textfile/1`) used by the company demo.
- `Makefile` — install-only; see Notes for Agents.

`bench/` — Canadian real-time DB benchmark (LMC/UU-01:025, by Håkan Mattsson):
- `bench.erl` — main API/config entry: `run/0,1`, `start_all/0,1`, `populate/0,1`, `generate/0,1`, `args_to_config/1`, `verify_config/2`, `stop_peer_nodes/1`, `bind_schedulers/0`.
- `bench_populate.erl` — creates and populates the fragmented `subscriber`/`group`/`server`/`session`/`suffix` tables.
- `bench_trans.erl` — the T1..T5 benchmark transactions (`update_current_location/5`, `read_current_location/2`, `read_session_details/4`, `create_session_to_server/6`, `delete_session_from_server/5`).
- `bench_generate.erl` — spawns the request generators, runs warmup/measure/cooldown and aggregates statistics.
- `bench.hrl` — `#config{}` (defaults) plus `#subscriber{}`, `#session{}`, `#server{}`, `#group{}` records and debug macros.
- `bench.sh` — starts `erl -pa .. -boot start_sasl -sasl errlog_type error -sname bench`; with args runs `bench:run(ConfigFile)` once per config file, with none drops into a shell.
- `bench.config1`..`bench.config7` — 7 preconfigured scenarios: 1/2 = single `bench@wppgpbN` node, 3/4 = 2 nodes, 5/6 = 4 nodes, 7 = 8 nodes; `n_replicas` 1 (configs 1–2) or 2 (3–7); all `ram_copies`, `sticky_write`, `n_fragments` 100, `n_subscribers` 500000, `n_groups` 100, `n_servers` 20, 15 min duration.
- `bench/Makefile` — `make all` = `erl -make`; `make test` = `./bench.sh bench.config*`; `make clean` removes `*.beam`.
- `bench/README` — authoritative parameter reference (`generator_profile` t1..t5/ping/random, warmup/duration/cooldown, `storage_type`, `n_fragments`, `n_replicas`, `write_lock_type`, `use_binary_subscriber_key`, `always_try_nearest_node`, `cookie`, node-start requirements).

## Constraints
- `company.erl`, `company.hrl`, `company_o.erl`, `company_o.hrl`, `bup.erl` and `DATA` are **git symlinks** to `../doc/src/<same name>` — they are the same file, not copies, so editing through either path changes both and no sync step exists. `../doc/src/` additionally holds `company.fig`, `DATA2`, `FRUITS`, not exposed here.
- `mnesia_meter.erl` and `mnesia_tpcb.erl` are regular files here and are also committed byte-identically in `../test/` (identical git blob SHAs). Editing one of the pair without the other silently diverges the copies.
- The examples must stay compilable: `../test/Makefile` compiles `../examples/{mnesia_meter,mnesia_tpcb}.erl` and `../doc/src/{company,company_o,bup}.erl` straight into the test ebin.
- File names/arities must stay stable because test modules call them directly (`bup:test/1`, `mnesia_tpcb:init/1`, `run/1`, `stop/0`, `verify_tabs/0`).

## Notes for Agents
- `examples/Makefile` compiles nothing: `TARGET_FILES = ` is empty (the real assignment is commented out), so `clean` removes nothing. Only `release_docs_spec` does work, installing the `.erl`/`.hrl`/`DATA` files plus the whole `bench/` dir into `$(RELEASE_PATH)/lib/mnesia-$(VSN)/doc/examples` — examples are deliberately *not* installed as `lib/mnesia-<VSN>/examples`.
- The example modules carry edoc doc chunks (`%0`, `%1`, `%% @doc` markers); the symlinked files under `../doc/src/` are simultaneously the documentation sources, which is why the sources live there and are only *referenced* from here.
- `bench/` is manual-only: never built by the OTP build, no Common Test coverage. It hardcodes node names `bench@wppgpb1`..`bench@wppgpb8`, needs `$ERL_TOP/bin` on `PATH` (or manually started nodes and ssh-free remote startup) and uses the `bench_cookie` cookie.
- `mnesia_meter.erl`, `mnesia_tpcb.erl` and `DATA` are also reachable through the code path `mnesia/examples` that `../test/README` prescribes for running the suite.

## Routing Table
| Area | Description |
| --- | --- |
| `bench/` | Canadian (LMC/UU-01:025) DB benchmark harness: API/generator/transactions/populate, 7 configs, README, `bench.sh`. Small and self-contained — no own CONTEXT.md needed. |
| `../doc/src/` | Real source location of the symlinked `company*`, `bup.erl`, `DATA` (sibling reached via symlink — READ-ONLY from here; edits made here land in that directory) |
| `../test/` | Test suite that compiles and runs these examples (`mnesia_examples_test.erl` for bup/meter/tpcb, `mnesia_install_test.erl`, `mnesia_consistency_test.erl`); holds duplicate copies of `mnesia_meter.erl`/`mnesia_tpcb.erl` (sibling — read-only, escalate writes to parent) |
| `../src/` | Mnesia application sources — the APIs these programs demonstrate |

## See Also
- `../test/CONTEXT.md` — how the examples are compiled into the test ebin and which suites use them.
- `../doc/CONTEXT.md` — documentation build that consumes `../doc/src/`, including the symlinked tutorial sources.
- `../CONTEXT.md` — mnesia application root (`MNESIA_VSN = 4.27`, `SUB_DIRECTORIES = src include examples doc`).
