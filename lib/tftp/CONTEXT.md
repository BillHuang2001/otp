# tftp — OTP TFTP Application

## Intent
The OTP **tftp** application: a standalone TFTP client and server (RFC 1350) with the option extensions RFC 2347 (options), RFC 2348 (blksize) and RFC 2349 (timeout/tsize), split out of `inets` in OTP 18 (`lib/inets/CONTEXT.md` documents that FTTP/TFTP are no longer part of `inets`).
A daemon process listens on UDP and spawns a temporary server process per request; the client functions spawn a temporary client process per transfer.
File access is pluggable through the `tftp` behaviour; `tftp_file` (real files) and `tftp_binary` (in-memory payloads) are the built-in callbacks.
App version is `TFTP_VSN = 1.3` in `vsn.mk`; the release-wide version mapping is the repo-root `otp_versions.table`.
Listed in `lib/Makefile` as an `ALL_ERLANG_APPLICATIONS` app — it is only built by `make` in `lib/` or `otp_build -a`, not by a plain `make` at the repo root.

## API Surface (public)
- `tftp:read_file(RemoteFilename, LocalFilename, Options)` → `{ok, LastCallbackState} | {error, Reason}` — client download; `LocalFilename` may be a filename or the atom `binary` (selects `tftp_binary`).
- `tftp:write_file(RemoteFilename, LocalFilename, Options)` → `{ok, LastCallbackState} | {error, Reason}` — client upload; `LocalFilename` is a filename or a binary payload.
- `tftp:start(Options)` → `{ok, Pid} | {error, Reason}` — start a daemon on `{port, Port}` (default 69; port `0` picks a free port, recoverable via `info/1`). There is **no** `read_file/1`.
- `tftp:info(Pid)` and `tftp:change_config(Pid, Options)` — inspect/alter a running daemon; `tftp_engine` additionally accepts the atoms `daemons` and `servers` for all instances.
- `tftp:start/0` / `tftp:stop/0` — undocumented convenience wrappers around `application:start/stop(tftp)`.
- Options (type `tftp:connection_option/0`): `debug`, `host`, `port`, `port_policy`, `udp`, `use_tsize`, `max_tsize`, `max_conn`, `reject`, `callback`, `logger`, `max_retries`, plus raw TFTP options `{Key, Value}`.
- Behaviours defined here: `tftp` (`prepare/6`, `open/6`, `read/1`, `write/2`, `abort/3`) and `tftp_logger` (`error_msg/2`, `warning_msg/2`, `info_msg/2`, all optional, default `tftp_logger` over `logger`).
- `vsn.mk` — defines `TFTP_VSN` (consumed by the app's `Makefile`s); `Makefile` — `SUB_DIRECTORIES = src doc` and the app-level release/dialyzer targets.

## Layout
- Standard pure-Erlang application: `src/`, `test/`, `doc/`, `ebin/` (build output; only a `.gitignore` is tracked), `vsn.mk`, `Makefile`, `AUTHORS`.
- No `include/`, `c_src/`, `priv/` or `configure` (no C or OS dependencies); `priv/plt` is created on demand by the `make dialyzer` target in `../../../make/app_targets.mk`.
- `src/tftp.app.src` declares `mod {tftp_app, []}`, `applications [kernel, stdlib]`, `runtime_dependencies ["erts-6.0","stdlib-5.0","kernel-6.0"]`, `env []`, and the 8 modules; `src/tftp.appup.src` restarts the whole application on every upgrade.

## Constraints
- Dependencies are `kernel` and `stdlib` only (`gen_udp`, `inet`, `file`, `filelib`, `ets`, `proc_lib`, `sys`, `logger`, `re`, `rand`).
- `%VSN%` in `.app.src`/`.appup.src` is substituted at build time; never hardcode the version.
- Documentation is generated from `-moduledoc`/`-doc` attributes in the sources plus the guides listed in `doc/docs.exs` (Markdown + ExDoc); the `src` and `doc` `Makefile`s contain no hand-written doc sources.
- Callback/lifecycle changes must keep the `tftp` behaviour contract in `src/tftp.erl` intact, since user callbacks are dispatched by module and by filename-regexp match.
- `application:start(tftp)` alone starts an **empty** supervisor: daemons come from the `services` environment key (`[{tftpd, Options}]`), which the app does not default.
- `ebin/` and generated `tftp.app`/`tftp.appup` are build artifacts — not edited by hand.

## Known Issues
- The default `tftp_file` callback applies no path restriction, so a server started without `{callback, {"", tftp_file, [{root_dir, Dir}]}}` exposes any file the VM user can read/write — the documented default-server risk (CVE-2026-21620) with `root_dir` (absolute) as the remedy; see `doc/notes.md`, `doc/guides/introduction.md`.
- "netascii" mode is accepted as a mode string but performs no CRLF translation; it is rejected with `badop` on LF platforms unless `{native_ascii, true}` is forced.
- Client error terms have two different shapes: `{error, {badarg, {Key, Val}}}` for bad options and `{error, {Where, Code, Text}}` (internal `Where` tag) for protocol/callback failures — the test suite matches both exactly.

## Notes for Agents
- `tftp:start/1` (used by tests and most users) starts a daemon linked to the caller via `proc_lib`; the `tftp_sup` supervisor path is only used when the application is started with a `services` env list.
- `src/tftp_engine.erl` is intentionally monolithic (~1459 lines) — see `src/CONTEXT.md` for its internal function groups before editing.
- `tftp_engine` honours the `-tftpd_<Port> <Fd>` command-line argument (prebound socket fd for `setuid_socket_wrap`).

## Test Strategy
- Common Test suite in `test/` (`tftp_SUITE.erl` + `tftp_test_lib.erl`); run `make test` in this directory (or `ct_run` with `test/tftp.spec`).
- The suite starts daemons itself on port `0` and uses only loopback, so it needs no external TFTP server.
- Cases cover option negotiation, `root_dir` traversal rejection, connection reuse, packet re-send, a >32 MB file (block-number wraparound), and `.app`/`.appup` validity; the re-send cases are timing-sensitive.
- See `test/CONTEXT.md` for known quirks of the test `Makefile` (unused `tftp_SUITE_data` dir, dead `-Dtftp__data_dir`/`-Dtftp_priv_dir` defines, broken debug-flag logic).

## Routing Table
- `src/` → the application's Erlang modules, `tftp.hrl`, `.app.src`/`.appup.src` and the source `Makefile`; module roles, the `tftp` behaviour and engine internals are documented there.
- `test/` → Common Test suite, test helper library and the `tftp` behaviour failure-injection callback.
- `doc/` → docs entry point: `docs.exs` (ExDoc config, `extras` = the two guides), `guides/introduction.md` (overview + security considerations), `guides/getting_started.md` (server/client example), `notes.md` (release notes / security fixes), `Makefile` (includes `../../../make/doc.mk`).
- `vsn.mk` / `Makefile` / `AUTHORS` / `ebin/` → version, build and release wiring (see above); `ebin/` is build output only.
- `../CONTEXT.md` → OTP library-application conventions and the app build list (read-only).

## See Also
- `../../../system/doc/top/` and `scripts/run-dialyzer` reference `tftp` in the doc/dialyzer app lists.
- `erts/test/upgrade_SUITE.erl` covers the historical `inets` → `ftp`/`tftp` application split during upgrade.
- `../ftp/` — the sibling FTP application (also split out of `inets`).
