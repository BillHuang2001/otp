# tftp — Sources

## Intent
Erlang sources of the OTP `tftp` application: a TFTP client and server (RFC 1350) with the option extensions RFC 2347 (options), RFC 2348 (blksize) and RFC 2349 (timeout/tsize).
`tftp` is a thin public-API/behaviour module; all protocol logic lives in `tftp_engine` and `tftp_lib`.
The only documented unimplemented feature is the "netascii" transfer mode (no CRLF translation is performed — see Known Issues).
App version comes from `../vsn.mk` (`TFTP_VSN`); release-wide mapping is in the repo-root `otp_versions.table`.

## API Surface (modules)
| Module | Role |
| --- | --- |
| `tftp.erl` | Public API + definition of the `tftp` behaviour. Exports `read_file/3`, `write_file/3` (client), `start/1`, `info/1`, `change_config/2` (server) plus undocumented `start/0`, `stop/0`; defines `-callback prepare/6`, `open/6`, `read/1`, `write/2`, `abort/3`. All functions delegate straight to `tftp_engine`. Only the `option/0` type is exported; `peer/0`, `access/0`, `connection_option/0`, `error_code/0` are internal. Documentation (`-moduledoc`/`-doc`) originates here. |
| `tftp_app.erl` | `-behaviour(application)`. `start/2` reads `application:get_env(tftp, services)` (default `[]`) and calls `tftp_sup:start_link/1`; `stop/1` is a no-op. |
| `tftp_sup.erl` | Top supervisor, registered locally as `tftp_sup`, `one_for_one` (10 restarts / 3600 s). Children are workers started via `{tftp, start, [Options]}` with id `{tftpd, Port}` (or `{tftpd, unique_integer}` when no positive port is configured); also exports `start_child/1`, `stop_child/1`, `which_children/0`. |
| `tftp_engine.erl` | The protocol engine (~1459 lines): daemon loop, per-connection server/client processes, option negotiation/verification, block transfer state machine, behaviour-callback dispatch, `sys` callbacks, `info/1` and `change_config/2`. |
| `tftp_lib.erl` | Config parsing and wire codec: `parse_config/1,2` (options → `#config{}`), `encode_msg/1`/`decode_msg/1`, `replace_val_r/3`, `to_lower/1`, `host_to_string/1`, `add_default_callbacks/1`. |
| `tftp_file.erl` | Default `tftp` behaviour callback for regular files; supports the `{root_dir, Dir}` initial-state sandbox. |
| `tftp_binary.erl` | Default `tftp` behaviour callback for in-memory payloads (`LocalFilename = binary` / a binary argument). |
| `tftp_logger.erl` | Definition of the `tftp_logger` behaviour (`error_msg/2`, `warning_msg/2`, `info_msg/2`, all optional) plus the default implementation over `logger`. |
| `tftp.hrl` | Opcode and error-code macros, `#config{}`, `#tftp_msg_{req,data,ack,error,oack}{}`, `#tftp_decode_error{}`, `#callback{}`. Include-only, no code. |
| `tftp.app.src`, `tftp.appup.src` | Application/appup templates; `%VSN%` is substituted by sed in the `Makefile`. Modules and `runtime_dependencies` (`erts-6.0`, `stdlib-5.0`, `kernel-6.0`) are listed in `tftp.app.src`. |
| `Makefile` | Compiles `MODULES` and `BEHAVIOUR_MODULES`; `tftp`/`tftp_logger` beams are built first. `RELSYSDIR = $(RELEASE_PATH)/lib/tftp-$(VSN)`; `docs:` target is intentionally empty (doc build lives in `../doc/`). |

## Design Decisions
- Behaviour-based dispatch instead of hard-coded file access: a callback is selected per request by matching the filename against `{callback, {RegExp, Module, State}}` entries, first match wins.
- `tftp_lib:add_default_callbacks/1` always appends `tftp_file` and `tftp_binary` with `RegExp = ""` (matching everything) at the end, so user callbacks registered earlier take precedence and the defaults are the fallback.
- `tftp_binary` is selected by module identity (`match_callback/2` short-circuits to `tftp_binary` whenever the local filename is `binary` or a binary), not by regexp.
- Processes are hand-rolled `proc_lib` processes with explicit `receive` loops and `sys` callbacks (`system_continue/3`, `system_terminate/4`, `system_code_change/4`) — no `gen_server`.
- `tftp_sup`'s child module list is `[proc_lib, tftp, tftp_engine]` so release handling finds the used modules.
- Request/reply is a monitor + unique-ref protocol (`call/3`, `reply/3`) rather than `gen_server:call`; a `DOWN` or timeout yields `{error, timeout}`.
- Client results travel through the `proc_lib:start_link` acknowledgement: `proc_lib:init_ack`/`proc_lib:init_fail` deliver `{ok, LastCallbackState}` or the error to the caller, so `read_file/3`/`write_file/3` are synchronous.
- All per-connection state is threaded in one `#config{}` record; per-request negotiation starts from a copy of the daemon config merged with the request's options (`parse_config/2`).
- Errors are propagated three ways: `throw({badarg, ...})` for bad config (turned into `{error, ...}` by the entry points), the internal `#error{}` record + `?ERROR` macro for protocol failures, and behaviour callbacks returning `{error, {Code, Text}}`.
- Block numbers wrap modulo 65536 (`rem 65536`); `max_conn`, `max_tsize` and `port_policy` limits are enforced in the engine, not in the callbacks.

## Constraints
- New/modified callback modules must satisfy the `tftp` behaviour contract in `tftp.erl` exactly (`prepare/6`, `open/6`, `read/1`, `write/2`, `abort/3` and their return shapes).
- Documentation is generated from the `-moduledoc`/`-doc` attributes in the sources (ExDoc + `+nowarn_missing_spec_documented`); do not add hand-written module docs elsewhere.
- `%VSN%` in `tftp.app.src` / `tftp.appup.src` is substituted at build time — never hardcode the version.
- Dependencies are limited to `kernel` and `stdlib` (see `runtime_dependencies` in `tftp.app.src`); no new dependencies may be introduced.
- Callbacks must be callable as-is: modules are no longer loaded explicitly (removed for embedded-mode compatibility), and callbacks must signal errors with `throw/1` / return values only — deep returns via `exit/1` are no longer supported.
- Legacy `and`/`or` operators and legacy `catch` must not be reintroduced (removed in tftp 1.3).

## Known Issues
- `read_file/3`/`write_file/3` return protocol/callback failures as `{error, {Where, Code, Text}}` where `Where` is an internal tag (`client_open`, `pre_verify_options`, `post_verify_options`, `verify_server_options`, `read`, `transfer`, `common_loop`, `match`, ...), while bad options return `{error, {badarg, {Key, Val}}}` (or `{error, {badarg, {Key, Val}, Reason}}` for a bad `callback` regexp) — the two error shapes differ.
- netascii is not implemented: the mode string is validated but no CRLF translation happens. On LF platforms `"netascii"` is rejected with `badop` unless `{native_ascii, true}` is forced, in which case the data is transferred untranslated.
- The default `tftp_file` callback has no path restriction, so any file readable/writable by the VM user is exposed; this is a documented security issue (CVE-2026-21620) — the remedy is `{callback, {"", tftp_file, [{root_dir, Dir}]}}` with an absolute existing directory (see `../doc/notes.md`).
- `tftp.hrl` is `-include`d by `tftp.erl` but nothing from it is used there; the macros/records are consumed by `tftp_lib`, `tftp_engine` and the test suite.
- `tftp_file` and `tftp_binary` still carry `upgrade_state/1` clauses that translate pre-1.3 state record shapes, and `tftp_binary:open/6`/`read/1` retain old-format clauses for release upgrade; these are dead code for fresh transfers but must stay for hot upgrades.
- `port_policy = {range, ...}` picks its start port with `rand:seed_s(default)`, which the code itself flags as not cryptographically secure.
- `tftp_sup` shuts down daemon workers with a 3-second timeout (`timer:seconds(3)`), overridable only by compiling with `-Ddebug_shutdown`.

## Notes for Agents
- `tftp_engine.erl` is intentionally monolithic; navigate it by function group: Info/daemon (`daemon_start/daemon_init/daemon_loop`), Server (`server_init`), Client (`client_start/client_init/client_prepare/client_open/do_client_open`), common loop/read/write/ack, Transfer (`transfer/do_transfer/wait_for_msg.../send_msg`), Callback dispatch (`callback/do_callback/callback_apply/match_callback`), Options (`pre_verify_options/post_verify_options/verify_reject/lookup_*`), Debug formatting (`print_debug_info/*`), System upgrade (`system_*`).
- Behaviour callbacks are invoked through `callback_apply/3`, which catches `throw`/`exit`/`error`; an `{'EXIT', ...}` or unexpected return is turned into an abort with code `undef` and the text "Internal error. File handler error.".
- The `tftp_file` initial state may also carry `{native_ascii, boolean()}` besides `{root_dir, Dir}`; `root_dir` must be absolute (validated in `tftp_file:safe_filename/2` via `filelib:safe_relative_path/2`).
- The `-tftpd_<Port> <Fd>` command-line argument is honoured in `tftp_engine:prepare_daemon_udp/1` (prebound socket fd for `setuid_socket_wrap`); it is read through `init:get_argument/1`.

## Dependencies
- `kernel` and `stdlib` only: uses `gen_udp`, `inet`, `file`, `filelib`, `ets`, `proc_lib`, `sys`, `logger`, `re`, `rand`, `calendar`, `io_lib`.

## Test Strategy
- No unit tests live in `src/`; the suites are the sibling `../test/tftp_SUITE.erl` + `tftp_test_lib.erl` (run with `make test` from `../test`).
- The suite asserts the exact error shapes above (e.g. `{error, {client_open, enoent, _}}`, `{error, {badarg, {host, _}}}`), so any change to error tuples breaks it.
- Client-side tracing is enabled by compile flags (`-Dtftp_debug`, `-Dtftp_trace_client`) defined in `../test/Makefile`.

## See Also
- `../CONTEXT.md` — the `tftp` application root (versions, doc layout).
- `../doc/` — ExDoc config (`docs.exs`) and `notes.md` release notes / security notes.
- `../test/` — sibling Common Test suite (read-only; escalate changes to the parent).
- `../vsn.mk` — `TFTP_VSN`.
- `../../CONTEXT.md` — OTP library-application conventions.
