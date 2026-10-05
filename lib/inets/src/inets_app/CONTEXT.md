# `lib/inets/src/inets_app` — Inets application housekeeping

## Intent
App-level (non-protocol) subdirectory of the OTP `inets` application: the public `inets` module, the `inets` application/supervisor callbacks, the service behaviour, and the app resource files.
It is also the place that hosts the artifacts shared with the other three `src/` subdirectories: the private header `inets_internal.hrl` and the compile-flag fragment `inets.mk`.
It carries no HTTP protocol logic — that lives in `../http_lib`, `../http_client`, `../http_server`; all four subdirs compile into the single shared `lib/inets/ebin/` (`EBIN = ../../ebin`).
It is the first entry of `../subdirs.mk` (`SUB_DIRECTORIES = inets_app http_lib http_client http_server`), i.e. built before the subdirs that include its `.mk`/`.hrl`.

## API Surface
Public, documented service API — `inets.erl` (ExDoc group "Service API", see `../../doc/docs.exs`):
- `start/0` → `application:start(inets, temporary)`; `start/1` → `application:ensure_all_started(ssl)` (result ignored) then `application:start(inets, Type)`.
- `start/2 (Service, Config)` = `start/3` with `How = inets`; `start/3 (Service, Config, How)` with `How :: inets | stand_alone`.
- `stop/0` → `application:stop(inets)`; `stop(stand_alone, Pid)` → `exit(Pid, shutdown)`; `stop(Service, Pid)` → `Service:stop_service(Pid)`.
- `services/0` → flattened `Module:services()` over `service_names()`; returns `{error, inets_not_started}` when the calls exit with `{noproc, _}`.
- `services_info/0` → `[{Service, Pid, Info}]`, `Info` being the `Service:service_info(Pid)` proplist or the error reason.
- `service_names/0` → hardcoded `[httpc, httpd]` (FTP/TFTP are no longer inets services).
- `service() = httpc | httpd`, `inets_service()` type and `service_info()` type are declared here.
Exported but `-doc false` (internal/legacy) in `inets.erl`: `versions/0`, `print_version_info/0,1`, `enable_trace/2,3`, `disable_trace/0`, `set_trace/1`, `report_event/4` (all trace entry points delegate to `inets_trace`).
`inets_service.erl` — the `-behaviour` contract (no runtime code, `-moduledoc false`): callbacks `start_standalone/1`, `start_service/1`, `stop_service/1`, `services/0`, `service_info/1`.
`inets_app.erl` — application callback module referenced by `{mod,{inets_app,[]}}`: `start/2` → `inets_sup:start_link()`, `stop/1` → `ok`.
`inets_sup.erl` — registered top supervisor of the application.
`inets_lib.erl` — internal helpers `millisec_passed/1`, `formated_timestamp/0`, `format_timestamp/1` (timestamp formatting only; not a service registry despite the name).
`inets_trace.erl` — the inets trace/event mechanism: `enable/2,3` (Destination = file | port | io | `{Fun,Data}`), `disable/0`, `set_level/1,2`, `report_event/4`; built on `dbg`.
Shared with sibling subdirs (this is the node's cross-cutting API):
- `inets_internal.hrl` — `?report/4` + `?report_important|verbose|debug|trace` severity macros, and HTTP character macros (`?CR`, `?LF`, `?CRLF`, `?SP`, `?TAB`, paren/quote macros).
- `inets.mk` — `INETS_ERL_COMPILE_FLAGS` with `-pa $(ERL_TOP)/lib/inets/ebin`, the `sys_pre_attributes` parse transform inserting `{attribute,insert,app_vsn,$(APP_VSN)}`, `+warn_obsolete_bool_op +nowarn_deprecated_catch`; included by `../http_lib/Makefile`, `../http_client/Makefile`, `../http_server/Makefile`.
- `inets.app.src` / `inets.appup.src` — the `inets` application resource and upgrade specs (both use `%VSN%`).
- `inets.config` — example `services` app-env configuration (not installed by the Makefile, not referenced by the build).
- `Makefile` — compiles the five `MODULES` (inets, inets_service, inets_app, inets_sup, inets_trace, inets_lib) into `../../ebin`, generates `ebin/inets.app`/`ebin/inets.appup` by sed-substituting `%VSN%`, and installs sources + `inets_internal.hrl` into `$(RELEASE_PATH)/lib/inets-$(VSN)/src/inets_app`.

## Behaviour contract (`inets_service`)
`start_standalone(Config)` → `<service>:start_link(Config)`, `{ok, Pid} | {error, Reason}`.
`start_service(Config)` → `<service>_sup:start_child(Config)`, `{ok, Pid} | {error, Reason}`.
`stop_service(Ref)` → `<service>_sup:stop_child(...)`, `ok | {error, Reason}`; the pid→ref mapping is the service's responsibility (`httpc`/`httpd` do `service_info/1` first).
`services()` → `[{Service, Pid}]` of *running* services only; standalone-started services are not listed.
`service_info(Ref)` → `{ok, [{Property, Value}]} | {error, Reason}` (e.g. `[{profile, ProfileName}]` for httpc, `[{bind_address, ...}, {port, ...}, {profile, ...}]` for httpd).
`inets.erl` invokes these by dynamic dispatch (`start_service/3`, `call_service/3`), so any module named in `service_names/0` must export all five callbacks.
Implementations: `httpc` in `../http_client/httpc.erl`, `httpd` in `../http_server/httpd.erl`; both declare `-behaviour(inets_service)` and mark the callbacks `-doc false`.

## Supervision layout
`inets_app` (application callback) → `inets_sup` (registered, `one_for_one`, intensity 10 / period 3600) → two permanent children with `shutdown infinity`: `httpc_sup` and `httpd_sup` (defined in `../http_client` and `../http_server` respectively).
`inets_sup:children/0` reads the `services` app env (`application:get_env(inets, services)`, `[]` on failure), splits it into httpd/httpc entries and starts `httpc_sup` first, then `httpd_sup`, independent of the order in the config.
Recognized `services` entry shapes: `{httpc, Conf}`, `{httpd, Conf}`, `{httpd, Address, Port}`; `is_httpc/1` matches only a 2-tuple, so any other shape is silently dropped.
A `httpc` default profile (`{httpc, {default, only_session_cookies}}`) is always injected when the configured list has no `{httpc, {default, _}}` entry.
`{registered,[inets_sup, httpc_manager]}` — `httpc_manager` (owned by `../http_client`) is the second registered process of the application.
Dynamic services added at runtime become children of the per-service supervisors; `stand_alone` services bypass `inets_sup` entirely and are linked to the caller.

## Constraints
`inets.app.src`'s `modules` list is the authoritative registry for the whole application: every module in all four `src/` subdirs, including deprecated ones (`http_uri`, `mod_cgi`, `mod_actions`), must be listed there — the generated `ebin/inets.app` and hence `test_server:app_test/1` and `inets:versions/0` depend on it.
`%VSN%` in `inets.app.src`/`inets.appup.src` is substituted from `../../vsn.mk` (`INETS_VSN`); edit the `.src` files, never `ebin/inets.app`.
Only `kernel` and `stdlib` may appear in `{applications, ...}`; ssl/crypto/public_key/runtime_tools/mnesia are optional or test-only (`{runtime_dependencies, ...}` lists stdlib/ssl/runtime_tools/mnesia/kernel/erts/public_key versions).
Modules in this directory must stay protocol-agnostic; httpc/httpd specifics belong in the sibling subdirs.
`inets_internal.hrl` is an internal header, but must remain parseable from sibling subdirs both via `-include_lib("inets/src/inets_app/inets_internal.hrl")` and via `-I../inets_app` + `-include("inets_internal.hrl")`.

## Design Decisions
The service/behaviour split exists so one service implementation supports two lifecycles: supervised child of the inets tree (`start_service/1`, the default path) or process linked to the caller (`start_standalone/1`).
Dynamic services are intentionally excluded from OTP application takeover/failover and from restart on application restart (documented in `inets:start/2,3`); `stand_alone` is documented as deprecated.
`inets_service.erl` exports nothing — it exists purely as the `-callback` contract that gives both services the same dynamic call convention used by `inets.erl`.
Trace events are emitted unconditionally as calls to `inets_trace:report_event/4` with a numeric Severity 0..100; the function body is only `hopefully_traced`, so the calls are cheap no-ops unless a dbg match spec is installed (No conditional compilation at call sites, by design.)
`inets_sup` keeps the httpc/httpd supervisors as two fixed child specs rather than one per configured service, so the supervision tree shape is stable and services can be added at runtime.
The top supervisor restart strategy is `one_for_one` with intensity 10/3600 because the two service trees are independent.

## Known Issues
`inets:start/1` calls `application:ensure_all_started(ssl)` and discards the result while `inets:start/0` does not touch ssl at all — with `start/0` a later HTTPS request fails unless the caller started ssl.
`inets.mk` still adds `-Dinets_trace_io` (when `INETS_TRACE=io`) and `-Dinets_debug` (when `INETS_DEBUG=true`), but no source under `lib/inets/src/` tests those macros any longer; `../test/Makefile` also adds `-Dinets_debug` — the flags are vestigial.
`inets_internal.hrl`'s `?report*` macros are only reached through `../http_client/httpc_internal.hrl` (`hcri/hcrv/hcrd/hcrt`); the http_server/http_lib modules include the header only for the HTTP character macros, so httpd emits no inets trace events.
`inets.config` in this directory is a stale example (`{httpd, "/var/tmp/server_root/conf/8888.conf"}` etc.), not installed and not used by any build or test; the config actually used by the test suites is `../test/inets.config`.
`inets.erl` relies on targeted warning suppressions (`nowarn_deprecated_function` for `erlang:exit/2`, `nowarn_possibly_unsafe_function` for `file:consult/1`); do not drop them without changing the corresponding calls.
`inets_sup:get_services/0` matches only `{httpc, Conf}`, `{httpd, Conf}` and `{httpd, Address, Port}` — a malformed or 3-tuple httpc entry is ignored without any error or log.

## Routing Table
No subdirectories — the node is flat (12 files). Module/file map:
- `inets.erl` → public service API + version/trace entry points (the only documented module here)
- `inets_service.erl` → service behaviour contract (extension point implemented by `../http_client/httpc.erl` and `../http_server/httpd.erl`)
- `inets_app.erl` → application callback module (`start/2`, `stop/1`)
- `inets_sup.erl` → top supervisor; builds the `httpc_sup`/`httpd_sup` child specs from the `services` env
- `inets_lib.erl` → timestamp/duration helpers used by tracing and tests
- `inets_trace.erl` → trace/event mechanism built on `dbg`
- `inets_internal.hrl` → shared internal macros (trace + HTTP characters), also installed to the release
- `inets.mk` → shared compile-flag fragment, included by the other three subdir Makefiles
- `inets.app.src`, `inets.appup.src` → application resource + upgrade specs (`%VSN%` templates)
- `inets.config` → stale example app-env configuration
- `Makefile` → build/install of the six modules and of `ebin/inets.app|.appup`

## Notes for Agents
Sibling subdirectories (read-only from here; escalate writes to the `../` parent): `../http_lib/` (shared HTTP encode/decode + transport), `../http_client/` (`httpc`, profiles, supervisors), `../http_server/` (`httpd`, modules, instance supervisors).
App-level tests live in the sibling `../../test/` — `inets_SUITE.erl` (`app`/`appup` via `test_server:app_test/1`/`appup_test/1`, `services_test` group covering start/stop of httpc/httpd, standalone and `services` env), `httpc_SUITE.erl` and `httpd_*_SUITE.erl` exercise `inets:start/2,3` and `inets:stop/2` dynamically.
Trace API (`enable_trace` etc.) has no test coverage in `lib/inets/test/` and is undocumented — treat it as internal debug tooling.
`inets:versions/0` consults `ebin/inets.app` at runtime, so it only works against an installed/generated app file, not from a plain source checkout.

## See Also
`../CONTEXT.md` — `lib/inets/` app overview (`INETS_VSN = 9.8`, subdir build order).
`../../doc/guides/inets_services.md` — user-facing description of the service concept.
`../../doc/docs.exs` — docs module grouping ("Service API" → `inets`).
Root `CONTEXT.md` — `lib/` conventions for OTP application directories.
