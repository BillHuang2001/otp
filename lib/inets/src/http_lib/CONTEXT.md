# inets — Shared HTTP Layer (`src/http_lib/`)

## Intent
The HTTP building blocks shared by BOTH the `inets` HTTP client (`../http_client/`) and the HTTP server (`../http_server/`) — that shared usage is the reason this is a separate directory.
Provides message encoding/decoding (request headers, response headers, chunked transfer coding), a socket abstraction that hides `gen_tcp` vs `ssl`, and small misc helpers.
Compiles into the single shared `../../ebin/` (see `Makefile` `EBIN = ../../ebin`); there is no per-subdir `ebin`.

## API Surface
All modules except `http_uri` are **internal** (`-moduledoc false`, listed in `inets.app.src` but not in any `docs.exs` doc group). Only `http_uri` is public, documented API.

- `http_chunk` — chunked transfer coding (RFC 7230 / RFC 2616 §3.6.1), used by both sides.
  - `decode(ChunkedBody, MaxBodySize, MaxHeaderSize)` → `{ok, {Headers, Body}}` or a continuation `{Module, Function, Args}` when the input is incomplete.
  - `encode(Chunk)` / `encode_last()` / `encode_last(Trailers)` build the wire format.
  - `handle_headers(HeaderRecord, ChunkedHeaders)` folds decoded trailer headers back into a `#http_request_h{}`/`#http_response_h{}`, drops `chunked` from `transfer-encoding` and adds `content-length`.
  - Callback API (`decode_size/1`, `ignore_extensions/1`, `decode_data/1`, `decode_trailer/1`) is returned as a continuation and invoked as `Module:Function([NewData | Args])` on partial data — a streaming/callback protocol, not a plain function API.
  - Enforces `MaxBodySize`/`MaxHeaderSize`, throwing `body_too_long`, `header_too_long`, `chunk_size`.
- `http_request` — request **header** parsing/building (NOT the request line: the method/URI/version line is parsed in `http_client`/`http_server`).
  - `headers(HeaderList, #http_request_h{})` → updated record, or `{error, whitespace_before_colon}` (RFC 7230 §3.2.4).
  - `http_headers(#http_request_h{})` → flattened header string.
  - `key_value/1` (split one `"Key: Value"`), `is_absolut_uri/1`, `normalize_host/3` (omits the port for scheme well-known ports 80/443).
- `http_response` — response **header** parsing/building (NOT the status line, which is handled by `http_client`/`http_server`).
  - `headers(RevLines, #http_response_h{})` → record; input is a reversed list of lines and obs-fold continuation lines are unfolded.
  - `header_list(#http_response_h{})` → `[{Key, Value}]` for the application programmer (records are not exposed as user API).
- `http_transport` — transport abstraction over `gen_tcp` (ip_comm) and `ssl`, keyed by `SocketType`.
  - `SocketType :: ip_comm | {ip_comm, SockOpts} | {ssl, SslConfig}` is the first argument of every function; it selects the concrete stack.
  - Core: `start/1`, `connect/3,4`, `listen/4,5`, `accept/2,3`, `close/2`, `close_tag/1`, `send/3`, `setopts/3`, `getopts/2,3`, `getstat/2`, `controlling_process/3`, `peername/2`, `sockname/2`, `resolve/0`, `negotiate/3`.
  - Default socket opts applied on connect/listen: `binary, {packet, 0}, {active, false}` (plus `{reuseaddr, true}` and backlog 128 on listen).
  - `start({ssl,_})` ensures `crypto`, `asn1`, `public_key`, `ssl` are started; `start/1` for `ip_comm` is a no-op.
- `http_util` — miscellaneous shared helpers: `to_upper/1`, `to_lower/1`, `hexlist_to_integer/1`, `integer_to_hexlist/1`, `convert_netscapecookie_date/1`, `convert_month/1`, `convert_day/1`, `is_hostname/1`, `timestamp/0`, `timeout/2`, `html_encode/1`, `maybe_add_brackets/2`, `connection_tokens/1`.
- `http_uri` — **deprecated** since OTP 23; only `encode/1` (→ `uri_string:quote/1`) and `decode/1` (→ `uri_string:unquote/1`) remain.
  - `parse/1,2`, `scheme_defaults/0`, `uri/0`, and the URI types are marked `-removed` (removal target since OTP 25); the module also carries an `-unsafe` attribute.
  - Replacement for all of it is the stdlib `uri_string` module; do not add new uses.

## `http_internal.hrl` — the shared contract header
- Private contract header for the inets HTTP stack; includes `inets_internal.hrl` (transitively providing `?CR`, `?LF`, `?CRLF`, `?SP` and other inets-wide macros).
- Defines the tuning limits: `?HTTP_MAX_BODY_SIZE` (nolimit), `?HTTP_MAX_HEADER_SIZE` (10240), `?HTTP_MAX_URI_SIZE` (nolimit), `?HTTP_MAX_VERSION_STRING` (8), `?HTTP_MAX_METHOD_STRING` (20), `?HTTP_MAX_CLIENTS` (150), `?DATA_20MB`, `?HTTP_MAX_CONTENT_LENGTH` (100 MB), `?HTTP_REQUEST_READ_TIMEOUT` (60 s).
- Defines the records `#http_request_h{}` and `#http_response_h{}` (one field per known header + an `other` proplist for unknown headers) and the types `http_request_h()` / `http_response_h()`.
- Included by both client (`httpc`, `httpc_manager`, `httpc_request`, `httpc_response`, `httpc_handler`) and server (`httpd_conf`, `httpd_response`, `httpd_manager`, `httpd_request`, `httpd_request_handler`, `mod_cgi`) code, plus inets test suites — so any field rename/removal here is a cross-directory breaking change.
- Client code uses `-include_lib("inets/src/http_lib/http_internal.hrl")`; server/test code uses relative `-include(...)` paths.

## Constraints
- New modules must be added to `MODULES` in `Makefile` (compiles to `../../ebin/`) and to the modules list in `../inets_app/inets.app.src`.
- Header records in `http_internal.hrl` are a shared contract; changing them affects `../http_client/`, `../http_server/` and `../../test/` simultaneously.
- Keep the `{ssl, Config}` / `ip_comm` dispatch in `http_transport` as the single place that knows about `gen_tcp` vs `ssl`; callers must not call `gen_tcp`/`ssl` directly.
- `http_uri` is frozen/deprecated: do not add functionality, and prefer `uri_string` in new code.

## Design Decisions
- One shared directory (rather than duplicating in client/server) because request/response header handling and chunked coding are byte-for-byte symmetric between the two sides.
- `http_transport` takes `SocketType` as an explicit argument instead of a behaviour/parameterized module, so a single socket value works with both stacks and the caller decides once.
- Unknown headers are preserved in the `other` proplist (`record_info(fields, ...) -- [other]`) so no header is silently dropped when round-tripping.
- `http_response:headers/2` takes a *reversed* line list so obs-fold continuation lines can be joined (`unfold/1`) while streaming in.

## Known Issues
- `http_chunk:decode/3` can return data belonging to a following pipelined request/response as part of `Body`; callers must split it using the returned `content-length` (see the note in `decode/3`'s source comment).
- `http_transport:getstat/2` returns `[]` for `{ssl, _}` sockets — socket statistics are only available for `ip_comm`.
- `http_transport:negotiate/3` is a no-op for `ip_comm` but performs the TLS handshake (`ssl:handshake/2`) for `{ssl, _}`; it must be called after `accept` for SSL sockets.
- `http_util:timestamp/0` uses `os:timestamp/0`, which is wall-clock and deprecated in favour of `erlang:monotonic_time/0`.

## Dependencies
- `kernel`/`stdlib` only: `uri_string`, `string`, `inet`, `inet_parse`, `gen_tcp`, `sets`.
- `ssl`, `crypto`, `asn1`, `public_key` are required only when a `{ssl, _}` socket type is used (started on demand by `http_transport:start/1`).

## Test Strategy
- `../../test/http_format_SUITE.erl` unit-tests this directory directly: `http_chunk` encode/decode (incl. error and partial-data cases), `http_request:http_headers/1` and `is_absolut_uri/1`, `http_util:convert_netscapecookie_date/1`.
- `../../test/httpd_basic_SUITE.erl` covers `http_util:html_encode/1` and `http_transport:listen/4`; integration coverage of the client/server use of these modules lives in the `../http_client/` and `../http_server/` suites.
- Test suites consume `http_internal.hrl` (copied/listed in `../../test/Makefile` `HRL_FILES`, ~line 151); run via `make test` in `lib/inets` with the inets release built.

## Routing Table (module → file)
| Module | File | Role |
| --- | --- | --- |
| `http_chunk` | `http_chunk.erl` | Chunked transfer coding encode/decode + continuation callback API |
| `http_request` | `http_request.erl` | Request header parsing/building over `#http_request_h{}` |
| `http_response` | `http_response.erl` | Response header parsing/building over `#http_response_h{}` |
| `http_transport` | `http_transport.erl` | `gen_tcp`/`ssl` transport abstraction keyed by `SocketType` |
| `http_util` | `http_util.erl` | Misc shared helpers (strings, dates, hex, html/url, connection tokens) |
| `http_uri` | `http_uri.erl` | Deprecated URI encode/decode shims over `uri_string` |
| shared contract | `http_internal.hrl` | Header records, sizes/timeout limits (included by client/server/test) |
| build | `Makefile` | `MODULES` list; compiles into `../../ebin/` |

## See Also
- `../http_client/` — `httpc` HTTP client (consumer of `http_transport`, `http_request`, `http_response`, `http_chunk`, `http_util`).
- `../http_server/` — `httpd` HTTP server (consumer of the same modules).
- `../inets_app/` — `inets.app.src` module list and `inets_internal.hrl` (transitively included here).
- `../../test/` — `http_format_SUITE` direct unit tests for these modules.
