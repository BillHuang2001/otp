# esock ttest driver scripts

## Intent
Standalone, manually-run drivers for the (e)socket *throughput tests* ("ttest") of the OTP `kernel` application, kept next to the Common Test suites in `lib/kernel/test/`.
They let a developer start a ttest server or client from a bash shell without going through Common Test.
They are developer tooling only — no test suite, Makefile or Erlang module invokes them.

## API Surface
- `esock-ttest` — the only real program: an Erlang escript (`#!/usr/bin/env escript`, 522 lines).
  - Usage: `esock-ttest --help | --server [opts] | --client <opts>`.
  - Common options: `--transport gen|gs|sock|sock:plain|sock:msg` (default `sock:plain`), `--active false|once|true` (server) or `false|once|true|1..32767` (client), `--async` (only meaningful for transport `sock`/`gs` with active =/= false), `--profile`.
  - `--transport`: `gen` = `gen_tcp`; `gs` = `gen_tcp` with socket backend; `sock` = `socket` (`:plain` = send/recv, `:msg` = sendmsg/recvmsg).
  - Server options: `--domain local|inet|inet6` (default `inet`); no mandatory options.
  - Client options: mandatory `--scon <addr>:<port>` or `<path>`, optional `--msg-id 1|2|3` (default `1`; 1 = small, 2 = medium, 3 = large), `--max-outstanding <N>` (defaults 100/10/1 for msg-id 1/2/3), `--runtime <secs>` (default 60).
  - Dispatches to the parent-dir helpers: `socket_test_ttest_tcp_server_{gen,gs,socket}:start/2,4` and `socket_test_ttest_tcp_client_{gen,gs,socket}:start/6,8`, then monitors the started process and waits for its `'DOWN'`.
  - `--profile` wraps the run in `socket_test_profile:profile/2` (fprof trace/`.fprof`/`.calltree` files named `socket_test_profile_<slogan>-<fts>-*` in the cwd).
  - All output goes through `socket_test_ttest_lib:format/2`.
- `esock-ttest-server-gen` / `esock-ttest-server-gs` / `esock-ttest-server-sock` — `/bin/sh` wrappers, one per fixed transport (`gen` / `gs` / `sock`), forwarding `--domain` and `--active` (plus `--async` for the sock variant) to `esock-ttest --server`.
- `esock-ttest-client` — `/bin/sh` wrapper; requires `--addr` and `--port`, optional `--msgid`, `--runtime` (default 30), `--profile`; runs a fixed 21-case matrix of transport × active (`gen|gs` × `false|true|once|1|5|10|20`, same for `sock` with `--async`) and builds `--scon <addr>:<port>` for each case.
- `.gitignore` — tracked but **empty** (0 bytes); a placeholder with no patterns, so it ignores nothing.

## Constraints
- Both `esock-ttest` and the three `esock-ttest-server-*` / `esock-ttest-client` wrappers must stay executable, and `esock-ttest` must keep `#!/usr/bin/env escript` as its first line.
- The wrappers build their paths from `$ERL_TOP` (`$ERL_TOP/lib/kernel/test/esock_ttest`); `ERL_TOP` must be set or they resolve to a bogus absolute path and fail.
- The escript sets up no code path (`-pa`, `ERL_FLAGS`) of its own, so the modules it calls (`socket_test_ttest_*`, `socket_test_profile`) must already be loadable — the documented usage is to `cd lib/kernel/test` first (that dir is `EBIN = .` in `../Makefile`, so its compiled `.beam` files are reachable) and then run `./esock_ttest/esock-ttest...`.
- POSIX only: the wrappers are `sh` scripts (no Windows equivalent), and neither CLI parses options for Windows paths.
- Option defaults are duplicated between the escript and the wrappers — change both together.

## Known Issues
- `esock_ttest/` is **not installed** by the parent `release_tests_spec`: `../Makefile` copies only `$(ERL_FILES)`, `$(HRL_FILES)`, `$(APP_FILES)`, the `*.spec`/`Emakefile` files, `*_SUITE_data`, `kernel_SUITE_data` and `../examples` into `$(RELEASE_PATH)/kernel_test`, so these scripts exist only in the source tree.
- All three `esock-ttest-server-*` wrappers validate `--domain` with `inet6` listed twice in the `elif` chain, so the documented `local` value is always rejected (`--help` advertises it).
- The escript accepts `--scon <path>` (Unix domain socket), but only the `sock` client clause matches a non-tuple server; `--transport gen --scon <path>` / `--transport gs --scon <path>` fall through to `usage("Unexpected option combo")`.
- A server `--domain local` only works with `--transport sock`: the `gen` and `gs` server clauses are guarded to `inet`/`inet6`.
- Server `--active` accepts only `false|once|true`; the client additionally accepts an integer `1..32767`.
- Wrapper and escript CLIs differ: the client wrapper uses `--msgid` while the escript uses `--msg-id`, and the client `--runtime` default is 30 (wrapper) vs 60 (escript).

## Notes for Agents
- The suites do not shell out to these scripts: no `esock-ttest`/`esock_ttest` reference exists outside this directory (verified repo-wide), so these scripts cannot break or be broken by CT runs.
- `?TTEST_MANAGER` in `../socket_ttest_SUITE.erl` is `esock_ttest_manager` — a suite-internal manager process, unrelated to these files.
- Run timeouts of the corresponding tests are scaled by the `ESOCK_TTEST_CONDITION` env var / `esock_ttest_condition` kernel app env, read in `../kernel_test_lib.erl`; the suite runtime can also be set via `ESOCK_TEST_TTEST_RUNTIME`.
- `--profile` output files land in the current working directory (not in this directory), so they are not covered by the empty `.gitignore`.

## See Also
- `../` — the flat `lib/kernel/test/` suite directory; `socket_test_ttest_*.erl` / `socket_test_ttest*.hrl` are the modules these scripts drive (read-only from here).
- `../socket_ttest_SUITE.erl`, `../socket_traffic_SUITE.erl`, `../socket_sctp_SUITE.erl` (+ `socket_sctp_traffic_*`) — the suites that exercise the same ttest/traffic code in-process (read-only from here).
- `../esock_misc/` — sibling helper Erlang modules for esock tests (read-only; escalate writes to the parent).
- `../../../CONTEXT.md` and `../../CONTEXT.md` — kernel app / test directory context.
