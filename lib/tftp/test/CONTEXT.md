# tftp — Test Suite

## Intent
Common Test suite for the `tftp` application (`../src/`), covering the client/server API (`tftp:read_file/3`, `tftp:write_file/3`, `tftp:start/1`, `tftp:info/1`, `tftp:change_config/2`), the option handling, block-size (`blksize`) and `timeout` option negotiation, packet re-send behaviour, the `root_dir` file-access restriction, and app/appup file validity.
Tests exercise a daemon started in-process by the suite itself, so no external TFTP server, network peer or fixed port is required.

## API Surface (files)
- `tftp_SUITE.erl` — the only suite (~1050 lines); `-compile([export_all, nowarn_export_all])`; top case `all/0` = `simple, root_dir, extra, reuse_connection, resend_client, resend_server, large_file, app, appup`; `groups/0` is empty.
- `tftp_test_lib.erl` — helper library: `try_catch/4` (used through `?TRY/1`) and a `generic` callback module implementing the `tftp` behaviour via a per-callback fun state record (see Failure Injection).
- `tftp_test_lib.hrl` — defines `?TRY(Expr)`, which wraps `Expr` in `tftp_test_lib:try_catch/4` and normalises exceptions to `{'EXIT'|'ERROR'|'THROW', Reason}` tuples matched by the suite.
- `Makefile` — builds/installs the test modules (`MODULES = tftp_SUITE tftp_test_lib`) and test data; `RELTESTSYSDIR = $(RELEASE_PATH)/tftp_test`.
- `tftp.spec` — CT spec `{suites,"../tftp_test", all}.` (run from an installed release tests dir).
- `tftp.config` — CT config file, empty (`[].`).
- `tftp.cover` — coverage spec `{incl_app,tftp,details}.`.

## Test Cases
- `simple` — start daemon, invalid config (`{host,4711}`), read-failure, then binary and file write+read round-trip.
- `root_dir` — daemon with `{callback,{"",tftp_file,[{root_dir,RootDir}]}}`; verifies reads/writes outside `RootDir` return `{client_open,badop,_}` and missing files return `{client_open,enoent,_}`.
- `extra` — exercises client-side option negotiation and the generic callback (checks returned `{count,_}` / `{bin,_}` and peer).
- `reuse_connection` — same client re-sends the initial READ/WRITE request; the daemon must reuse the ongoing connection.
- `resend_client` — raw UDP: client re-sends packets (ACKs) to force a server re-send.
- `resend_server` — raw UDP: server re-sends packets (DATA/OACK) and the real `tftp` client must tolerate it.
- `large_file` — transfer a >32 MB file (>65535 blocks, exercises block-number wraparound).
- `app` / `appup` — validate the `.app` / `.appup` files via `test_server:app_test/1` / `test_server:appup_test/1`.

## Test Strategy
- Framework: Common Test, with the `ts_install_cth` CT hook (`suite/0`).
- Daemon lifecycle: tests start a daemon with `tftp:start([{port,0}|Options])` and read the real port back with `tftp:info/1` via the `?START_DAEMON/1` macro; port `0` means "kernel picks a free port".
- `init_per_testcase`/`end_per_testcase` both call `application:stop(tftp)` (each case starts/stops the app itself where needed).
- Transfers use relative filenames in the CT run cwd and clean up after themselves; `root_dir` case uses `priv_dir` from the CT config via `get_conf/2`.
- Re-send tests open raw `gen_udp` sockets and hand-craft TFTP packets (RRQ/WRQ/DATA/ACK/OACK) with a 3 s default / 1 s short timeout to simulate loss; they are timing-sensitive.
- Run: `make test` in `lib/tftp/`, or `ct_run`/`ct` with `tftp.spec` from the installed `$(RELEASE_PATH)/tftp_test`.

### Failure Injection
- `tftp_test_lib` can be used as a `tftp` callback module: pass `{callback,{Regex,tftp_test_lib,Options}}` where `Options` is a proplist of `{state,_}`, `{prepare,Fun/6}`, `{open,Fun/6}`, `{read,Fun/1}`, `{write,Fun/2}`, `{abort,Fun/3}`; the `generic` functions dispatch to those funs and thread a caller-owned state term.
- This lets a test inject arbitrary behaviour (errors, custom data, timing) into the prepare/open/read/write/abort callbacks; the `extra` case drives it with its own `#extra_state{}` funs.

## Constraints
- Tests must not bind or depend on the fixed TFTP port 69 — always start the daemon on port `0` and query the real port.
- Tests must not depend on external hosts; peers are `{127,0,0,1}` or the `"127.0.0.1"` string.
- The `root_dir` case is security-relevant: it asserts path-traversal attempts (`../`) and out-of-root paths are rejected with `badop`; keep those assertions when editing.
- Files created by tests live in the CT run cwd / `priv_dir` and must be cleaned up (tests delete them explicitly).
- Only the modules listed in `MODULES` are compiled/tested; new suite helpers must be added there.

## Known Issues / Notes for Agents
- `Makefile` sets `TFTP_DATADIRS = tftp_SUITE_data`, but no `tftp_SUITE_data/` directory exists in the repo (the data-dir install loop in `release_spec`/`release_tests_spec` is therefore effectively a no-op).
- The `-Dtftp__data_dir` / `-Dtftp_priv_dir` compile defines (from `TFTP_DATA_DIR`/`TFTP_PRIV_DIR`, defaulting to `/ldisk/tests/$(USER)/tftp/...`) are built but never referenced by the current suite — dead configuration.
- The `test/Makefile` debug flag logic is broken/quirked: `TFTP_DEBUG_CLIENT` is defaulted to `y` before the test that would add `-Dtftp_debug_client` (so that flag is never added), and the `ifneq ($(TFTP_DEBUG),)` block assigns `TFTP_DEBUG = s` instead of leaving it, so only `-Dtftp_trace_client` (with `TFTP_TRACE_CLIENT=y`) and `-Dtftp_log`/`-Dtftp_debug` (with `TFTP_DEBUG=l`/`d`) are reachable.
- `emakebuild` globs `'*_SUITE_make'` for suite-specific Emakefile fragments, but no such file exists and `MODULES` lists `tftp_SUITE`; with `make_emakefile` present the suite is still compiled through the `$(MODULES)` invocation.
- `resend_*` and `reuse_connection` rely on real wall-clock timeouts (`timer:seconds`), so they are slow and prone to flakiness on loaded hosts.
- `resend_client` starts the daemon with `{debug,all}`; the raw-packet tests are sensitive to the negotiated `blksize` branches (512 vs. option-negotiated sizes 10/1025).
- The suite (not Makefile) owns the daemon; `?TRY` only logs and normalises exceptions, it does not fail a test by itself — assertions are the pattern matches.

## Dependencies
- Requires the `common_test` and `test_server` frameworks and the built `tftp` application (`-pa ../tftp/ebin`).
- Raw UDP tests need loopback networking only.

## See Also
- `../src/tftp.erl` — the tested API and the `tftp` behaviour (`prepare/6`, `open/6`, `read/1`, `write/2`, `abort/3`).
- `../src/tftp_file.erl` — default callback module implementing `root_dir`.
- `../CONTEXT.md` — the tftp application overview.
