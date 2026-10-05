# inets — Erlang sources

## Intent
Holds all Erlang source of the `inets` application, split into four subdirectories that each build independently but share one output directory.
`Makefile` includes `subdirs.mk` (`SUB_DIRECTORIES = inets_app http_lib http_client http_server`) and recurses via `$(ERL_TOP)/make/otp_subdir.mk`.
The build order in `subdirs.mk` is significant: `inets_app` first (the other three include its `inets.mk` compile-flag fragment and `inets_internal.hrl`), then `http_lib`, then `http_client`, then `http_server`.
There is no FTP or TFTP code here — those are separate OTP applications (`lib/ftp`, `lib/tftp`), and `inets:service_names/0` is only `[httpc, httpd]`.

## Constraints
- All four subdirectories compile into the single shared `lib/inets/ebin/` (`EBIN = ../../ebin`); there is no per-subdirectory ebin.
- Each subdirectory has a `Makefile` with an explicit `MODULES` list; a new module must be added there AND to the `modules` list of `../src/inets_app/inets.app.src`, or it will not be loaded/installed.
- Compile flags come from `inets_app/inets.mk` (`INETS_ERL_COMPILE_FLAGS`, the `sys_pre_attributes` parse transform that injects `app_vsn`); include paths are `../../include`, `../inets_app`, `../http_lib`.
- `src/.gitignore` ignores `**/deps/`, the directory each subdirectory `Makefile` writes its generated `erlc -M` dependency file into (`DEPFILE = deps/<subdir>.d`, created by `mkdir -p`) — build artifacts, never edited by hand.
- Header include-path convention: private headers live next to their modules (`httpc_internal.hrl`, `httpd_internal.hrl`, `http_internal.hrl`); only `httpd.hrl`/`mod_auth.hrl` are exported via `lib/inets/include/`.

## Notes for Agents
- Layering is `inets_app` (app/service shell) → `http_lib` (shared wire-level primitives) → `http_client`/`http_server` (protocol endpoints); respect it when adding code.
- `../include/httpd.hrl` and `../include/mod_auth.hrl` are the public copies; `http_server/httpd.hrl` and `http_server/mod_auth.hrl` are thin wrappers that `-include_lib` them with different include guards (`src_httpd_hrl`/`src_mod_auth_hrl`) for backwards compatibility.
- Files in `http_*` were historically in the reverse order (http_server also once did directory dispatch); the current split is by protocol role, not by ordinal.

## Routing Table
| Subdirectory | Contents |
| --- | --- |
| `inets_app/` | `inets` public service API, app/supervisor callbacks, `inets_service` behaviour, app resource (`inets.app.src`/`.appup.src`), plus the cross-cutting `inets_internal.hrl` and `inets.mk` shared by the three sibling subdirs |
| `http_lib/` | Shared HTTP layer used by BOTH client and server: `http_chunk`, `http_request`, `http_response`, `http_transport`, `http_util`, deprecated `http_uri`, `http_internal.hrl` |
| `http_client/` | The `httpc` client: `httpc` API, profile/session supervisor chain, request/response encoding, cookie store |
| `http_server/` | The `httpd` server: `httpd` API + `mod_*` behaviour, instance/acceptor/handler supervision, config parsing, auth and security stacks, deprecated `mod_cgi`/`mod_actions` |

## See Also
- `../CONTEXT.md` — inets application overview (version, layout, test entry points).
- `../test/CONTEXT.md` — the Common Test suites covering each subdirectory here.
- `../include/`, `../doc/`, `../examples/` — public headers, documentation and sample server root.
