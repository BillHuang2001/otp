# inets — Internet Services

## Intent
The `inets` application provides the Internet protocol services that ship with Erlang/OTP: an HTTP/1.1 client (`httpc`), an HTTP/1.1 web server (`httpd`), and the app-level "service" concept that starts/stops them either standalone or under the app supervision tree.
The description string is `"INETS  CXC 138 49"`; version is `INETS_VSN = 9.8` in `vsn.mk` (`APP_VSN = "inets-9.8"`).
FTP and TFTP are NOT part of this application — they are the separate `lib/ftp` and `lib/tftp` apps, so `inets:service_names/0` returns only `[httpc, httpd]`.
`examples/server_root/` ships a runnable sample server root demonstrating the server's features; documentation is ExDoc-generated from the guides here plus `-moduledoc`s in the sources.

## API Surface
Application-level (the module `inets`, ExDoc group "Service API"):
- `inets:start/0,1` start the `inets` application; `start/2,3 (Service, Config, How)` starts an individual service with `How :: inets | stand_alone`.
- `inets:stop/0` and `stop(How, Pid)`; `services/0`, `services_info/0`, `service_names/0` for introspection; `versions/0` for component versions.
- `inets:enable_trace/2,3`, `disable_trace/0`, `set_trace/1`, `report_event/4` are exported but `-doc false` — internal debug tooling built on `dbg`.
- The app env key `services` — read by `inets_sup` via `application:get_env(inets, services)`, i.e. set by the user in `sys.config`, not declared in `inets.app.src` — accepts `{httpc,Conf}`, `{httpd,Conf}` or `{httpd,Address,Port}` entries.
HTTP client (`httpc`, in `src/http_client/`):
- `httpc:request/1,2,4,5` (there is no `request/3`), `cancel_request/1,2`, `stream_next/1`; options via `set_option(s)`/`get_option(s)` in four scopes (default | profile | session | request).
- `httpc` profiles/sessions: `default_profile/0`, `profile_name/1,2`, `which_sessions/0,1`, `info/0,1`, `ssl_verify_host_options/1`; cookies via `store_cookies/2,3`, `cookie_header/1,2,3`, `which_cookies/0,1`, `reset_cookies/0,1`.
HTTP server (`httpd`, in `src/http_server/`):
- `httpd:start/1`/`serve/1` (standalone; `erl -S httpd serve --directory --port --bind`), `inets:start(httpd, Config)` for managed startup, `stop/1`, `info/1..4`, `reload_config/2`, and deprecated `parse_query/1` (use `uri_string:dissect_query/1`).
- Config is an Erlang proplist (or `{proplist_file, Path}`); Apache-style config file syntax is NOT supported.
Extension points / behaviours:
- `inets_service` (`src/inets_app/inets_service.erl`) — implemented by `httpc` and `httpd`: `start_standalone/1`, `start_service/1`, `stop_service/1`, `services/0`, `service_info/1`.
- The `mod_*` request-handler behaviour is declared in `httpd.erl` itself via `-callback do/1`, `store/2`, `remove/1` — there is NO `httpd_mod` module (shipped `mod_*` modules do not even carry a `-behaviour` attribute).
- `mod_esi` declares the `-callback 'Function'/3` for ESI scripts plus `mod_esi:deliver/2`; `httpd_custom_api` is the behaviour for custom response modules (default impl `httpd_custom`, enabled by the `customize` property); `mod_security` declares `-callback event/4`.
Public headers in `include/`: `httpd.hrl` (records `init_data`, `mod` — the data handed to `mod_*` handlers) and `mod_auth.hrl` (records `httpd_user`, `httpd_group` for auth backends).

## Constraints
- `Makefile` sets `SUB_DIRECTORIES = src examples doc` (test suites are run through the OTP test harness, not this Makefile) and declares `DIA_PLT_APPS = runtime_tools mnesia ssl` for Dialyzer.
- All source lives in `src/{inets_app,http_lib,http_client,http_server}` and compiles into the single shared `ebin/`; a new module must be added to the subdirectory's `MODULES` list and to the `modules` list of `src/inets_app/inets.app.src`.
- Never edit `ebin/` contents, generated `ebin/inets.app`/`inets.appup` (sed-generated from the `.src` templates) or the generated `deps/*.d`/`.depend` files.
- `vsn.mk` is owned by this app only; bump `INETS_VSN` there — the release-wide mapping lives in the repo-root `otp_versions.table`.
- `include/` headers are the public, installed copies; `src/http_server/*.hrl` only re-include them with legacy guards — change the public copy, not the wrapper.

## Design Decisions
- `src/` is split by protocol role (client vs server) with a shared `http_lib` layer, so wire-format parsing/encoding is implemented once for both endpoints.
- Services are pluggable: `httpc`/`httpd` can run standalone (`:start_link/1`) or as supervised children of `inets_sup` (`*_sup:start_child/1`), selected by the `How` argument — this is why the `inets_service` behaviour exists.
- `httpd` supports listening on a pre-opened file descriptor (`-httpd_<Port> <Fd>` inherited socket / argv) so an unprivileged process can serve a privileged port via a socket wrapper.
- `httpc` keeps one `httpc_manager` gen_server per profile (ETS-backed) and one `httpc_handler` per connection; handlers are started with `proc_lib:start_link` + `enter_loop` so a blocking `gen_tcp:connect` cannot stall the manager.
- Request handlers for the server are started `temporary` under a `simple_one_for_one` `httpd_connection_sup`, giving connection-per-process isolation with no restart storms.

## Dependencies
- Hard runtime dependencies: `kernel`, `stdlib` (`{applications,[kernel,stdlib]}` in `inets.app.src`).
- `runtime_dependencies` declared: `stdlib-6.0`, `stdlib-5.0`, `ssl-9.0`, `runtime_tools-1.8.14`, `mnesia-4.12`, `kernel-9.0`, `erts-14.0`, `public_key-1.13`.
- HTTPS/TLS needs `ssl` + `public_key` + `crypto`; note the `inets.app.src` comment: when the "new" ssl is used, `crypto` must be started before `inets`.
- `runtime_tools` and `mnesia` are used by tooling/optional features (`mnesia` for the `mod_auth_mnesia` backend and some tests); `mnesia`/`public_key`/`ssl` are also the Dialyzer PLT apps.
- A C compiler is needed to build the test data directories (CGI demo sources and the setuid socket wrapper).

## Test Strategy
- Suites live in `test/` and are run with `make test` from this directory (or `ct_run -spec inets.spec`); the bench suite uses `inets_bench.spec`.
- Suites are grouped by area: `httpc_*_SUITE` (client), `httpd_*_SUITE` (server), `http_format_SUITE`/`uri_SUITE` (shared HTTP/URI helpers), `inets_SUITE`/`inets_sup_SUITE` (app + supervision), `inets_socketwrap_SUITE` (pre-opened fd, needs linux + setuid).
- Tests need a built `ebin/` and start real `httpd` instances and/or the in-Erlang fake server `test/http_test_lib.erl`; they bind ephemeral local ports and require `ssl`/`crypto` for the TLS variants.
- Known gaps: `mod_actions` has no dedicated assertions, the `heavy` load profiles are disabled, and `test/property_test/` is dormant (only a README remains).

## Notes for Agents
- Deprecated but still shipped current functionality: `http_uri` (use `uri_string`), `httpd:parse_query/1`, and the server modules `mod_cgi`/`mod_actions` (with helper `httpd_cgi`) — `mod_cgi`/`mod_actions` are still listed in `inets.app.src` and covered by `httpd_SUITE`/`httpc_SUITE`.
- `test/inets.spec` is the `make test` entry point and `test/inets_bench.spec` the benchmark one; `test/inets.config` is installed by `test/Makefile` but contains only a dead legacy path, and the `inets.config` inside `src/inets_app/` is an equivalent stale example — no suite reads either.
- Sample data (not secrets): `examples/server_root/auth/passwd` has plaintext demo credentials and `examples/server_root/ssl/*.pem` embed a sample certificate+key — keep them as-is.
- `examples/server_root/conf/*` reference absolute `/var/tmp/server_root` paths, so the sample tree must be copied/run from its own root; ports 8888/8080/8088 are hard-coded.

## Routing Table
| Path | Description |
| --- | --- |
| `src/` | All Erlang sources — see `src/CONTEXT.md` for the four-way split and build order |
| `src/inets_app/` | `inets` service API, app/supervisor callbacks, `inets_service` behaviour, `inets.app.src`/`.appup.src`, shared `inets_internal.hrl`/`inets.mk` |
| `src/http_lib/` | Shared HTTP primitives for client and server (`http_chunk`, `http_request`, `http_response`, `http_transport`, `http_util`, deprecated `http_uri`) |
| `src/http_client/` | `httpc` API, profiles/sessions, handlers, request/response encoding, cookies |
| `src/http_server/` | `httpd` API + `mod_*` behaviour, instance/acceptor/handler supervision, config, auth/security stacks, deprecated `mod_cgi`/`mod_actions` |
| `test/` | Common Test suites, helpers, spec/config files and per-suite data dirs — see `test/CONTEXT.md` |
| `doc/` | Guides, ExDoc config (`docs.exs`), release notes and the RFC archive — see `doc/CONTEXT.md` |
| `include/` | Public installed headers `httpd.hrl` (`init_data`, `mod` records) and `mod_auth.hrl` (`httpd_user`, `httpd_group`) |
| `examples/` | Runnable sample `httpd` server root (conf/htdocs/cgi-bin/auth/ssl/icons) — see `examples/CONTEXT.md` |
| `Makefile`, `vsn.mk`, `AUTHORS` | App build driver (`src examples doc`, `DIA_PLT_APPS`), version (`INETS_VSN`) and credits |
| `ebin/` | Build output only (generated `inets.app`/`inets.appup` and `.beam` files); only `.gitignore` is tracked |

## See Also
- `../CONTEXT.md` — `lib/` conventions for OTP applications (app dir schema, `vsn.mk`, `make/app_targets.mk`).
- `../ftp/`, `../tftp/` — the FTP and TFTP client/server applications, separate from (and not services of) `inets`.
- `../ssl/`, `../public_key/`, `../crypto/` — the TLS stack used by `httpc`/`httpd` HTTPS.
- `../common_test/` — the test framework and `ts:compile_datadirs/2` used by `test/`.
