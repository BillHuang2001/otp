# ftp — FTP Client Application

## Intent
OTP application `ftp` (`FTP_VSN = 1.2.6` in `vsn.mk`) implementing a client for a subset of the File Transfer Protocol (RFC 959), including FTP over TLS (FTPS, RFC 4217).
It is a client only — no FTP server and no FTP proxy is provided.
The application is small: one public module (`ftp`), four internals, documentation and a test suite.

## API Surface
- `src/ftp.erl` — the only public module. Client API: `open/1,2`, `close/1`, `cd/2`, `lcd/2`, `pwd/1`, `lpwd/1`, `ls/1,2`, `nlist/1,2`, `mkdir/2`, `rmdir/2`, `delete/2`, `rename/3`, `send/2,3`, `send_bin/3`, `send_chunk_start/2`, `send_chunk/2`, `send_chunk_end/1`, `append/2,3`, `append_bin/3`, `recv/2,3`, `recv_bin/2`, `recv_chunk_start/2`, `recv_chunk/1`, `type/2`, `user/3`, `quote/2`, `formaterror/1`.
- Public types: `ftp:client() :: pid()` (opaque-ish handle returned by `open/1,2` and required by every other call); documentation groups are `Connection API`, `Update API`, `Info API`.
- Hidden/undocumented helpers exported for internal use: `ftp:start/0`, `ftp:stop/0`, `info/1`, `latest_ctrl_response/1` (still exported).
- `open/2` options: `{host,H}`, `{port,P}` (0 → 21, or 990 with `{tls_sec_method,ftps}`), `{mode,passive|active}` (default `passive`), `{verbose,Bool}`, `{debug,disable|debug|trace}`, `{ipfamily,inet|inet6|inet6fb4}`, `{timeout,T}`, `{dtimeout,T}`, `{tls,TLSOptions}`, `{tls_sec_method,ftpes|ftps}`, `{tls_ctrl_session_reuse,Bool}`, `{sock_ctrl,...}`, `{sock_data_act,...}`, `{sock_data_pass,...}`, `{progress,ignore|{M,F,InitData}}`.
- Client always attempts passive mode first and falls back to active mode unless `mode` says otherwise.
- `{progress,{M,F,Init}}` drives a middleman process (`ftp_progress`) calling `M:F(Term, File, {file_size,_}|{transfer_size,_})` so user callbacks cannot disturb the transfer.
- Error reasons returned as `{error, Reason}` atoms: `echunk, eclosed, econn, ehost, elogin, enotbinary, epath, etype, euser, etnospc, epnospc, efnamena`; `formaterror/1` (via `ftp_response:error_string/1`) renders them.
- `src/ftp_internal.erl` — `gen_server` holding the whole protocol implementation (~2.5k lines) and `#state{}`: `csock`/`dsock` tagged as `{tcp|ssl, Socket}`, `tls_options`, `mode`, `type`, `data`/`ctrl_data` buffers, `caller`, `client`, `sockopts_*`, `progress`, `tls_ctrl_session_reuse`, `ftp_extension`.
- `src/ftp_response.erl` — multi-line FTP response parsing (`parse_lines/3`, `interpret/1`) and error-string mapping.
- `src/ftp_sup.erl` — `simple_one_for_one` supervisor (intensity 0, period 3600) starting `ftp_internal` workers as `temporary` children via `start_child/1`.
- `src/ftp_app.erl` — application callback; starts/stops `ftp_sup`.
- `src/ftp_internal.hrl` — shared protocol character macros (`?CR`, `?LF`, `?CRLF`, `?SP`, `?TAB`, quotes, parens); also copied into `test/` for suites.

## Constraints
- Only `ftp.erl` is documented as a public module; every other module is `-moduledoc false` — new internal modules must stay hidden.
- Calls use the `client()` pid from `open`; the client process monitors its creator and terminates when the creator dies, so all calls must come from the creating process.
- Transport is abstracted behind the `{tcp|ssl, Socket}` tag in `#state{}`; both plain FTP and FTPS must go through the same code path.
- The whole `ftp` module is currently marked `-deprecated` ("Legacy protocol support will be dropped in OTP-30"); `start_service/1` and `stop_service/1` are `-removed`.
- Passive-mode data connections validate the IP returned in the PASV/EPSV response against the control connection peer (SSRF / FTP-bounce protection).
- Build inputs are generated: `src/ftp.app` and `src/ftp.appup` come from `*.app.src`/`*.appup.src` by `%VSN%` substitution — never edit the generated files.
- `src/` compiles with `+warn_unused_vars`, `-I$(ERL_TOP)/lib/kernel/src`, `-pz $(ERL_TOP)/lib/public_key/ebin`, `-DVSN`.
- No `configure` script: this app has no C code, NIFs or platform detection.

## Dependencies
- `src/ftp.app.src`: `applications = [kernel, stdlib]`; `mod = {ftp_app, []}`; `registered = []`.
- `runtime_dependencies = ["erts-7.0","stdlib-3.5","kernel-6.0","ssl-10.2","runtime_tools-1.15.1"]` — `ssl` and `runtime_tools` are needed for FTPS and debug support but are *not* listed in `applications`.

## Design Decisions
- `ssl` is deliberately not an application dependency: `ftp_internal` does `catch ssl:start()` while establishing the TLS connection, so plain-FTP users never pull in `ssl`/`public_key`.
- `runtime_tools` is listed as a runtime dependency because `{debug, debug|trace}` uses the `dbg` toolkit.
- TLS options are passed through verbatim to `ssl:connect/3` (control and data connections); `tls_ctrl_session_reuse` copies the control channel TLS session onto the data channel, as required by some servers (e.g. vsftpd).
- Progress reporting runs in a separate linked process so a crashing/slow user callback degrades to `ignore` instead of breaking the session.

## Known Issues
- Results of `ls/1,2` and `nlist/1,2` depend heavily on the remote server: real errors are often not reported as errors by `ls`, and some servers respond with an error for `nlist` of an existing but empty directory.
- Remote file size is not determinable platform-independently, so `{file_size, unknown}` is passed to progress callbacks for remote files.
- The `ftp` module is deprecated in favour of SFTP; expect removal of legacy protocol support in OTP-30.
- FTPS interop depends on the peer: servers that require TLS session reuse on the data channel need `{tls_ctrl_session_reuse,true}`.

## Test Strategy
- `test/ftp_SUITE.erl` (main suite, ~1.5k lines) needs an external `vsftpd` binary found via `os:find_executable/1`; if none is found the whole suite skips with `"No ftp server found"` (also skips if the server starts but is unusable).
- The suite starts `vsftpd` on `localhost:9999` with anonymous access, using `ftp_SUITE_data/vsftpd.conf` plus generated certs; test groups cover passive/active, `ftpes`/`ftps` (explicit vs implicit TLS) and TLS session reuse variants.
- `test/ftp_format_SUITE.erl` is self-contained (response parsing / `formaterror`, uses `ftp_internal.hrl`) and runs without any FTP server — the only always-runnable suite.
- `test/ftp_test_lib.erl` generates PEM test certificates via `public_key:pkix_test_data/1` for the TLS groups.
- `test/property_test/` is experimental QuickCheck code (EQC license assumed, `#PROPER` alternative) run manually — it is *not* part of `test/Makefile`'s `MODULES`.
- `test/Makefile` defines the release test dir `$(RELEASE_PATH)/ftp_test` and `FTP_DATA_DIR`/`FTP_PRIV_DIR` (defaults under `/ldisk/tests/$(USER)/ftp`) plus `FTP_DEBUG*`/`FTP_TRACE_CLIENT` debug macros.
- `test/ftp.spec` = `{suites,"../ftp_test", all}`; `test/ftp.cover` = `{incl_app,ftp,details}`; `test/ftp.config` is empty.
- `test/ftp_SUITE_data/ftpd_hosts.skel` is a leftover template listing hosts per platform for the legacy external-server test infrastructure.

## Notes for Agents
- Read `src/ftp.erl` for the public contract (its `-doc` blocks are the authoritative option/error documentation) and `src/ftp_internal.erl` for the protocol state machine; do not read the whole of `ftp_internal.erl` at once (~2.5k lines).
- `doc/notes.md` holds the release notes (per-version change history) — use git history rather than the file itself for archaeology.
- `doc/archive/` contains the plain-text RFCs (959, 2428, 2577) referenced by the implementation.

## Routing Table
- `src/` → implementation: public `ftp` module, `ftp_internal` protocol `gen_server`, `ftp_response` parser, `ftp_progress`, `ftp_sup`, `ftp_app`, `ftp.app.src` / `ftp.appup.src`
- `test/` → Common Test suites (`ftp_SUITE`, `ftp_format_SUITE`), test helper `ftp_test_lib`, external `vsftpd` config/certs data, experimental QuickCheck suite in `property_test/`
- `doc/` → ExDoc config (`docs.exs`), guides (`introduction.md`, `ftp_client.md`), release notes (`notes.md`), archived RFCs
- `Makefile` / `vsn.mk` → build wiring (`SUB_DIRECTORIES = src doc`, `VSN = $(FTP_VSN)`, `DIA_PLT_APPS = runtime_tools ssl`)
- `../ssl/`, `../public_key/`, `../runtime_tools/` → runtime prerequisites for FTPS and `{debug,...}` (sibling apps — read-only, escalate writes to `lib/`)

## See Also
- `../CONTEXT.md` → application conventions shared by all `lib/<app>` directories
- `../inets/` → HTTP/FTP server-side services (distinct from this FTP client)
