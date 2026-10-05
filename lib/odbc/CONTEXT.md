# odbc — Erlang ODBC Application

## Intent
Erlang interface to relational SQL databases via Microsoft ODBC 3.0, exposed by the `odbc` module (`connect/2`, `sql_query`, `param_query`, scrollable-cursor navigation, transactions).
A thin Erlang control layer (`src/`) drives a C port program (`c_src/odbcserver.c`) that calls the system ODBC driver manager.
**Status: DEPRECATED** — the whole application is deprecated and planned for removal (`doc/notes.md`, OTP-19980); every `odbc` function is marked `-deprecated` in `src/odbc.erl`.

## API Surface
- Module `odbc` (`src/odbc.erl`, 1417 lines) — the public API and a `gen_server` (one process per connection):
  `start/0,1`, `stop/0`, `connect/2`, `disconnect/1`, `commit/2,3`, `sql_query/2,3`, `select_count/2,3`, `first/1,2`, `last/1,2`, `next/1,2`, `prev/1,2`, `select/3,4`, `param_query/3,4`, `describe_table/2,3`; internal `start_link_sup/1`.
- Exported types: `connection_reference/0` (opaque `pid()`), `col_name/0`, `row/0`, `selected/0` (`{selected, Cols, Rows}`), `updated/0` (`{updated, NRows}`), `n_rows/0`, `odbc_data_type/0` (`sql_integer | {sql_varchar, Size} | sql_wchar | ...`), `common_reason/0`, `extended_error/0` (`{ODBCState, NativeCode, Reason}`).
- `connect/2` options: `{auto_commit, on|off}`, `{timeout, timeout()}`, `{binary_strings, on|off}`, `{tuple_row, on|off}`, `{scrollable_cursors, on|off}`, `{trace_driver, on|off}`, `{extended_errors, on|off}`, `{max_long_column_size, 0..4294967295}`.
- Module `odbc_debug:trace_odbc/2` — turns `dbg` tracing on/off for a connection process (exported functions only; not listed in the `.app` `modules`).
- Public header `include/odbc.hrl` — deprecated SQL_* result/attribute constants (kept static).
- `doc/` ships User's Guide guides (`introduction`, `getting_started`, `databases` data-type mapping, `error_handling`) and `notes.md` release notes.

## Architecture / Design Decisions
- Per-connection `gen_server` (module `odbc`) started as a `simple_one_for_one` temporary child of `odbc_sup`; only the owning process may issue requests (`process_not_owner_of_odbc_connection` otherwise).
- The C port program `odbcserver` is spawned as an external executable located via `os:find_executable("odbcserver", priv/bin)`; if absent, `connect/2` returns `{error, port_program_executable_not_found}`.
- Erlang↔C protocol: 4-byte packet-size-framed control messages on the port plus **two loopback TCP sockets** (a supervisor socket for out-of-band messages and an ODBC data socket) — bulk result data is not carried over stdin/stdout.
- The command/exit-code protocol constants are duplicated in `src/odbc_internal.hrl` and `c_src/odbcserver.h` and MUST stay in sync.

## Constraints
- Requires a system ODBC driver manager at build time: unixODBC (`-lodbc`) or iODBC (`-liodbc`), probed via the `SQLAllocHandle` symbol, plus `sql.h`/`sqlext.h` headers.
- Has its own handcrafted autoconf (`configure.ac`, requires autoconf >= 2.71) run by the top-level `configure`; `configure` (generated) must not be edited directly — edit `configure.ac` and regenerate.
- `ODBC_CAN_BUILD_DRIVER` (set by `configure`, substituted into `subdirs.mk`) gates the `c_src` build: when false, only `src` and `doc` are built and no `priv/bin/odbcserver` exists.
- Default is `--with-odbc` (build the driver); if the library/headers are missing, configure errors out unless `--without-odbc` is given, and only then sets `ODBC_CAN_BUILD_DRIVER=false` and writes the reason to `CONF_INFO` (git-ignored).
- Header-check and link-check failures are recorded in `CONF_INFO` (see `.gitignore`); a stale `SKIP` file is cleared by configure.
- Build order/versioning follow the standard OTP app convention (`vsn.mk` `ODBC_VSN = 2.17.1`, `Makefile` includes `make/app_targets.mk`).

## Dependencies
- Application deps (`src/odbc.app.src`): `kernel`, `stdlib` only; `runtime_dependencies` = `stdlib-2.0`, `kernel-3.0`, `erts-6.0`.
- Native deps: system ODBC driver manager (unixODBC/iODBC on Unix, `odbc32` on Windows), `erl_interface` (`-lei`) for the port entry point, and a pthread/thr library (`LM_CHECK_THR_LIB`).
- Runtime deps: a configured ODBC DSN/driver and access to the target database.

## Test Strategy
- Common Test suites in `test/`: `odbc_start_SUITE` (app/appup/start), `odbc_connect_SUITE`, `odbc_query_SUITE`, `odbc_data_type_SUITE`; `odbc_test_lib` provides helpers, `odbc_test.hrl` holds `?RDBMS`/timeouts.
- Suites skip when unsupported: `odbc_test_lib:skip/0` (e.g. SLES 9, non-supported Solaris) and `odbc_check/0` (macOS, non-64-bit non-Linux), plus skip when `code:which(odbc) == non_existing` ("No ODBC built").
- DB-specific behavior is isolated behind callback modules selected by `?RDBMS` (`postgres.erl`, `mysql.erl`, `sqlserver.erl`, `oracle.erl`) each defining `connection_string/0`; `test/README` documents required setup (installed DB + driver, `.odbc.ini`/ODBC DSN).
- Tests create and drop their own tables and cannot run without a real database and driver — they are not hermetic; `odbc.spec`/`odbc.cover` drive CT and coverage.

## Notes for Agents
- `src/odbc_debug.erl` exists but is intentionally not in the `.app` `modules` list.
- macOS uses iODBC/Homebrew paths (`/usr/local`); Windows uses `odbc32`/`odbcserver.exe` with `erl_port_entry`.
- `--without-odbc` produces an installable app whose Erlang modules load but whose `connect/2` fails at runtime (no `odbcserver` binary).

## Routing Table
| Path | Contents |
| --- | --- |
| `src/` | Erlang sources: `odbc.erl` (API + gen_server), `odbc_app.erl`, `odbc_sup.erl`, `odbc_debug.erl`, `odbc_internal.hrl` (protocol constants), `odbc.app.src`, `odbc.appup.src` |
| `include/` | Public header `odbc.hrl` (deprecated SQL_* constants) |
| `c_src/` | C port program `odbcserver.c`/`odbcserver.h` (ODBC API driver); `Makefile.in` → per-host `Makefile` (generated by configure) |
| `test/` | Common Test suites (`odbc_*_SUITE.erl`), `odbc_test_lib.erl`, `odbc_test.hrl`, RDBMS callback modules (`postgres/mysql/sqlserver/oracle.erl`), `odbc.spec`, `odbc.cover`, `README` |
| `doc/` | `docs.exs`, `guides/` (User's Guide), `notes.md` (release notes), `src/specs.xml`, `assets/` |
| `priv/` | Build output only: `bin/` (compiled `odbcserver`), `obj/<target>/` (object files); only `.gitignore` tracked |
| `ebin/` | Erlang build output (only `.gitignore` tracked) |
| `configure` / `configure.ac` | App-specific autoconf that locates the system ODBC library/headers (invoked by top-level `configure`) |
| `Makefile` / `subdirs.mk` / `vsn.mk` | Standard OTP app build wiring; `subdirs.mk` selects `src c_src doc` vs `src doc` via `ODBC_CAN_BUILD_DRIVER` |

## See Also
- `../CONTEXT.md` — OTP library applications overview and build conventions.
- `../erl_interface/` — provides `-lei` linked into the port program.
- `../../system/` — doc build used by `doc/docs.exs`.
