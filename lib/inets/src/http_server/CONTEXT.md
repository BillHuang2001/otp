# inets HTTP Server (`httpd`)

## Intent
The `inets` HTTP server — a self-contained HTTP/1.1 (RFC 2616 / RFC 9112) web server in Erlang: supervisors, acceptor/connection processes, request parsing/response pipeline, socket abstraction, pluggable `mod_*` request-handler modules and the public `httpd` API.
Flat directory (no subdirectories): 42 `.erl` files + 3 `.hrl` wrappers, all compiled into the shared `../../ebin/` (see `Makefile` `MODULES`/`BEHAVIOUR_MODULES`, `../../src/subdirs.mk`).
Public documentation for this area lives in `../../doc/` (`httpd`, `httpd_util`, `httpd_socket`, `mod_*` reference pages and the "Inets Web Server" user's guide).

## API Surface

### Public API (`httpd.erl`)
- Public modules with `-moduledoc`: `httpd`, `httpd_util`, `httpd_socket`, `mod_alias`, `mod_auth`, `mod_esi`, `mod_security`, `httpd_custom_api`; everything else is `-moduledoc false` (internal).
- `httpd` implements the `inets_service` behaviour (`../inets_app/inets_service.erl`): `start_standalone/1`, `start_service/1`, `stop_service/1`, `services/0`, `service_info/1` — these are the hooks `inets:start(httpd, Config)` / `inets:stop/1` call; they are `-doc false` and not meant for direct use.
- `start_standalone/1` starts a *standalone* `httpd_sup` (`httpd_sup:start_link/2` with `stand_alone`) for `erl -S httpd serve`; `start_service/1` adds one supervised instance under the node-global `httpd_sup` (`httpd_sup:start_child/1`).
- Public helper API: `httpd:info/1..4` (query live properties by pid, `{Address,Port}`, or `{Address,Port,Profile,Properties}`), `reload_config/2` (reload without restart; `bind_address`/`port` cannot change; modes `non_disturbing | disturbing | blocked`), `parse_query/1` — **deprecated**, use `uri_string:dissect_query/1`.
- Command-line API: `httpd:start/1` and `serve/1` back `erl -S httpd serve` (argparse: `--directory`, `--port` default 8000, `--bind` default `{127,0,0,1}`, `--help`); `serve/1` starts `inets` and an instance with `modules = [mod_alias, mod_dir, mod_get]`.
- `#mod{}` (and `#init_data{}`) is the request record passed to every handler, defined in the public header `../../include/httpd.hrl` (include with `-include_lib("inets/include/httpd.hrl")`).
- Config is a plain Erlang proplist (`[{Option, Value}]`) or `{proplist_file, Path}` consulted via `file:consult/1`; the Apache-style config-file syntax was dropped (OTP-23), so config files must be Erlang terms.
- Config property groups: mandatory (`port`, `server_root`, `document_root`), communication (`bind_address`, `server_name`, `profile`, `socket_type`, `ipfamily`, `minimum_bytes_per_second`), `modules`, limits (`customize`, `keep_alive`, `keep_alive_timeout`, `request_timeout`, `max_clients`, `max_header_size`, `max_body_size`, `max_content_length`, `max_uri_size`, `max_keep_alive_request`, `max_client_body_chunk`, `disable_chunked_transfer_encoding_send`), administrative (`mime_types`, `mime_type`, `server_admin`, `server_tokens`, `logger`, `log_format`, `error_log_format`), plus per-`mod_*` directives (see each module below) and the legacy internal `{debug, ...}` / `{accept_timeout, ...}` service options.
- `socket_type` is `ip_comm | {ip_comm, Opts} | {ssl, Opts}`; legacy `{essl, Opts}` is silently rewritten to `{ssl, Opts}` by `httpd_ssl_wrapper/1`; `info/1..4` property groups are typed as `CommonOption | CommunicationOption | ModOption | LimitOption | AdminOption`.

### Behaviours / extension points
- `mod_*` request handlers: the behaviour is declared **in `httpd.erl`**, not in a separate module — callbacks `do/1` (`{proceed, OldData} | {proceed, NewData} | {break, NewData} | done`), optional `store/2` and `remove/1`; there is **no `httpd_mod` module or behaviour** and the shipped `mod_*` modules do not declare `-behaviour` at all.
- `httpd_custom_api` behaviour (optional callbacks `response_default_headers/0`, `response_header/1`, `request_header/1`) selected by the `{customize, Module}` property; `httpd_custom` is the default implementation and wraps the user module, falling back to itself on exceptions.
- `mod_esi` behaviour: `Module:Function(SessionID, Env, Input) -> {continue, State} | _` (optional callback, chunking supported); `mod_esi:deliver/2` (with opaque `session_id()` since OTP 28) streams response chunks; scripts are reached through the `erl_script_alias` property.
- `mod_security` declares a security-audit `event/4` callback and a per-directory `callback_module` option.
- `httpd_example` is a sample ESI module used by examples/tests — not part of the supported API.

## Architecture (supervision & processes)
- `httpd_sup` (registered `httpd_sup`, one per node/standalone VM) → one `httpd_instance_sup` per server instance, registered as `{httpd_instance_sup, Address, Port, Profile}`; children: `httpd_connection_sup`, `httpd_misc_sup`, `httpd_manager`, `httpd_acceptor_sup`.
- `httpd_acceptor_sup` → one `httpd_acceptor` per listener (`proc_lib` process that owns the listen socket and loops on `http_transport:accept/3`); on accept it calls `httpd_connection_sup:start_child/2`, transfers socket ownership (`http_transport:controlling_process/3`) and notifies the worker via `httpd_request_handler:socket_ownership_transfered/3`.
- `httpd_connection_sup` is `simple_one_for_one` with `Temporary`/`brutal_kill` `httpd_request_handler` children — one short-lived process per connection.
- `httpd_request_handler` is a `gen_server` that boots through `gen_server:enter_loop/3` (its `init/1` must first wait for the socket-ownership hand-off); it parses, dispatches and keeps the connection alive, supporting pipelining and keep-alive with `keep_alive_timeout` / `request_timeout` timers.
- `httpd_manager` is the per-instance `gen_server` that owns the config DB lifetime, implements `reload/2`, `block/2,3`/`unblock/1` and admits connections (`new_connection/1` enforcing `max_clients`); registered as `httpd_<addr>_<port>_<profile>` (`httpd_util:make_name("httpd", ...)`).
- `httpd_misc_sup` starts extra per-instance workers on demand: `mod_auth_server` (`start_auth_server/3`) and `mod_security_server` (`start_sec_server/3`); its child list starts empty.
- All per-instance names are derived from `{Address, Port, Profile}` with `httpd_util:make_name/4` — the `profile` property exists so several logical servers can share one address/port.
- Socket I/O is abstracted in `../http_lib/http_transport.erl` (`ip_comm` → `gen_tcp`, `ssl` → `ssl`); `httpd_socket` is the thin, documented wrapper (`deliver/3`, `peername/2`, `resolve/0`, `close/2`) used by every handler.
- The listener socket can be inherited from a pre-bound fd passed as the `-httpd_<Port> <Fd>` init argument (`httpd_sup:listen_init/4` + `get_fd/1`), which supports socket-activation style startup.

## Architecture (config & request pipeline)
- `httpd_conf` validates and stores the config: `validate_properties/1` (mandatory + inter-property checks, fills `server_name` default, maps legacy `inet6fb4`→`inet`), then `store/1,2` writes a named ETS `bag` table `<httpd_conf addr port profile>` and calls each configured module's `store/2` to validate/fix its own directives; `remove/1` walks the table calling `Module:remove/1` for cleanup.
- `httpd_request` parses the HTTP message incrementally from binaries (`parse/1`, `parse_method/uri/version/headers`, body helpers) using the shared `../http_lib/` parsers (`http_request`, `http_chunk`, `http_uri`, `http_util`), validates method/URI/version (`GET POST PUT DELETE PATCH OPTIONS TRACE HEAD`; `Host` required for HTTP/1.1) and builds `#mod{}` via `update_mod_data/5`.
- Response dispatch: `httpd_response:generate_and_send_response/1` reads the `modules` property (default `mod_alias, mod_auth, mod_esi, mod_dir, mod_get, mod_head, mod_log, mod_disk_log`) and folds `traverse_modules/2` over it, calling `Module:do(ModData)` and threading the `data` proplist; `{proceed, Data}` continues, `{break, _}`/`done` stop, a raised exception is logged and answered with 500, and an empty handler list yields 501.
- `#mod.data` carries `{status, ...}` (short-circuit response), `{response, {StatusCode, Body}}` (legacy) and `{response, Head, Body}` / `{already_sent, StatusCode, Size}` (NewDataFormat, recommended); `{Fun, Arg}` bodies are applied and may return `sent`/`close`.
- `httpd_response` builds headers (HTTP/1.0 filtering, `Server`/`Date`/`Connection`/caching defaults, chunked encoding, `map_status_code/2`) and `httpd_util` provides the handler helpers (config-DB `lookup/2..4`/`multi_lookup/2`, MIME lookup, RFC1123 dates, path splitting, ETag creation, `make_name/4`, `enable_debug/1`).
- Request-body handling enforces `max_body_size`, `max_content_length`, `max_uri_size`, `max_header_size` and `max_client_body_chunk` (chunked delivery to `mod_esi` callbacks via a `{continue, MFA, Args}` state machine); `Transfer-Encoding` + `Content-Length` together is rejected, as are whitespace-before-colon and obs-fold continuation headers.
- Logging: `httpd_logger` emits `logger` events with `domain => [otp, inets, httpd, ServerID, Level]` and a `format/1` `report_cb` (gated by the `{logger, ...}` property); `httpd_log` formats Apache-style common/combined access, error and security entries (`{log_format, common|combined}`, `{error_log_format, pretty|compact}`) and is used by `mod_log` (`error_log`, `security_log`, `transfer_log` as plain files) and `mod_disk_log` (`error_disk_log`, `security_disk_log`, `transfer_disk_log` + `*_disk_log_size`, `disk_log_format`).

## Module routing (files → role)
- Server core / API: `httpd.erl` (public API, `mod_*` behaviour callbacks, CLI), `httpd_util.erl` (public helpers), `httpd_socket.erl` (documented socket wrapper), `httpd_internal.hrl` (private contract macros: `?DEFAULT_MODS`, chunk sizes, `?SERVER_PROTOCOL`).
- Supervisors/processes: `httpd_sup.erl`, `httpd_instance_sup.erl`, `httpd_misc_sup.erl`, `httpd_connection_sup.erl`, `httpd_acceptor_sup.erl`, `httpd_acceptor.erl`, `httpd_manager.erl`, `httpd_request_handler.erl`.
- Request/response machinery: `httpd_request.erl` (parse/validate/`#mod{}` build), `httpd_response.erl` (module traversal + response/status/chunk sending), `httpd_file.erl` (error-file mapping), `httpd_script_env.erl` (CGI/ESI environment variables; deliberately skips `PROXY`, CVE-2016-1000107).
- Config: `httpd_conf.erl` (validate/store/ETS config DB, MIME-types loading).
- Logging: `httpd_log.erl` (entry formatting), `httpd_logger.erl` (`logger` bridge).
- URL/file handlers: `mod_alias.erl` (`alias`, `re_write`, `script_alias`, `script_re_write`, `directory_index`; exports `real_name/3`, `real_script_name/3`, `default_index/2`, `path/3`), `mod_dir.erl` (generates the HTML directory listing; the `directory_index` lookup itself lives in `mod_alias`), `mod_get.erl` (file delivery, streamed with `file:read/2` in `?FILE_CHUNK_SIZE` chunks and answered as `already_sent`), `mod_head.erl`, `mod_range.erl` (`Range`/`If-Range`, `206`/`416`), `mod_responsecontrol.erl` (caching/ETag/`If-*` handling), `mod_trace.erl` (`TRACE`).
- Authentication: `mod_auth.erl` (auth/`allow_from`/`deny_from`/`require_*` checks plus the legacy user/group management API), `mod_auth_server.erl` (per-instance `gen_server` dispatcher backing that API), backends selected by `auth_type` (`plain`→`mod_auth_plain`, `dets`→`mod_auth_dets`, `mnesia`→`mod_auth_mnesia`); records `#httpd_user{}`/`#httpd_group{}` are public in `../../include/mod_auth.hrl`.
- Security: `mod_security.erl` (per-directory security event handling, `list_blocked_users`/`block_user`/`unblock_user`/`list_auth_users`), `mod_security_server.erl` (per-instance `gen_server` holding the ETS tables, implements `max_retries`, `block_time`, `fail_expire_time`, `auth_timeout`, `data_file`).
- ESI: `mod_esi.erl` (ESI API, `deliver/2`, `erl_script_alias`/`erl_script_timeout`/`erl_script_nocache`), `httpd_esi.erl` (internal header parse/format helpers for erl scripts), `httpd_example.erl` (sample ESI module).
- `httpd_custom_api.erl` (behaviour) + `httpd_custom.erl` (default implementation) — the `customize` extension point.
- Deprecated: `mod_cgi.erl` (+ `httpd_cgi.erl` CGI output-parsing helpers, `script_alias`/`script_nocache`/`script_timeout`) and `mod_actions.erl` (`action`/`script`) — both carry `-deprecated([{'_','_',"use 'mod_esi' for dynamic page generation"}])`, are documented as deprecated since OTP 29 and slated for removal in OTP 30, yet still shipped and still listed in `../../src/inets_app/inets.app.src`.
- Not source modules here: `httpd.hrl` and `mod_auth.hrl` in this directory are thin backwards-compatibility wrappers that just `-include_lib("inets/include/...")` with their own include guards (`src_httpd_hrl`, `src_mod_auth_hrl`) — **edit the copies in `../../include/`, not these**.

## Constraints
- The private contract between the server modules is `httpd_internal.hrl`; `#mod{}`/`#init_data{}` come from `../../include/httpd.hrl` — do not add new records to the private header.
- A `mod_*` handler must implement `do/1` and return one of `{proceed, Data} | {break, Data} | done`; `store/2`/`remove/1` are optional but must be idempotent, as `httpd_conf` calls them on every (re)load/removal.
- Handler ordering in the `modules` property is significant (e.g. `mod_alias` before `mod_get`, `mod_auth` before content modules) and modules depend on each other's `data` entries.
- `httpd_util` is supported only in combination with `httpd`; its other uses are considered deprecated (per its `-moduledoc`).
- Every new module must be added to `MODULES` in this directory's `Makefile` (behaviours go in `BEHAVIOUR_MODULES`, which is compiled first) and to `../../src/inets_app/inets.app.src`.
- Deprecated modules must keep working until removal; do not delete `mod_cgi`/`mod_actions` without the release-level deprecation decision.

## Design Decisions
- Per-instance isolation keyed by `{Address, Port, Profile}`, with every process registered under a name derived from it, so multiple servers can share an address/port pair.
- One acceptor process per listener plus a transient worker per connection (`simple_one_for_one`) keeps accept latency low and makes a crashed connection cheap to discard.
- `httpd_request_handler` uses `gen_server:enter_loop/3` because the socket is handed over *after* the worker is started (the acceptor must not block on `init`).
- Config is materialised into a per-instance ETS `bag` so hot code paths read directives by key lookup and each `mod_*` module validates its own directives through the `store/2` callback.
- The `mod_*` behaviour callbacks live in `httpd.erl` (the only public "behaviour" entry point) rather than in a separate behaviour module.
- NewDataFormat responses (`{response, Head, Body}`) are preferred over the legacy `{StatusCode, Body}` shorthand because header parsing and status codes are more accurate.

## Known Issues
- `httpd_connection_sup` registers itself *without* the profile (`httpd_util:make_name("httpd_connection_sup", Addr, Port)`), unlike its parent child spec which is keyed by `{httpd_connection_sup, Address, Port, Profile}` — two instances differing only in `profile` on the same address/port collide on that registered name.
- `httpd_instance_sup` and `httpd_misc_sup` use restart intensity `{one_for_one, 0, 1}`, i.e. no child is restarted before the supervisor itself gives up — a single child crash tears down the whole server instance (other sups use `{one_for_one, 10, 3600}`).
- `src/http_server/httpd.hrl` and `src/http_server/mod_auth.hrl` are wrappers only; agents editing the "local" header will change nothing (guards differ so double-inclusion does not fail loudly).
- `httpd_manager:start/2` and `start_link/2` are commented `Deprecated`; only `httpd_instance_sup`'s 3/4-arity variants are used.
- Untested/undocumented `{debug, ...}` service options activate `?hdrd/?hdrv/?vtrace` tracing through `httpd_util:enable_debug/1`; they are not part of the public config surface.

## Dependencies
- Runtime/OTP apps: `kernel` (`gen_tcp`, `inet`, `logger`, `disk_log`, `file`, `dets`), `stdlib`, `ssl` (only for `socket_type` ssl; see `inets.app.src` `runtime_dependencies` for ssl/mnesia/public_key versions), `mnesia` only when `auth_type = mnesia`.
- In-app: `../http_lib/` (`http_transport`, `http_request`, `http_chunk`, `http_uri`, `http_util`, `http_internal.hrl`), `../inets_app/` (`inets_internal.hrl` with `?LOCATION`, `inets_service` behaviour), `../../include/` (`httpd.hrl`, `mod_auth.hrl`).
- External OS requirements: only for `mod_cgi`/`mod_actions` (external CGI scripts) and SSL certificates/TLS options supplied through `socket_type`.

## Test Strategy
- Common Test suites live in `../../test/` (sibling — read-only, escalate writes to the parent node): `httpd_SUITE.erl` (main functional suite), `httpd_basic_SUITE.erl`, `httpd_serve_SUITE.erl` (the `erl -S httpd serve` path), `httpd_bench_SUITE.erl` (benchmarks/load), plus helper modules `httpd_test_lib.erl`, `httpd_1_1.erl`, `httpd_1_0.erl`, `httpd_poll.erl`, `httpd_time_test.erl`, `httpd_load.erl`.
- Suites run the real server via `inets:start(httpd, ...)`/`httpd:start_standalone` and drive it over real sockets; data directories `httpd_SUITE_data/`, `httpd_basic_SUITE_data/`, `httpd_serve_SUITE_data/`, `httpd_test_data/` hold `server_root` trees, CGI sources and MIME files.
- `../../test/Makefile` maps test profile variables (`MANV`, `REQV`, `ACCV`, `AUTHV`, `SECV`) onto `-D<module>_verbosity=...` macros, i.e. per-process verbosity for manager/request-handler/acceptor/auth/security must be baked in at compile time.
- `../../test/property_test/` is only a README describing intended (not implemented) property tests for this area.
- There is no unit-test directory inside `src/http_server/`; changes here are validated through the `../../test/` suites (`make test` in `lib/inets`).

## See Also
- `../CONTEXT.md` — the `inets` app (routing table for `inets_app`, `http_lib`, `http_client`, `http_server`).
- `../http_lib/CONTEXT.md` — shared HTTP parsing/socket layer used by this directory.
- `../inets_app/CONTEXT.md` — `inets` service API, supervisors and internal macros.
- `../../include/` and `../../doc/` — public headers and published documentation for this directory.
- `../../test/` — Common Test suites covering the HTTP server (sibling, read-only from here).
